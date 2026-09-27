use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{App, GIT_REMOTE_STATUS_REFRESH_INTERVAL, GIT_REPO_DISCOVERY_REFRESH_INTERVAL};
use crate::events::{AppEvent, ExternalVcsRefreshResult};
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

    pub(crate) fn start_external_vcs_refresh_if_due(&mut self, now: Instant) {
        if self.vcs_registry.providers().is_empty()
            || self.external_vcs_refresh_in_flight.is_some()
            || now.saturating_duration_since(self.last_external_vcs_refresh)
                < GIT_REMOTE_STATUS_REFRESH_INTERVAL
        {
            return;
        }
        let demand = self.external_vcs_status_demand();
        let refresh_identity = self.external_vcs_identity_refresh_requested
            || now.saturating_duration_since(self.last_external_vcs_discovery_refresh)
                >= GIT_REPO_DISCOVERY_REFRESH_INTERVAL;
        if !refresh_identity && !demand {
            return;
        }

        let mut targets = Vec::new();
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
            }
            if !refresh_identity {
                if let Some(state) = workspace.cached_external_vcs.as_ref() {
                    targets.push(Target {
                        workspace_id: workspace.id.clone(),
                        cwd,
                        root: state.repository_root.clone(),
                        provider_id: state.provider_id.clone(),
                    });
                }
                continue;
            }
            let Ok(Some(discovered)) = self.vcs_registry.discover(&cwd) else {
                continue;
            };
            // Preserve Git at the same or a deeper repository root. An external
            // repository nested inside Git may still become the selected VCS.
            if crate::workspace::git_space_metadata(&cwd)
                .is_some_and(|git| path_depth(&git.repo_root) >= path_depth(&discovered.root))
            {
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
        if had_discovered_targets && targets.is_empty() {
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
            let (results, activations, failures) = refresh(registry, cached, targets, demand).await;
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
    Vec<String>,
) {
    let mut grouped: BTreeMap<String, Vec<Target>> = BTreeMap::new();
    for target in targets {
        grouped
            .entry(target.provider_id.clone())
            .or_default()
            .push(target);
    }
    let mut output = Vec::new();
    let mut activated_updates = Vec::new();
    let mut failures = Vec::new();
    for (provider_id, targets) in grouped {
        let activated = if let Some(provider) = cached.remove(&provider_id) {
            provider
        } else {
            let Some(provider) = registry.provider(&provider_id) else {
                continue;
            };
            let Ok(provider) = provider.activate().await else {
                failures.push(provider_id);
                continue;
            };
            activated_updates.push(provider.clone());
            provider
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
                if let Ok(items) = activated.inspect(request).await {
                    for item in items {
                        if let Ok(root) = item.root.to_path_buf() {
                            if chunk.contains(&root) && !status.contains_key(&root) {
                                status.insert(root, (item.branch, item.ahead, item.behind));
                            }
                        }
                    }
                } else {
                    failures.push(provider_id.clone());
                    break;
                }
            }
        }
        let capabilities = activated.capability_names();
        let checkout_directory = activated.checkout_directory().map(Path::to_path_buf);
        for target in targets {
            let (branch, ahead, behind) = status.remove(&target.root).unwrap_or((None, None, None));
            output.push(ExternalVcsRefreshResult {
                workspace_id: target.workspace_id,
                resolved_identity_cwd: target.cwd,
                state: Some(ExternalVcsState {
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
            });
        }
    }
    (output, activated_updates, failures)
}

fn path_depth(path: &Path) -> usize {
    path.components().count()
}
