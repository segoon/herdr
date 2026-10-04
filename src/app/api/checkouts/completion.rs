use crate::api::schema::{CheckoutInfo, ResponseResult, WorkspaceCloseParams};
use crate::events::{
    ExternalCheckoutCompletion, ExternalCheckoutOutcome, ExternalCheckoutRemovePrepared,
    ExternalCheckoutResult, ExternalCheckoutSource,
};

use super::super::responses::{encode_error, encode_success};
use super::operations::{checkout_source_info, execute_checkout_remove};
use crate::app::vcs_workspace::CheckoutWorkspaceCandidate;
use crate::app::App;

impl App {
    pub(crate) fn handle_external_checkout_remove_prepared(
        &mut self,
        prepared: ExternalCheckoutRemovePrepared,
    ) {
        let ExternalCheckoutRemovePrepared {
            context,
            provider,
            operation,
        } = prepared;
        let operation_matches = self.checkout_requests.matches_remove(
            operation.operation_id,
            &operation.workspace_id,
            &operation.checkout_key,
        );
        if !operation_matches {
            let _ = context.respond_to.send(encode_error(
                context.id,
                "checkout_operation_superseded",
                "checkout operation is no longer current",
            ));
            return;
        }

        let rejection = if context.registry_generation != self.vcs_registry.generation() {
            Some((
                "vcs_configuration_changed",
                "VCS configuration changed before checkout removal started; retry the request",
            ))
        } else {
            let workspace_matches = self.state.workspaces.iter().any(|workspace| {
                workspace.id == operation.workspace_id
                    && workspace.checkout_space.as_ref() == Some(&operation.membership)
            });
            (!workspace_matches).then_some((
                "checkout_changed",
                "checkout workspace changed before removal started; retry the request",
            ))
        };
        if let Some((code, message)) = rejection {
            self.checkout_requests.finish_remove(
                operation.operation_id,
                &operation.workspace_id,
                &operation.checkout_key,
            );
            let _ = context
                .respond_to
                .send(encode_error(context.id, code, message));
            return;
        }

        let Some(ws_idx) = self
            .state
            .workspaces
            .iter()
            .position(|workspace| workspace.id == operation.workspace_id)
        else {
            return;
        };
        let shutdown_panes = self.shutdown_workspace_terminal_runtimes_for_checkout_remove(ws_idx);
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            let result = execute_checkout_remove(&provider, &context.source, &operation).await;
            let _ = event_tx
                .send(context.finished(ExternalCheckoutCompletion::Remove {
                    operation,
                    shutdown_panes,
                    result,
                }))
                .await;
        });
    }

    pub(crate) fn handle_external_checkout_finished(&mut self, result: ExternalCheckoutResult) {
        let ExternalCheckoutResult {
            context,
            completion,
        } = result;
        let response = match completion {
            ExternalCheckoutCompletion::ReadOrCreate { creation, result } => {
                if creation.is_none()
                    && context.registry_generation != self.vcs_registry.generation()
                {
                    encode_error(context.id, "vcs_configuration_changed", "VCS configuration changed while the request was running; retry the request")
                } else if creation.as_ref().is_some_and(|creation| {
                    !self
                        .checkout_requests
                        .matches_create(creation.operation_id, &creation.checkout_key)
                }) {
                    encode_error(
                        context.id,
                        "checkout_operation_superseded",
                        "checkout operation is no longer current",
                    )
                } else {
                    let outcome_unknown = result
                        .as_ref()
                        .is_err_and(|(code, _)| code == "checkout_outcome_unknown");
                    if let Some(creation) = creation.filter(|_| !outcome_unknown) {
                        self.checkout_requests
                            .finish_create(creation.operation_id, &creation.checkout_key);
                    }
                    match result {
                        Err((code, message)) => encode_error(context.id, &code, message),
                        Ok(ExternalCheckoutOutcome::Listed(checkouts)) => encode_success(
                            context.id,
                            ResponseResult::CheckoutList {
                                source: checkout_source_info(&context.source),
                                checkouts: checkouts
                                    .into_iter()
                                    .map(|checkout| self.checkout_info(checkout))
                                    .collect(),
                            },
                        ),
                        Ok(ExternalCheckoutOutcome::Opened {
                            checkout,
                            label,
                            focus,
                        }) => self.finish_external_checkout_open(
                            context.id,
                            context.source,
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
                            context.id,
                            context.source,
                            checkout,
                            label,
                            focus,
                            true,
                        ),
                    }
                }
            }
            ExternalCheckoutCompletion::Remove {
                operation,
                shutdown_panes,
                result,
            } => {
                if !self.checkout_requests.matches_remove(
                    operation.operation_id,
                    &operation.workspace_id,
                    &operation.checkout_key,
                ) {
                    encode_error(
                        context.id,
                        "checkout_operation_superseded",
                        "checkout operation is no longer current",
                    )
                } else {
                    let outcome_unknown = result
                        .as_ref()
                        .is_err_and(|(code, _)| code == "checkout_outcome_unknown");
                    // Uncertain mutations keep their reservation until server restart.
                    if !outcome_unknown {
                        self.checkout_requests.finish_remove(
                            operation.operation_id,
                            &operation.workspace_id,
                            &operation.checkout_key,
                        );
                    }
                    let workspace_matches = self.state.workspaces.iter().any(|workspace| {
                        workspace.id == operation.workspace_id
                            && workspace.checkout_space.as_ref().is_some_and(|membership| {
                                membership.provider_id == context.source.provider_id
                                    && crate::worktree::canonical_or_original(
                                        &membership.checkout_path,
                                    ) == operation.checkout_key
                            })
                    });
                    if result.is_ok() && workspace_matches {
                        let _ = self.handle_workspace_close(
                            String::new(),
                            WorkspaceCloseParams {
                                workspace_id: operation.workspace_id.clone(),
                                close_group: false,
                            },
                        );
                    }
                    if !outcome_unknown {
                        let _ = self.restore_shutdown_checkout_panes(
                            &shutdown_panes,
                            operation.operation_id,
                            &operation.membership.checkout_path,
                            crate::app::checkout_runtime::CheckoutBackendKind::External,
                        );
                    }
                    match result {
                        Err((code, message)) => encode_error(context.id, &code, message),
                        Ok(()) => encode_success(
                            context.id,
                            ResponseResult::CheckoutRemoved {
                                workspace_id: operation.workspace_id,
                                path: operation.membership.checkout_path.display().to_string(),
                                forced: operation.force,
                            },
                        ),
                    }
                }
            }
        };
        let _ = context.respond_to.send(response);
    }

    pub(super) fn finish_external_checkout_open(
        &mut self,
        id: String,
        source: ExternalCheckoutSource,
        checkout: crate::vcs::Checkout,
        label: Option<String>,
        focus: bool,
        created: bool,
    ) -> String {
        let path = checkout.path.clone();
        let already = self.open_workspace_idx_for_external_checkout(&path);
        let candidate = already.map(|index| CheckoutWorkspaceCandidate {
            index,
            created: false,
        });
        let opened = match self.open_or_create_checkout_workspace(&path, candidate, focus) {
            Ok(opened) => opened,
            Err(error) => return encode_error(id, "checkout_open_failed", error),
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
                                .map(|checkout| checkout.path.clone())
                                .unwrap_or_else(|| source.repository_root.clone()),
                            managed: source_checkout.is_some_and(|checkout| checkout.managed),
                            source_workspace_id: None,
                        });
                }
            }
        }
        self.state.workspaces[opened.index].checkout_space =
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
            self.state.workspaces[opened.index].set_custom_name(label);
        }
        self.finalize_checkout_workspace_open(opened);
        self.external_vcs_identity_refresh_requested = true;
        self.mark_external_vcs_refresh_due(std::time::Instant::now());
        let info = self.checkout_info(checkout);
        let records = self.checkout_workspace_records(opened.index);
        let payload = if created {
            ResponseResult::CheckoutCreated {
                workspace: records.workspace,
                tab: records.tab,
                root_pane: records.root_pane,
                checkout: info,
            }
        } else {
            ResponseResult::CheckoutOpened {
                workspace: records.workspace,
                tab: records.tab,
                root_pane: records.root_pane,
                checkout: info,
                already_open: already.is_some(),
            }
        };
        encode_success(id, payload)
    }

    fn checkout_info(&self, checkout: crate::vcs::Checkout) -> CheckoutInfo {
        let path = checkout.path;
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
        CheckoutInfo {
            id: checkout.id,
            name: checkout.name,
            path: path.display().to_string(),
            managed: checkout.managed,
            open_workspace_id,
        }
    }
}
