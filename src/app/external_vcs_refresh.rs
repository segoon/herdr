use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{App, GIT_REMOTE_STATUS_REFRESH_INTERVAL, GIT_REPO_DISCOVERY_REFRESH_INTERVAL};
use crate::events::{
    AppEvent, ExternalVcsFailure, ExternalVcsFailureStage, ExternalVcsObservation,
    ExternalVcsRefreshResult,
};
use crate::vcs::{ActivatedProvider, Capability, ExactPath};
use crate::workspace::ExternalVcsState;

#[derive(Clone)]
struct Target {
    workspace_id: String,
    cwd: PathBuf,
    root: PathBuf,
    provider_id: String,
}

impl App {
    pub(crate) fn mark_external_vcs_refresh_due(&mut self, now: Instant) {
        self.last_external_vcs_refresh = now
            .checked_sub(GIT_REMOTE_STATUS_REFRESH_INTERVAL)
            .unwrap_or(now);
    }

    pub(crate) fn start_external_vcs_refresh_if_due(
        &mut self,
        now: Instant,
        client_status_interest: bool,
    ) {
        if self.vcs_registry.providers().is_empty()
            || self.external_vcs_refresh_in_flight.is_some()
            || now.saturating_duration_since(self.last_external_vcs_refresh)
                < GIT_REMOTE_STATUS_REFRESH_INTERVAL
        {
            return;
        }
        let demand = self.external_vcs_status_demand() || client_status_interest;
        let refresh_identity = self.external_vcs_identity_refresh_requested
            || now.saturating_duration_since(self.last_external_vcs_discovery_refresh)
                >= GIT_REPO_DISCOVERY_REFRESH_INTERVAL;
        if !refresh_identity && !demand {
            return;
        }

        let mut targets = Vec::new();
        let mut observations = Vec::new();
        for workspace in &self.state.workspaces {
            let cwd = workspace
                .resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes);
            let Some(cwd) = cwd else { continue };
            if let Some(membership) = workspace.checkout_space.as_ref() {
                if self
                    .vcs_registry
                    .provider(&membership.provider_id)
                    .is_some()
                {
                    targets.push(Target {
                        workspace_id: workspace.id.clone(),
                        cwd,
                        root: membership.repository_root.clone(),
                        provider_id: membership.provider_id.clone(),
                    });
                    continue;
                }
                observations.push(ExternalVcsRefreshResult {
                    workspace_id: workspace.id.clone(),
                    resolved_identity_cwd: cwd,
                    observation: ExternalVcsObservation::Absent,
                });
                continue;
            }
            if !refresh_identity {
                if let Some(state) = workspace.cached_external_vcs.as_ref() {
                    targets.push(Target {
                        workspace_id: workspace.id.clone(),
                        cwd,
                        root: state.repository_root.clone(),
                        provider_id: state.provider_id.clone(),
                    });
                } else {
                    observations.push(ExternalVcsRefreshResult {
                        workspace_id: workspace.id.clone(),
                        resolved_identity_cwd: cwd,
                        observation: ExternalVcsObservation::NotExamined,
                    });
                }
                continue;
            }
            let discovered = match self.vcs_registry.discover(&cwd) {
                Ok(Some(discovered)) => discovered,
                Ok(None) => {
                    observations.push(ExternalVcsRefreshResult {
                        workspace_id: workspace.id.clone(),
                        resolved_identity_cwd: cwd,
                        observation: ExternalVcsObservation::Absent,
                    });
                    continue;
                }
                Err(_) => {
                    observations.push(ExternalVcsRefreshResult {
                        workspace_id: workspace.id.clone(),
                        resolved_identity_cwd: cwd,
                        observation: ExternalVcsObservation::NotExamined,
                    });
                    continue;
                }
            };
            // Preserve Git at the same or a deeper repository root. An external
            // repository nested inside Git may still become the selected VCS.
            if crate::workspace::git_space_metadata(&cwd)
                .is_some_and(|git| path_depth(&git.repo_root) >= path_depth(&discovered.root))
            {
                observations.push(ExternalVcsRefreshResult {
                    workspace_id: workspace.id.clone(),
                    resolved_identity_cwd: cwd,
                    observation: ExternalVcsObservation::Absent,
                });
                continue;
            }
            targets.push(Target {
                workspace_id: workspace.id.clone(),
                cwd,
                root: discovered.root,
                provider_id: discovered.provider.id().to_owned(),
            });
        }

        let generation = self.vcs_registry.generation();
        let registry = self.vcs_registry.clone();
        let cached = self.activated_vcs_providers.clone();
        let had_discovered_targets = !targets.is_empty();
        targets.retain(|target| {
            self.external_vcs_retry_after
                .get(&target.provider_id)
                .is_none_or(|deadline| now >= *deadline)
        });
        if had_discovered_targets && targets.is_empty() && observations.is_empty() {
            self.last_external_vcs_refresh = now;
            return;
        }
        let task_id = self.next_external_vcs_refresh_id;
        self.next_external_vcs_refresh_id = task_id.saturating_add(1);
        self.external_vcs_refresh_in_flight = Some(task_id);
        self.external_vcs_identity_refresh_requested = false;
        if refresh_identity {
            self.last_external_vcs_discovery_refresh = now;
        }
        self.last_external_vcs_refresh = now;
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            let (mut results, activations, failures) =
                refresh(registry, cached, targets, demand).await;
            results.extend(observations);
            results.sort_by(|left, right| left.workspace_id.cmp(&right.workspace_id));
            let _ = event_tx
                .send(AppEvent::ExternalVcsRefreshed {
                    generation,
                    task_id,
                    results,
                    activations,
                    failures,
                })
                .await;
        });
    }

    fn external_vcs_status_demand(&self) -> bool {
        self.state
            .sidebar_spaces
            .rows
            .iter()
            .flatten()
            .any(|token| {
                matches!(
                    token.parts().0,
                    crate::config::SpaceSidebarToken::Branch
                        | crate::config::SpaceSidebarToken::GitStatus
                )
            })
    }
}

async fn refresh(
    registry: crate::vcs::Registry,
    mut cached: HashMap<String, ActivatedProvider>,
    targets: Vec<Target>,
    inspect: bool,
) -> (
    Vec<ExternalVcsRefreshResult>,
    Vec<ActivatedProvider>,
    Vec<ExternalVcsFailure>,
) {
    let mut grouped: BTreeMap<String, Vec<Target>> = BTreeMap::new();
    for target in targets {
        grouped
            .entry(target.provider_id.clone())
            .or_default()
            .push(target);
    }
    let mut groups = tokio::task::JoinSet::new();
    for (provider_id, targets) in grouped {
        let cached_provider = cached.remove(&provider_id);
        let configured_provider = registry.provider(&provider_id).cloned();
        groups.spawn(async move {
            refresh_provider_group(
                provider_id,
                targets,
                cached_provider,
                configured_provider,
                inspect,
            )
            .await
        });
    }
    let mut output = Vec::new();
    let mut activated_updates = Vec::new();
    let mut failures = Vec::new();
    while let Some(group) = groups.join_next().await {
        let Ok((mut results, activation, failure)) = group else {
            tracing::error!("external VCS refresh task terminated unexpectedly");
            continue;
        };
        output.append(&mut results);
        if let Some(activation) = activation {
            activated_updates.push(activation);
        }
        if let Some(failure) = failure {
            failures.push(failure);
        }
    }
    output.sort_by(|left, right| left.workspace_id.cmp(&right.workspace_id));
    activated_updates.sort_by(|left, right| left.id().cmp(right.id()));
    failures.sort_by(|left, right| left.provider_id.cmp(&right.provider_id));
    (output, activated_updates, failures)
}

async fn refresh_provider_group(
    provider_id: String,
    targets: Vec<Target>,
    cached: Option<ActivatedProvider>,
    configured: Option<crate::vcs::ExternalProvider>,
    inspect: bool,
) -> (
    Vec<ExternalVcsRefreshResult>,
    Option<ActivatedProvider>,
    Option<ExternalVcsFailure>,
) {
    let (activated, activation_update) = if let Some(provider) = cached {
        (provider, None)
    } else {
        let Some(provider) = configured else {
            return (
                unavailable_results(targets, &provider_id),
                None,
                Some(ExternalVcsFailure {
                    provider_id,
                    stage: ExternalVcsFailureStage::Activate,
                    retryable: false,
                    message: "provider was removed from the active registry".into(),
                }),
            );
        };
        match provider.activate().await {
            Ok(provider) => (provider.clone(), Some(provider)),
            Err(error) => {
                let failure = ExternalVcsFailure {
                    provider_id: provider_id.clone(),
                    stage: ExternalVcsFailureStage::Activate,
                    retryable: error.retryable(),
                    message: error.to_string(),
                };
                return (
                    unavailable_results(targets, &provider_id),
                    None,
                    Some(failure),
                );
            }
        }
    };
    let mut status = HashMap::new();
    if inspect && activated.supports(Capability::Inspect) {
        let mut roots = targets
            .iter()
            .map(|target| target.root.clone())
            .collect::<Vec<_>>();
        roots.sort();
        roots.dedup();
        for chunk in roots.chunks(256) {
            let request = chunk
                .iter()
                .map(|root| ExactPath::from_path(root))
                .collect();
            let items = match activated.inspect(request).await {
                Ok(items) => items,
                Err(error) => {
                    let failure = ExternalVcsFailure {
                        provider_id: provider_id.clone(),
                        stage: ExternalVcsFailureStage::Inspect,
                        retryable: error.retryable(),
                        message: error.to_string(),
                    };
                    return (
                        unavailable_results(targets, &provider_id),
                        activation_update,
                        Some(failure),
                    );
                }
            };
            for item in items {
                if let Ok(root) = item.root.to_path_buf() {
                    if chunk.contains(&root) && !status.contains_key(&root) {
                        status.insert(root, (item.branch, item.ahead, item.behind));
                    }
                }
            }
        }
    }
    let capabilities = activated.capabilities();
    let checkout_directory = activated.checkout_directory().map(Path::to_path_buf);
    let results = targets
        .into_iter()
        .map(|target| {
            let (branch, ahead, behind) = status.remove(&target.root).unwrap_or((None, None, None));
            ExternalVcsRefreshResult {
                workspace_id: target.workspace_id,
                resolved_identity_cwd: target.cwd,
                observation: ExternalVcsObservation::Present(ExternalVcsState {
                    provider_id: provider_id.clone(),
                    provider_display_name: activated.display_name().to_owned(),
                    repository_key: crate::vcs::repository_key(&provider_id, &target.root),
                    repository_root: target.root,
                    capabilities: capabilities.clone(),
                    checkout_directory: checkout_directory.clone(),
                    branch,
                    ahead,
                    behind,
                }),
            }
        })
        .collect();
    (results, activation_update, None)
}

fn unavailable_results(targets: Vec<Target>, provider_id: &str) -> Vec<ExternalVcsRefreshResult> {
    targets
        .into_iter()
        .map(|target| ExternalVcsRefreshResult {
            workspace_id: target.workspace_id,
            resolved_identity_cwd: target.cwd,
            observation: ExternalVcsObservation::Unavailable {
                provider_id: provider_id.to_owned(),
            },
        })
        .collect()
}

fn path_depth(path: &Path) -> usize {
    path.components().count()
}
