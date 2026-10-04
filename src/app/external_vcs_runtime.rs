use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use super::{GIT_REMOTE_STATUS_REFRESH_INTERVAL, GIT_REPO_DISCOVERY_REFRESH_INTERVAL};
use crate::events::ExternalVcsFailure;
use crate::vcs::{ActivatedProvider, Registry};

/// Provider lifecycle and refresh scheduling, independent of workspace presentation.
pub(crate) struct ExternalVcsRuntime {
    registry: Registry,
    activated: HashMap<String, ActivatedProvider>,
    retry_after: HashMap<String, Instant>,
    in_flight: Option<u64>,
    next_refresh_id: u64,
    identity_requested: bool,
    last_refresh: Instant,
    last_discovery: Instant,
}

impl ExternalVcsRuntime {
    pub(crate) fn new(registry: Registry, now: Instant) -> Self {
        Self {
            registry,
            activated: HashMap::new(),
            retry_after: HashMap::new(),
            in_flight: None,
            next_refresh_id: 1,
            identity_requested: true,
            last_refresh: now
                .checked_sub(GIT_REMOTE_STATUS_REFRESH_INTERVAL)
                .unwrap_or(now),
            last_discovery: now
                .checked_sub(GIT_REPO_DISCOVERY_REFRESH_INTERVAL)
                .unwrap_or(now),
        }
    }

    pub(crate) fn registry(&self) -> &Registry {
        &self.registry
    }

    pub(crate) fn activated(&self) -> &HashMap<String, ActivatedProvider> {
        &self.activated
    }

    pub(crate) fn replace_config(
        &mut self,
        config: &crate::config::VcsConfig,
        now: Instant,
    ) -> Result<(), Vec<String>> {
        let registry = Registry::from_config(config, self.registry.generation().saturating_add(1))?;
        self.registry = registry;
        self.activated.clear();
        self.retry_after.clear();
        self.request_identity_refresh(now);
        self.last_discovery = now
            .checked_sub(GIT_REPO_DISCOVERY_REFRESH_INTERVAL)
            .unwrap_or(now);
        // The old task retains its slot until its completion arrives.
        Ok(())
    }

    pub(crate) fn mark_due(&mut self, now: Instant) {
        self.last_refresh = now
            .checked_sub(GIT_REMOTE_STATUS_REFRESH_INTERVAL)
            .unwrap_or(now);
    }

    pub(crate) fn request_identity_refresh(&mut self, now: Instant) {
        self.identity_requested = true;
        self.mark_due(now);
    }

    pub(crate) fn refresh_due(&self, now: Instant) -> bool {
        !self.registry.providers().is_empty()
            && self.in_flight.is_none()
            && now.saturating_duration_since(self.last_refresh)
                >= GIT_REMOTE_STATUS_REFRESH_INTERVAL
    }

    pub(crate) fn identity_due(&self, now: Instant) -> bool {
        self.identity_requested
            || now.saturating_duration_since(self.last_discovery)
                >= GIT_REPO_DISCOVERY_REFRESH_INTERVAL
    }

    pub(crate) fn retry_ready(&self, provider_id: &str, now: Instant) -> bool {
        self.retry_after
            .get(provider_id)
            .is_none_or(|deadline| now >= *deadline)
    }

    /// A cooldown skip must retain pending discovery work.
    pub(crate) fn defer_refresh(&mut self, now: Instant) {
        self.last_refresh = now;
    }

    pub(crate) fn begin_refresh(&mut self, now: Instant, refresh_identity: bool) -> u64 {
        debug_assert!(self.in_flight.is_none());
        let task_id = self.next_refresh_id;
        self.next_refresh_id = task_id.saturating_add(1);
        self.in_flight = Some(task_id);
        self.identity_requested = false;
        if refresh_identity {
            self.last_discovery = now;
        }
        self.last_refresh = now;
        task_id
    }

    pub(crate) fn accept_completion(
        &mut self,
        generation: u64,
        task_id: u64,
        activations: Vec<ActivatedProvider>,
        failures: Vec<ExternalVcsFailure>,
        now: Instant,
    ) -> Option<HashSet<String>> {
        if self.in_flight != Some(task_id) {
            return None;
        }
        self.in_flight = None;
        if generation != self.registry.generation() {
            return None;
        }
        for provider in activations {
            self.retry_after.remove(provider.id());
            self.activated.insert(provider.id().to_owned(), provider);
        }
        let failed_providers = failures
            .iter()
            .map(|failure| failure.provider_id.clone())
            .collect::<HashSet<_>>();
        for failure in failures {
            tracing::warn!(
                provider = %failure.provider_id,
                stage = ?failure.stage,
                retryable = failure.retryable,
                error = %failure.message,
                "external VCS refresh failed"
            );
        }
        let retry_at = now + Duration::from_secs(30);
        for provider_id in &failed_providers {
            self.retry_after.insert(provider_id.clone(), retry_at);
        }
        Some(failed_providers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> crate::config::VcsConfig {
        crate::config::VcsConfig {
            providers: vec![crate::config::VcsProviderConfig {
                id: "test".into(),
                display_name: "Test".into(),
                command: vec!["test-provider".into()],
                discovery: vec![crate::config::VcsDiscoveryMarkerConfig {
                    path: ".test".into(),
                    kind: crate::config::VcsDiscoveryMarkerKind::File,
                }],
                allow: vec!["inspect".into()],
                ..Default::default()
            }],
        }
    }

    #[test]
    fn reload_and_stale_completions_preserve_the_current_task_slot() {
        let now = Instant::now();
        let config = config();
        let mut runtime = ExternalVcsRuntime::new(Registry::from_config(&config, 1).unwrap(), now);
        let first = runtime.begin_refresh(now, true);
        runtime.replace_config(&config, now).unwrap();
        assert!(!runtime.refresh_due(now));
        assert!(runtime
            .accept_completion(2, first + 1, Vec::new(), Vec::new(), now)
            .is_none());
        assert!(!runtime.refresh_due(now));
        // A matching task from the old generation frees its own slot.
        assert!(runtime
            .accept_completion(1, first, Vec::new(), Vec::new(), now)
            .is_none());
        assert!(runtime.refresh_due(now));
        assert!(runtime.identity_due(now));
        let second = runtime.begin_refresh(now, true);
        assert!(second > first);
        assert!(runtime
            .accept_completion(1, first, Vec::new(), Vec::new(), now)
            .is_none());
        assert!(runtime
            .accept_completion(2, second, Vec::new(), Vec::new(), now)
            .is_some());
    }

    #[test]
    fn cooldown_deferral_retains_identity_work_and_invalid_reload_retains_state() {
        let now = Instant::now();
        let config = config();
        let mut runtime = ExternalVcsRuntime::new(Registry::from_config(&config, 1).unwrap(), now);
        let task = runtime.begin_refresh(now, true);
        runtime
            .accept_completion(
                1,
                task,
                Vec::new(),
                vec![ExternalVcsFailure {
                    provider_id: "test".into(),
                    stage: crate::events::ExternalVcsFailureStage::Inspect,
                    retryable: false,
                    message: "unavailable".into(),
                }],
                now,
            )
            .unwrap();
        assert!(!runtime.retry_ready("test", now));
        assert!(runtime.retry_ready("test", now + Duration::from_secs(30)));
        runtime.request_identity_refresh(now);
        runtime.defer_refresh(now);
        assert!(runtime.identity_due(now));
        assert!(!runtime.refresh_due(now));
        assert!(runtime.refresh_due(now + GIT_REMOTE_STATUS_REFRESH_INTERVAL));
        let mut invalid = config.clone();
        invalid.providers[0].command.clear();
        assert!(runtime.replace_config(&invalid, now).is_err());
        assert_eq!(runtime.registry().generation(), 1);
        assert!(!runtime.retry_ready("test", now));
        runtime.replace_config(&config, now).unwrap();
        assert!(runtime.retry_ready("test", now));
        assert!(runtime.refresh_due(now));
    }
}
