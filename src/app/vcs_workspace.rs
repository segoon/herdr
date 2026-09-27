use std::path::Path;

use super::App;

/// An existing workspace that can receive a VCS checkout membership.
///
/// `created` is true when source resolution created the workspace earlier in
/// the same operation. Keeping that fact explicit prevents the common
/// lifecycle from emitting duplicate workspace-open events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CheckoutWorkspaceCandidate {
    pub(crate) index: usize,
    pub(crate) created: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CheckoutWorkspaceOpen {
    pub(crate) index: usize,
    pub(crate) created: bool,
}

impl App {
    /// Focuses a matching workspace or creates one for a checkout path.
    ///
    /// Backend adapters remain responsible for proving that a candidate
    /// represents their checkout. This helper owns only the provider-neutral
    /// Herdr workspace transition.
    pub(crate) fn open_or_create_checkout_workspace(
        &mut self,
        path: &Path,
        candidate: Option<CheckoutWorkspaceCandidate>,
        focus: bool,
    ) -> Result<CheckoutWorkspaceOpen, String> {
        if let Some(candidate) = candidate {
            if self.state.workspaces.get(candidate.index).is_none() {
                return Err("checkout workspace changed while the operation was running".into());
            }
            if focus {
                self.state.switch_workspace(candidate.index);
            }
            return Ok(CheckoutWorkspaceOpen {
                index: candidate.index,
                created: candidate.created,
            });
        }

        self.create_workspace_with_options(path.to_path_buf(), focus)
            .map(|index| CheckoutWorkspaceOpen {
                index,
                created: true,
            })
            .map_err(|error| error.to_string())
    }

    /// Applies provider-neutral state changes after the backend membership has
    /// been attached to the selected workspace.
    pub(crate) fn finalize_checkout_workspace_open(&mut self, opened: CheckoutWorkspaceOpen) {
        self.state.mark_session_dirty();
        if opened.created {
            self.emit_workspace_open_events(opened.index);
        }
    }

    pub(crate) fn open_workspace_idx_for_external_checkout(&self, path: &Path) -> Option<usize> {
        let canonical = crate::worktree::canonical_or_original(path);
        self.state
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
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AppState;
    use crate::workspace::Workspace;

    #[test]
    fn reuses_and_focuses_a_checkout_workspace() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state = AppState::test_new();
        app.state.workspaces = vec![Workspace::test_new("one"), Workspace::test_new("two")];
        app.state.ensure_test_terminals();
        app.state.active = Some(0);

        let opened = app
            .open_or_create_checkout_workspace(
                Path::new("/checkout"),
                Some(CheckoutWorkspaceCandidate {
                    index: 1,
                    created: false,
                }),
                true,
            )
            .expect("reuse workspace");

        assert_eq!(opened.index, 1);
        assert!(!opened.created);
        assert_eq!(app.state.active, Some(1));
        assert_eq!(app.state.workspaces.len(), 2);
    }
}
