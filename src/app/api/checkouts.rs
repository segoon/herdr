use crate::api::schema::{
    CheckoutCreateParams, CheckoutInfo, CheckoutListParams, CheckoutOpenParams,
    CheckoutRemoveParams, CheckoutSourceInfo, Method, Request, ResponseResult,
    WorkspaceCloseParams,
};
use crate::events::{
    AppEvent, ExternalCheckoutMutation, ExternalCheckoutOutcome, ExternalCheckoutRemovalRecovery,
    ExternalCheckoutResult, ExternalCheckoutSource,
};
use crate::vcs::ExactPath;

use super::responses::{encode_error, encode_success};
use crate::app::App;

impl App {
    pub(crate) fn handle_deferred_checkout_api_request(
        &mut self,
        request: Request,
        respond_to: std::sync::mpsc::Sender<String>,
    ) -> bool {
        let (workspace_id, cwd) = match &request.method {
            Method::CheckoutList(params) => (params.workspace_id.clone(), params.cwd.clone()),
            Method::CheckoutCreate(params) => (params.workspace_id.clone(), params.cwd.clone()),
            Method::CheckoutOpen(params) => (params.workspace_id.clone(), params.cwd.clone()),
            Method::CheckoutRemove(params) => (Some(params.workspace_id.clone()), None),
            _ => return false,
        };
        let source = match self.resolve_external_checkout_source(workspace_id, cwd) {
            Ok(source) => source,
            Err((code, message)) => {
                let _ = respond_to.send(encode_error(request.id, &code, message));
                return true;
            }
        };
        let Some(provider) = self.vcs_registry.provider(&source.provider_id).cloned() else {
            let _ = respond_to.send(encode_error(
                request.id,
                "vcs_provider_unavailable",
                "the configured VCS provider is no longer available",
            ));
            return true;
        };
        let cached = self
            .activated_vcs_providers
            .get(&source.provider_id)
            .cloned();
        let registry_generation = self.vcs_registry.generation();
        let event_tx = self.event_tx.clone();
        let mut shutdown = None;
        let mut create_destination = None;
        let mut mutation = None;
        if let Method::CheckoutCreate(params) = &request.method {
            let destination = match checkout_create_destination(&provider, params) {
                Ok(destination) => destination,
                Err((code, message)) => {
                    let _ = respond_to.send(encode_error(request.id, &code, message));
                    return true;
                }
            };
            let checkout_key = crate::worktree::canonical_or_original(&destination);
            let Ok(operation_id) = self.checkout_requests.reserve_create(checkout_key.clone())
            else {
                let _ = respond_to.send(encode_error(
                    request.id,
                    "checkout_operation_in_progress",
                    "another checkout operation is already in progress for this path",
                ));
                return true;
            };
            create_destination = Some(destination);
            mutation = Some(ExternalCheckoutMutation::Create {
                operation_id,
                checkout_key,
            });
        }
        if let Method::CheckoutRemove(params) = &request.method {
            let Some(ws_idx) = self.parse_workspace_id(&params.workspace_id) else {
                let _ = respond_to.send(encode_error(
                    request.id,
                    "workspace_not_found",
                    "workspace does not exist",
                ));
                return true;
            };
            let Some(membership) = self.state.workspaces[ws_idx].checkout_space.clone() else {
                let _ = respond_to.send(encode_error(
                    request.id,
                    "checkout_not_managed",
                    "workspace is not an external VCS checkout",
                ));
                return true;
            };
            if !membership.managed {
                let _ = respond_to.send(encode_error(
                    request.id,
                    "checkout_not_managed",
                    "provider did not mark this checkout as managed",
                ));
                return true;
            }
            let checkout_key = crate::worktree::canonical_or_original(&membership.checkout_path);
            let Ok(operation_id) = self
                .checkout_requests
                .reserve_remove(params.workspace_id.clone(), checkout_key.clone())
            else {
                let _ = respond_to.send(encode_error(
                    request.id,
                    "checkout_operation_in_progress",
                    "another checkout operation is already in progress for this checkout",
                ));
                return true;
            };
            let panes = self.shutdown_workspace_terminal_runtimes_for_worktree_remove(ws_idx);
            mutation = Some(ExternalCheckoutMutation::Remove {
                operation_id,
                workspace_id: params.workspace_id.clone(),
                checkout_key,
            });
            shutdown = Some((membership, panes, operation_id, params.force));
        }
        let removal_recovery = shutdown
            .as_ref()
            .map(
                |(membership, panes, operation_id, _)| ExternalCheckoutRemovalRecovery {
                    path: membership.checkout_path.clone(),
                    shutdown_panes: panes.clone(),
                    operation_id: *operation_id,
                },
            );
        tokio::spawn(async move {
            let mut source = source;
            let activated = match cached {
                Some(provider) => Ok(provider),
                None => provider.activate().await,
            };
            let result = match activated {
                Err(error) => Err(("vcs_provider_failed".into(), error.to_string())),
                Ok(activated) => {
                    source.capabilities = activated.capabilities();
                    run_checkout_operation(
                        &activated,
                        &mut source,
                        request.method,
                        shutdown,
                        create_destination,
                    )
                    .await
                }
            };
            let _ = event_tx
                .send(AppEvent::ExternalCheckoutFinished(Box::new(
                    ExternalCheckoutResult {
                        id: request.id,
                        source,
                        registry_generation,
                        mutation,
                        respond_to,
                        result,
                        removal_recovery,
                    },
                )))
                .await;
        });
        true
    }

    fn resolve_external_checkout_source(
        &self,
        workspace_id: Option<String>,
        cwd: Option<String>,
    ) -> Result<ExternalCheckoutSource, (String, String)> {
        if workspace_id.is_some() && cwd.is_some() {
            return Err((
                "invalid_request".into(),
                "only one of workspace_id or cwd may be supplied".into(),
            ));
        }
        let (source_workspace_id, source_cwd) = if let Some(id) = workspace_id {
            let Some(index) = self.parse_workspace_id(&id) else {
                return Err((
                    "workspace_not_found".into(),
                    format!("workspace {id} not found"),
                ));
            };
            let workspace = &self.state.workspaces[index];
            let cwd = workspace
                .resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
                .unwrap_or_else(|| workspace.identity_cwd.clone());
            (Some(workspace.id.clone()), cwd)
        } else if let Some(cwd) = cwd {
            let path = crate::worktree::expand_tilde_path(&cwd);
            if !path.is_absolute() {
                return Err((
                    "invalid_request".into(),
                    "checkout cwd must be absolute".into(),
                ));
            }
            (None, path)
        } else {
            let Some(index) = self.state.active else {
                return Err(("workspace_not_found".into(), "no active workspace".into()));
            };
            let workspace = &self.state.workspaces[index];
            (Some(workspace.id.clone()), workspace.identity_cwd.clone())
        };
        if let Some(workspace_id) = source_workspace_id.as_ref() {
            if let Some(membership) = self
                .state
                .workspaces
                .iter()
                .find(|workspace| &workspace.id == workspace_id)
                .and_then(|workspace| workspace.checkout_space.as_ref())
            {
                return Ok(ExternalCheckoutSource {
                    provider_id: membership.provider_id.clone(),
                    provider_name: membership.provider_display_name.clone(),
                    repository_key: membership.repository_key.clone(),
                    repository_root: membership.repository_root.clone(),
                    source_workspace_id: Some(workspace_id.clone()),
                    source_cwd,
                    capabilities: std::collections::BTreeSet::new(),
                    source_checkout: None,
                });
            }
        }
        let discovered = self
            .vcs_registry
            .discover(&source_cwd)
            .map_err(|message| ("vcs_discovery_failed".into(), message))?
            .ok_or_else(|| {
                (
                    "vcs_not_found".into(),
                    "no configured external VCS repository found".into(),
                )
            })?;
        Ok(ExternalCheckoutSource {
            provider_id: discovered.provider.id().to_owned(),
            provider_name: discovered.provider.display_name().to_owned(),
            repository_key: crate::vcs::repository_key(discovered.provider.id(), &discovered.root),
            repository_root: discovered.root,
            source_workspace_id,
            source_cwd,
            capabilities: std::collections::BTreeSet::new(),
            source_checkout: None,
        })
    }

    pub(crate) fn handle_external_checkout_finished(&mut self, result: ExternalCheckoutResult) {
        let configuration_changed = result.registry_generation != self.vcs_registry.generation();
        if configuration_changed && result.mutation.is_none() {
            let _ = result.respond_to.send(encode_error(
                result.id,
                "vcs_configuration_changed",
                "VCS configuration changed while the request was running; retry the request",
            ));
            return;
        }

        let mutation_matches = result
            .mutation
            .as_ref()
            .is_none_or(|mutation| match mutation {
                ExternalCheckoutMutation::Create {
                    operation_id,
                    checkout_key,
                } => self
                    .checkout_requests
                    .matches_create(*operation_id, checkout_key),
                ExternalCheckoutMutation::Remove {
                    operation_id,
                    workspace_id,
                    checkout_key,
                } => {
                    self.checkout_requests
                        .matches_remove(*operation_id, workspace_id, checkout_key)
                }
            });
        if !mutation_matches {
            let _ = result.respond_to.send(encode_error(
                result.id,
                "checkout_operation_superseded",
                "checkout operation is no longer current",
            ));
            return;
        }

        let outcome_unknown = result
            .result
            .as_ref()
            .is_err_and(|(code, _)| code == "checkout_outcome_unknown");
        let removal_workspace_still_matches = result.mutation.as_ref().is_none_or(|mutation| {
            let ExternalCheckoutMutation::Remove {
                workspace_id,
                checkout_key,
                ..
            } = mutation
            else {
                return true;
            };
            self.state.workspaces.iter().any(|workspace| {
                &workspace.id == workspace_id
                    && workspace.checkout_space.as_ref().is_some_and(|membership| {
                        membership.provider_id == result.source.provider_id
                            && crate::worktree::canonical_or_original(&membership.checkout_path)
                                == *checkout_key
                    })
            })
        });
        if let Some(mutation) = result.mutation.as_ref() {
            match mutation {
                ExternalCheckoutMutation::Create {
                    operation_id,
                    checkout_key,
                } => {
                    if outcome_unknown {
                        self.checkout_requests.mark_outcome_unknown(*operation_id);
                    } else {
                        self.checkout_requests
                            .finish_create(*operation_id, checkout_key);
                    }
                }
                ExternalCheckoutMutation::Remove {
                    operation_id,
                    workspace_id,
                    checkout_key,
                } => {
                    if outcome_unknown {
                        self.checkout_requests.mark_outcome_unknown(*operation_id);
                    } else {
                        self.checkout_requests.finish_remove(
                            *operation_id,
                            workspace_id,
                            checkout_key,
                        );
                    }
                }
            }
        }
        if result.result.is_err() && !outcome_unknown {
            if let Some(recovery) = result.removal_recovery.as_ref() {
                let _ = self.restore_shutdown_worktree_panes(
                    &recovery.shutdown_panes,
                    recovery.operation_id,
                    &recovery.path,
                );
            }
        }
        let response = match result.result {
            Err((code, message)) => encode_error(result.id, &code, message),
            Ok(ExternalCheckoutOutcome::Listed(checkouts)) => encode_success(
                result.id,
                ResponseResult::CheckoutList {
                    source: checkout_source_info(&result.source),
                    checkouts: checkouts
                        .into_iter()
                        .filter_map(|checkout| self.checkout_info(checkout).ok())
                        .collect(),
                },
            ),
            Ok(ExternalCheckoutOutcome::Opened {
                checkout,
                label,
                focus,
            }) => self.finish_external_checkout_open(
                result.id,
                result.source,
                checkout,
                label,
                focus,
                false,
            ),
            Ok(ExternalCheckoutOutcome::Created {
                checkout,
                label,
                focus,
            }) => self.finish_external_checkout_open(
                result.id,
                result.source,
                checkout,
                label,
                focus,
                true,
            ),
            Ok(ExternalCheckoutOutcome::Removed {
                workspace_id,
                path,
                force,
                shutdown_panes,
                operation_id,
            }) => {
                if removal_workspace_still_matches {
                    let _ = self.handle_workspace_close(
                        String::new(),
                        WorkspaceCloseParams {
                            workspace_id: workspace_id.clone(),
                            close_group: false,
                        },
                    );
                }
                let _ = self.restore_shutdown_worktree_panes(&shutdown_panes, operation_id, &path);
                encode_success(
                    result.id,
                    ResponseResult::CheckoutRemoved {
                        workspace_id,
                        path: path.display().to_string(),
                        forced: force,
                    },
                )
            }
        };
        let _ = result.respond_to.send(response);
    }

    fn finish_external_checkout_open(
        &mut self,
        id: String,
        source: ExternalCheckoutSource,
        checkout: crate::vcs::Checkout,
        label: Option<String>,
        focus: bool,
        created: bool,
    ) -> String {
        let Ok(path) = checkout.path.to_path_buf() else {
            return encode_error(
                id,
                "invalid_provider_response",
                "provider returned an invalid checkout path",
            );
        };
        let canonical = crate::worktree::canonical_or_original(&path);
        let already = self
            .state
            .workspaces
            .iter()
            .position(|workspace| {
                workspace.checkout_space.as_ref().is_some_and(|membership| {
                    crate::worktree::canonical_or_original(&membership.checkout_path) == canonical
                })
            })
            .or_else(|| {
                self.state.workspaces.iter().position(|workspace| {
                    workspace
                        .resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
                        .is_some_and(|cwd| {
                            crate::worktree::canonical_or_original(&cwd).starts_with(&canonical)
                        })
                })
            });
        let (ws_idx, created_workspace) = match already {
            Some(index) => {
                if focus {
                    self.state.switch_workspace(index);
                }
                (index, false)
            }
            None => match self.create_workspace_with_options(path.clone(), focus) {
                Ok(index) => (index, true),
                Err(error) => return encode_error(id, "checkout_open_failed", error.to_string()),
            },
        };
        if let Some(source_id) = source.source_workspace_id.as_ref() {
            if let Some(source_idx) = self
                .state
                .workspaces
                .iter()
                .position(|workspace| &workspace.id == source_id)
            {
                if self.state.workspaces[source_idx].checkout_space.is_none() {
                    let source_checkout = source.source_checkout.as_ref();
                    self.state.workspaces[source_idx].checkout_space =
                        Some(crate::workspace::CheckoutSpaceMembership {
                            provider_id: source.provider_id.clone(),
                            provider_display_name: source.provider_name.clone(),
                            repository_key: source.repository_key.clone(),
                            repository_root: source.repository_root.clone(),
                            checkout_id: source_checkout
                                .map(|checkout| checkout.id.clone())
                                .unwrap_or_else(|| "source".into()),
                            checkout_name: source_checkout
                                .map(|checkout| checkout.name.clone())
                                .unwrap_or_else(|| "source".into()),
                            checkout_path: source_checkout
                                .and_then(|checkout| checkout.path.to_path_buf().ok())
                                .unwrap_or_else(|| source.repository_root.clone()),
                            managed: source_checkout.is_some_and(|checkout| checkout.managed),
                            source_workspace_id: None,
                        });
                }
            }
        }
        self.state.workspaces[ws_idx].checkout_space =
            Some(crate::workspace::CheckoutSpaceMembership {
                provider_id: source.provider_id.clone(),
                provider_display_name: source.provider_name.clone(),
                repository_key: source.repository_key.clone(),
                repository_root: source.repository_root.clone(),
                checkout_id: checkout.id.clone(),
                checkout_name: checkout.name.clone(),
                checkout_path: path.clone(),
                managed: checkout.managed,
                source_workspace_id: source.source_workspace_id.clone(),
            });
        if let Some(label) = label {
            self.state.workspaces[ws_idx].set_custom_name(label);
        }
        self.state.mark_session_dirty();
        self.external_vcs_identity_refresh_requested = true;
        self.mark_external_vcs_refresh_due(std::time::Instant::now());
        if created_workspace {
            self.emit_workspace_open_events(ws_idx);
        }
        let tab_idx = self.state.workspaces[ws_idx].active_tab;
        let info = self.checkout_info(checkout).unwrap_or(CheckoutInfo {
            id: String::new(),
            name: path.display().to_string(),
            path: path.display().to_string(),
            managed: false,
            open_workspace_id: Some(self.public_workspace_id(ws_idx)),
        });
        let payload = if created {
            ResponseResult::CheckoutCreated {
                workspace: self.workspace_info(ws_idx),
                tab: self
                    .tab_info(ws_idx, tab_idx)
                    .expect("checkout workspace tab"),
                root_pane: self
                    .root_pane_info(ws_idx, tab_idx)
                    .expect("checkout workspace pane"),
                checkout: info,
            }
        } else {
            ResponseResult::CheckoutOpened {
                workspace: self.workspace_info(ws_idx),
                tab: self
                    .tab_info(ws_idx, tab_idx)
                    .expect("checkout workspace tab"),
                root_pane: self
                    .root_pane_info(ws_idx, tab_idx)
                    .expect("checkout workspace pane"),
                checkout: info,
                already_open: already.is_some(),
            }
        };
        encode_success(id, payload)
    }

    fn checkout_info(&self, checkout: crate::vcs::Checkout) -> Result<CheckoutInfo, String> {
        let path = checkout.path.to_path_buf()?;
        let canonical = crate::worktree::canonical_or_original(&path);
        let open_workspace_id = self
            .state
            .workspaces
            .iter()
            .position(|workspace| {
                workspace.checkout_space.as_ref().is_some_and(|membership| {
                    crate::worktree::canonical_or_original(&membership.checkout_path) == canonical
                })
            })
            .map(|index| self.public_workspace_id(index));
        Ok(CheckoutInfo {
            id: checkout.id,
            name: checkout.name,
            path: path.display().to_string(),
            managed: checkout.managed,
            open_workspace_id,
        })
    }
}

async fn run_checkout_operation(
    provider: &crate::vcs::ActivatedProvider,
    source: &mut ExternalCheckoutSource,
    method: Method,
    shutdown: Option<(
        crate::workspace::CheckoutSpaceMembership,
        Vec<crate::layout::PaneId>,
        u64,
        bool,
    )>,
    create_destination: Option<std::path::PathBuf>,
) -> Result<ExternalCheckoutOutcome, (String, String)> {
    let root = ExactPath::from_path(&source.repository_root);
    let failure = |error: crate::vcs::ProviderFailure| match &error {
        crate::vcs::ProviderFailure::Provider(provider) if provider.code == "checkout_dirty" => (
            "dirty_checkout_requires_force".into(),
            provider.message.clone(),
        ),
        _ => ("vcs_operation_failed".into(), error.to_string()),
    };
    match method {
        Method::CheckoutList(CheckoutListParams { .. }) => {
            let checkouts = provider.checkout_list(root).await.map_err(failure)?;
            capture_source_checkout(source, &checkouts);
            Ok(ExternalCheckoutOutcome::Listed(checkouts))
        }
        Method::CheckoutOpen(CheckoutOpenParams {
            checkout_id,
            label,
            focus,
            ..
        }) => {
            let checkouts = provider.checkout_list(root).await.map_err(failure)?;
            capture_source_checkout(source, &checkouts);
            let checkout = checkouts
                .into_iter()
                .find(|checkout| checkout.id == checkout_id)
                .ok_or_else(|| {
                    (
                        "checkout_not_found".into(),
                        "checkout changed; list and retry".into(),
                    )
                })?;
            Ok(ExternalCheckoutOutcome::Opened {
                checkout,
                label,
                focus,
            })
        }
        Method::CheckoutCreate(CheckoutCreateParams {
            name,
            destination: _,
            label,
            focus,
            ..
        }) => {
            let destination = create_destination.expect("create destination was prepared");
            if provider.supports(crate::vcs::Capability::CheckoutList) {
                if let Ok(checkouts) = provider.checkout_list(root.clone()).await {
                    capture_source_checkout(source, &checkouts);
                }
            }
            let checkout = match provider
                .checkout_create(root.clone(), name, ExactPath::from_path(&destination))
                .await
            {
                Ok(checkout) => checkout,
                Err(error) if matches!(error, crate::vcs::ProviderFailure::Timeout(_)) => {
                    provider
                        .checkout_list(root)
                        .await
                        .map_err(|reconcile_error| {
                            (
                                "checkout_outcome_unknown".into(),
                                format!(
                                    "checkout creation timed out and could not be reconciled: {reconcile_error}"
                                ),
                            )
                        })?
                        .into_iter()
                        .find(|checkout| {
                            checkout
                                .path
                                .to_path_buf()
                                .ok()
                                .is_some_and(|path| same_checkout_path(&path, &destination))
                        })
                        .ok_or_else(|| {
                            (
                                "checkout_outcome_unknown".into(),
                                format!(
                                    "checkout creation timed out and the destination was not reported during reconciliation: {error}"
                                ),
                            )
                        })?
                }
                Err(error) => return Err(failure(error)),
            };
            let returned_path = checkout.path.to_path_buf().map_err(|message| {
                (
                    "checkout_outcome_unknown".into(),
                    format!("provider reported success with an invalid checkout path: {message}"),
                )
            })?;
            if !same_checkout_path(&returned_path, &destination) {
                return Err((
                    "checkout_outcome_unknown".into(),
                    "provider reported success for a checkout path different from the requested destination; the mutation result requires manual reconciliation".into(),
                ));
            }
            Ok(ExternalCheckoutOutcome::Created {
                checkout,
                label,
                focus,
            })
        }
        Method::CheckoutRemove(CheckoutRemoveParams {
            workspace_id,
            force,
        }) => {
            let (membership, shutdown_panes, operation_id, _) =
                shutdown.expect("remove shutdown context");
            let listed = provider
                .checkout_list(root.clone())
                .await
                .map_err(failure)?;
            capture_source_checkout(source, &listed);
            let valid =
                listed.iter().any(|checkout| {
                    checkout.id == membership.checkout_id
                        && checkout.managed
                        && checkout.path.to_path_buf().ok().is_some_and(|path| {
                            same_checkout_path(&path, &membership.checkout_path)
                        })
                });
            if !valid {
                return Err((
                    "checkout_not_managed".into(),
                    "provider no longer reports this checkout as managed".into(),
                ));
            }
            let remove_result = provider
                .checkout_remove(
                    root.clone(),
                    ExactPath::from_path(&membership.checkout_path),
                    force,
                )
                .await;
            if let Err(error) = remove_result {
                if matches!(error, crate::vcs::ProviderFailure::Timeout(_)) {
                    match provider.checkout_list(root).await {
                        Ok(checkouts)
                            if !checkouts
                                .iter()
                                .any(|checkout| checkout.id == membership.checkout_id) => {}
                        Ok(_) => {
                            return Err((
                                "checkout_outcome_unknown".into(),
                                format!(
                                    "checkout removal timed out and the checkout is still reported during reconciliation: {error}"
                                ),
                            ));
                        }
                        Err(reconcile_error) => {
                            return Err((
                                "checkout_outcome_unknown".into(),
                                format!(
                                    "checkout removal timed out and could not be reconciled: {reconcile_error}"
                                ),
                            ));
                        }
                    }
                } else {
                    return Err(failure(error));
                }
            }
            Ok(ExternalCheckoutOutcome::Removed {
                workspace_id,
                path: membership.checkout_path,
                force,
                shutdown_panes,
                operation_id,
            })
        }
        _ => unreachable!("checkout dispatcher received non-checkout method"),
    }
}

fn capture_source_checkout(
    source: &mut ExternalCheckoutSource,
    checkouts: &[crate::vcs::Checkout],
) {
    source.source_checkout = checkouts
        .iter()
        .filter_map(|checkout| {
            let path = crate::worktree::canonical_or_original(&checkout.path.to_path_buf().ok()?);
            let source_cwd = crate::worktree::canonical_or_original(&source.source_cwd);
            source_cwd
                .starts_with(&path)
                .then_some((path.components().count(), checkout.clone()))
        })
        .max_by_key(|(depth, _)| *depth)
        .map(|(_, checkout)| checkout);
}

fn same_checkout_path(left: &std::path::Path, right: &std::path::Path) -> bool {
    crate::worktree::canonical_or_original(left) == crate::worktree::canonical_or_original(right)
}

fn checkout_create_destination(
    provider: &crate::vcs::ExternalProvider,
    params: &CheckoutCreateParams,
) -> Result<std::path::PathBuf, (String, String)> {
    match params.destination.as_deref() {
        Some(path) => {
            let path = crate::worktree::expand_tilde_path(path);
            if !path.is_absolute() {
                return Err((
                    "invalid_request".into(),
                    "checkout destination must be absolute".into(),
                ));
            }
            Ok(path)
        }
        None => provider
            .checkout_directory()
            .map(|directory| directory.join(crate::worktree::branch_to_path_slug(&params.name)))
            .ok_or_else(|| {
                (
                    "checkout_directory_required".into(),
                    "provider has no checkout directory configured".into(),
                )
            }),
    }
}

fn checkout_source_info(source: &ExternalCheckoutSource) -> CheckoutSourceInfo {
    CheckoutSourceInfo {
        provider_id: source.provider_id.clone(),
        provider_name: source.provider_name.clone(),
        repository_key: source.repository_key.clone(),
        repository_root: source.repository_root.display().to_string(),
        capabilities: source
            .capabilities
            .iter()
            .map(|capability| capability.as_str().to_owned())
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::schema::{ErrorResponse, SuccessResponse};
    use crate::config::Config;
    use crate::workspace::{CheckoutSpaceMembership, Workspace};

    fn test_app() -> App {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        App::new(
            &Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        )
    }

    fn source() -> ExternalCheckoutSource {
        ExternalCheckoutSource {
            provider_id: "arc".into(),
            provider_name: "Arc".into(),
            repository_root: "/repo".into(),
            repository_key: "arc:repo".into(),
            source_workspace_id: None,
            source_cwd: "/repo".into(),
            capabilities: std::collections::BTreeSet::new(),
            source_checkout: None,
        }
    }

    #[test]
    fn stale_read_completion_is_rejected_after_config_reload() {
        let mut app = test_app();
        let (respond_to, response_rx) = std::sync::mpsc::channel();

        app.handle_external_checkout_finished(ExternalCheckoutResult {
            id: "request".into(),
            source: source(),
            registry_generation: app.vcs_registry.generation().saturating_sub(1),
            mutation: None,
            respond_to,
            result: Ok(ExternalCheckoutOutcome::Listed(Vec::new())),
            removal_recovery: None,
        });

        let response: ErrorResponse = serde_json::from_str(&response_rx.recv().unwrap()).unwrap();
        assert_eq!(response.error.code, "vcs_configuration_changed");
    }

    #[test]
    fn successful_remove_is_grandfathered_across_config_reload() {
        let mut app = test_app();
        let path = std::path::PathBuf::from("/repo/topic");
        let mut workspace = Workspace::test_new("topic");
        let workspace_id = workspace.id.clone();
        workspace.checkout_space = Some(CheckoutSpaceMembership {
            provider_id: "arc".into(),
            provider_display_name: "Arc".into(),
            repository_key: "arc:repo".into(),
            repository_root: "/repo".into(),
            checkout_id: "topic".into(),
            checkout_name: "topic".into(),
            checkout_path: path.clone(),
            managed: true,
            source_workspace_id: None,
        });
        app.state.workspaces = vec![workspace];
        app.state.ensure_test_terminals();
        let checkout_key = crate::worktree::canonical_or_original(&path);
        let operation_id = app
            .checkout_requests
            .reserve_remove(workspace_id.clone(), checkout_key.clone())
            .unwrap();
        let (respond_to, response_rx) = std::sync::mpsc::channel();

        app.handle_external_checkout_finished(ExternalCheckoutResult {
            id: "request".into(),
            source: source(),
            registry_generation: app.vcs_registry.generation().saturating_sub(1),
            mutation: Some(ExternalCheckoutMutation::Remove {
                operation_id,
                workspace_id: workspace_id.clone(),
                checkout_key,
            }),
            respond_to,
            result: Ok(ExternalCheckoutOutcome::Removed {
                workspace_id,
                path,
                force: false,
                shutdown_panes: Vec::new(),
                operation_id,
            }),
            removal_recovery: None,
        });

        let response: SuccessResponse = serde_json::from_str(&response_rx.recv().unwrap()).unwrap();
        assert!(matches!(
            response.result,
            ResponseResult::CheckoutRemoved { .. }
        ));
        assert!(app.checkout_requests.is_empty());
        assert!(app.state.workspaces.is_empty());
    }

    #[test]
    fn unknown_mutation_outcome_keeps_reservation_quarantined() {
        let mut app = test_app();
        let path = std::path::PathBuf::from("/repo/topic");
        let checkout_key = crate::worktree::canonical_or_original(&path);
        let operation_id = app
            .checkout_requests
            .reserve_create(checkout_key.clone())
            .unwrap();
        let (respond_to, response_rx) = std::sync::mpsc::channel();

        app.handle_external_checkout_finished(ExternalCheckoutResult {
            id: "request".into(),
            source: source(),
            registry_generation: app.vcs_registry.generation(),
            mutation: Some(ExternalCheckoutMutation::Create {
                operation_id,
                checkout_key: checkout_key.clone(),
            }),
            respond_to,
            result: Err(("checkout_outcome_unknown".into(), "timed out".into())),
            removal_recovery: None,
        });

        let response: ErrorResponse = serde_json::from_str(&response_rx.recv().unwrap()).unwrap();
        assert_eq!(response.error.code, "checkout_outcome_unknown");
        assert_eq!(
            app.checkout_requests.state(operation_id),
            Some(super::super::checkout_requests::CheckoutRequestState::OutcomeUnknown)
        );
        assert!(app.checkout_requests.reserve_create(checkout_key).is_err());
    }
}
