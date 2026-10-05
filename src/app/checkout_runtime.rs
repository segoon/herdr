use std::path::Path;
use std::time::Duration;

use crate::events::AppEvent;
use crate::vcs::CheckoutBackend;

use super::App;

impl App {
    fn queue_checkout_runtime_restore_failed(
        &self,
        pane_id: crate::layout::PaneId,
        operation_id: u64,
    ) {
        let event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            let _ = event_tx
                .send(AppEvent::CheckoutRuntimeRestoreFailed {
                    pane_id,
                    operation_id,
                })
                .await;
        });
    }

    pub(crate) fn shutdown_workspace_terminal_runtimes_for_checkout_remove(
        &mut self,
        ws_idx: usize,
    ) -> Vec<crate::layout::PaneId> {
        let mut shutdown_panes = Vec::new();
        for pane_id in self.state.pane_ids_for_workspace(ws_idx) {
            let Some(terminal_id) = self.state.terminal_id_for_pane(ws_idx, pane_id) else {
                continue;
            };
            if self.terminal_runtimes.get(&terminal_id).is_none() {
                continue;
            }
            tracing::debug!(
                workspace_index = ws_idx,
                terminal_id = %terminal_id,
                "shutting down terminal runtime before checkout removal"
            );
            *self
                .pending_checkout_remove_runtime_exits
                .entry(pane_id)
                .or_default() += 1;
            shutdown_panes.push(pane_id);
            self.shutdown_terminal_runtime(terminal_id);
        }
        shutdown_panes
    }

    pub(crate) fn restore_shutdown_checkout_panes(
        &mut self,
        shutdown_panes: &[crate::layout::PaneId],
        operation_id: u64,
        removed_checkout: &Path,
        backend: CheckoutBackend,
    ) -> Vec<crate::app::actions::PaneStateUpdate> {
        let mut pane_updates = Vec::new();
        let removed_checkout = crate::worktree::canonical_or_original(removed_checkout);
        for &pane_id in shutdown_panes {
            let Some((ws_idx, terminal_id)) = self
                .find_pane(pane_id)
                .map(|(ws_idx, pane)| (ws_idx, pane.attached_terminal_id.clone()))
            else {
                self.pending_checkout_remove_runtime_exits.remove(&pane_id);
                self.pending_checkout_remove_runtime_restores
                    .remove(&pane_id);
                continue;
            };
            let runtime_missing = self.terminal_runtimes.get(&terminal_id).is_none();
            if runtime_missing {
                let workspace = &self.state.workspaces[ws_idx];
                let current_checkout = match backend {
                    CheckoutBackend::GitWorktree => {
                        workspace.worktree_space().map(|space| &space.checkout_path)
                    }
                    CheckoutBackend::ExternalVcs => workspace
                        .checkout_space
                        .as_ref()
                        .map(|space| &space.checkout_path),
                }
                .unwrap_or(&workspace.identity_cwd);
                if crate::worktree::canonical_or_original(current_checkout) != removed_checkout {
                    if let Some(terminal) = self.state.terminals.get_mut(&terminal_id) {
                        terminal.cwd = current_checkout.clone();
                    }
                }
                if self
                    .pending_checkout_remove_runtime_exits
                    .contains_key(&pane_id)
                {
                    if self
                        .pending_checkout_remove_runtime_restores
                        .insert(pane_id, operation_id)
                        .is_none()
                    {
                        let event_tx = self.event_tx.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            let _ = event_tx
                                .send(AppEvent::CheckoutRuntimeRestoreFailed {
                                    pane_id,
                                    operation_id,
                                })
                                .await;
                        });
                    }
                } else {
                    pane_updates.extend(self.publish_checkout_runtime_agent_release(pane_id));
                    if !self.respawn_shell_for_launch_pane(pane_id, false) {
                        self.pending_checkout_remove_runtime_restores
                            .insert(pane_id, operation_id);
                        self.queue_checkout_runtime_restore_failed(pane_id, operation_id);
                    }
                }
            }
        }
        pane_updates
    }
}
