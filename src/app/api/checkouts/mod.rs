mod completion;
mod operations;

use crate::api::schema::{Method, Request};
use crate::events::{
    AppEvent, ExternalCheckoutMutation, ExternalCheckoutRemovePrepared, ExternalCheckoutResult,
    ExternalCheckoutSource,
};

use self::operations::{
    checkout_create_destination, preflight_checkout_remove, run_checkout_operation,
};
use super::responses::encode_error;
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
        let mut create_destination = None;
        let mut mutation = None;
        let mut remove_context = None;
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
            mutation = Some(ExternalCheckoutMutation::Remove {
                operation_id,
                workspace_id: params.workspace_id.clone(),
                checkout_key: checkout_key.clone(),
            });
            remove_context = Some((
                membership,
                params.workspace_id.clone(),
                checkout_key,
                operation_id,
                params.force,
            ));
        }
        tokio::spawn(async move {
            let mut source = source;
            let activated = match cached {
                Some(provider) => Ok(provider),
                None => provider.activate().await,
            };
            let activated = match activated {
                Ok(activated) => activated,
                Err(error) => {
                    let _ = event_tx
                        .send(AppEvent::ExternalCheckoutFinished(Box::new(
                            ExternalCheckoutResult {
                                id: request.id,
                                source,
                                registry_generation,
                                mutation,
                                respond_to,
                                result: Err(("vcs_provider_failed".into(), error.to_string())),
                                removal_recovery: None,
                            },
                        )))
                        .await;
                    return;
                }
            };
            source.capabilities = activated.capabilities();
            if let Some((membership, workspace_id, checkout_key, operation_id, force)) =
                remove_context
            {
                match preflight_checkout_remove(&activated, &mut source, &membership).await {
                    Ok(()) => {
                        let _ = event_tx
                            .send(AppEvent::ExternalCheckoutRemovePrepared(Box::new(
                                ExternalCheckoutRemovePrepared {
                                    id: request.id,
                                    source,
                                    registry_generation,
                                    provider: activated,
                                    membership,
                                    workspace_id,
                                    checkout_key,
                                    operation_id,
                                    force,
                                    respond_to,
                                },
                            )))
                            .await;
                    }
                    Err(error) => {
                        let _ = event_tx
                            .send(AppEvent::ExternalCheckoutFinished(Box::new(
                                ExternalCheckoutResult {
                                    id: request.id,
                                    source,
                                    registry_generation,
                                    mutation,
                                    respond_to,
                                    result: Err(error),
                                    removal_recovery: None,
                                },
                            )))
                            .await;
                    }
                }
                return;
            }
            let result =
                run_checkout_operation(&activated, &mut source, request.method, create_destination)
                    .await;
            let _ = event_tx
                .send(AppEvent::ExternalCheckoutFinished(Box::new(
                    ExternalCheckoutResult {
                        id: request.id,
                        source,
                        registry_generation,
                        mutation,
                        respond_to,
                        result,
                        removal_recovery: None,
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
}

#[cfg(test)]
mod tests;
