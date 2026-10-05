use std::path::Path;

use super::App;

/// A workspace selected for a checkout, with its creation provenance.
///
/// `created` records whether this operation created the workspace, including
/// during source resolution, so finalization emits creation events only once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CheckoutWorkspaceSelection {
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
        candidate: Option<CheckoutWorkspaceSelection>,
        focus: bool,
    ) -> Result<CheckoutWorkspaceSelection, String> {
        if let Some(candidate) = candidate {
            if self.state.workspaces.get(candidate.index).is_none() {
                return Err("checkout workspace changed while the operation was running".into());
            }
            if focus {
                self.state.switch_workspace(candidate.index);
            }
            return Ok(candidate);
        }

        self.create_workspace_with_options(path.to_path_buf(), focus)
            .map(|index| CheckoutWorkspaceSelection {
                index,
                created: true,
            })
            .map_err(|error| error.to_string())
    }

    /// Applies provider-neutral state changes after the backend membership has
    /// been attached to the selected workspace.
    pub(crate) fn finalize_checkout_workspace_open(&mut self, opened: CheckoutWorkspaceSelection) {
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
                    if workspace.checkout_space.is_some() || workspace.worktree_space().is_some() {
                        return false;
                    }
                    workspace
                        .resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)
                        .is_some_and(|cwd| {
                            crate::worktree::canonical_or_original(&cwd) == canonical
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
                Some(CheckoutWorkspaceSelection {
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

    #[test]
    fn external_checkout_reuse_does_not_override_explicit_provenance() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state = AppState::test_new();
        let mut workspace = Workspace::test_new("nested");
        workspace.identity_cwd = "/repo/checkout/nested".into();
        workspace.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
            key: "git:repo".into(),
            label: "git".into(),
            repo_root: "/repo/checkout/nested".into(),
            checkout_path: "/repo/checkout/nested".into(),
            is_linked_worktree: true,
        });
        app.state.workspaces = vec![workspace];

        assert_eq!(
            app.open_workspace_idx_for_external_checkout(Path::new("/repo/checkout")),
            None
        );
    }

    #[test]
    fn external_checkout_reuses_only_exact_unowned_workspace_path() {
        let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(
            &crate::config::Config::default(),
            crate::app::AppPolicy::TEST,
            None,
            api_rx,
            crate::api::EventHub::default(),
        );
        app.state = AppState::test_new();
        let mut workspace = Workspace::test_new("checkout");
        workspace.identity_cwd = "/repo/checkout".into();
        app.state.workspaces = vec![workspace];

        assert_eq!(
            app.open_workspace_idx_for_external_checkout(Path::new("/repo/checkout")),
            Some(0)
        );
        assert_eq!(
            app.open_workspace_idx_for_external_checkout(Path::new("/repo")),
            None
        );
    }
}
