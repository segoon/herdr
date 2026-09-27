use super::*;

fn checkout_path_preview(root: &str, repo: &str, branch: &str) -> String {
    // This is an endpoint path, not a path on the machine drawing the dialog.
    let separator = if !root.starts_with('/') && root.contains('\\') {
        '\\'
    } else {
        '/'
    };
    format!(
        "{}{separator}{repo}{separator}{}",
        root.trim_end_matches(separator),
        crate::worktree::branch_to_path_slug(branch)
    )
}

impl ClientShellState {
    pub(super) fn open_external_checkout_create(
        &mut self,
        workspace_id: String,
        outcome: &mut ClientShellInput,
    ) {
        let provider_name = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == self.active_endpoint_id)
            .and_then(|endpoint| Self::endpoint_workspace_vcs(endpoint, &workspace_id))
            .map(|vcs| vcs.provider_display_name.clone())
            .unwrap_or_else(|| "VCS".into());
        self.overlay = Some(ClientShellOverlay::WorktreeCreate(
            ClientWorktreeCreateOverlay {
                external_checkout: true,
                source_workspace_id: workspace_id,
                repo_name: provider_name,
                branch: TextEditor::new("checkout", true),
                checkout_path: String::new(),
                error: None,
                creating: false,
            },
        ));
        outcome.repaint = true;
    }

    pub(super) fn begin_external_checkout_open(
        &mut self,
        workspace_id: String,
        outcome: &mut ClientShellInput,
    ) {
        self.push_endpoint_method_with_kind(
            crate::api::schema::Method::CheckoutList(crate::api::schema::CheckoutListParams {
                workspace_id: Some(workspace_id.clone()),
                cwd: None,
            }),
            PendingEndpointKind::PrepareCheckoutOpen { workspace_id },
            outcome,
        );
    }

    pub(super) fn submit_external_checkout_remove(
        &mut self,
        workspace_id: String,
        outcome: &mut ClientShellInput,
    ) {
        self.overlay = Some(ClientShellOverlay::WorktreeRemove(
            ClientWorktreeRemoveOverlay {
                external_checkout: true,
                workspace_id,
                path: "external checkout".into(),
                error: None,
                removing: false,
                force_confirmation: false,
            },
        ));
        outcome.repaint = true;
    }

    fn endpoint_worktree_directory(&self) -> Option<String> {
        self.snapshot
            .as_deref()
            .map(|snapshot| snapshot.worktree_directory.clone())
    }

    pub(super) fn insert_worktree_overlay_text(&mut self, text: &str) -> bool {
        match self.overlay.as_mut() {
            Some(ClientShellOverlay::WorktreeCreate(create)) if !create.creating => {
                if create.branch.insert(text) {
                    self.sync_worktree_create_path();
                }
                true
            }
            Some(ClientShellOverlay::WorktreeOpen(open))
                if open.search_focused && !open.opening =>
            {
                if open.query.insert(text) {
                    if let Some(first) = open.filtered_indices().first().copied() {
                        open.selected = first;
                    }
                }
                true
            }
            _ => false,
        }
    }

    pub(super) fn route_worktree_overlay_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) -> bool {
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        match self.overlay.as_ref() {
            Some(ClientShellOverlay::WorktreeCreate(_)) => {
                let creating = matches!(
                    self.overlay,
                    Some(ClientShellOverlay::WorktreeCreate(
                        ClientWorktreeCreateOverlay { creating: true, .. }
                    ))
                );
                if !creating {
                    if let Some(ClientShellOverlay::WorktreeCreate(create)) = self.overlay.as_mut()
                    {
                        if let Some(content_changed) = create.branch.handle_key(key) {
                            if content_changed {
                                self.sync_worktree_create_path();
                            }
                            outcome.repaint = true;
                            return true;
                        }
                    }
                }
                match code {
                    KeyCode::Esc if !creating => {
                        self.overlay = None;
                        outcome.repaint = true;
                    }
                    KeyCode::Enter => self.submit_worktree_create(outcome),
                    _ => {}
                }
                true
            }
            Some(ClientShellOverlay::WorktreeOpen(_)) => {
                let opening = matches!(
                    self.overlay,
                    Some(ClientShellOverlay::WorktreeOpen(
                        ClientWorktreeOpenOverlay { opening: true, .. }
                    ))
                );
                let search_focused = matches!(
                    self.overlay,
                    Some(ClientShellOverlay::WorktreeOpen(
                        ClientWorktreeOpenOverlay {
                            search_focused: true,
                            ..
                        }
                    ))
                );
                if !opening && search_focused {
                    if let Some(ClientShellOverlay::WorktreeOpen(open)) = self.overlay.as_mut() {
                        if let Some(content_changed) = open.query.handle_key(key) {
                            if content_changed {
                                if let Some(first) = open.filtered_indices().first().copied() {
                                    open.selected = first;
                                }
                            }
                            outcome.repaint = true;
                            return true;
                        }
                    }
                }
                match code {
                    KeyCode::Esc if !opening => {
                        self.overlay = None;
                        outcome.repaint = true;
                    }
                    KeyCode::Enter => self.submit_worktree_open(outcome),
                    KeyCode::Up if !opening => {
                        self.move_worktree_open_selection(-1);
                        outcome.repaint = true;
                    }
                    KeyCode::Down if !opening => {
                        self.move_worktree_open_selection(1);
                        outcome.repaint = true;
                    }
                    KeyCode::Char('n' | 'p')
                        if !opening && modifiers == crossterm::event::KeyModifiers::CONTROL =>
                    {
                        self.move_worktree_open_selection(if code == KeyCode::Char('n') {
                            1
                        } else {
                            -1
                        });
                        outcome.repaint = true;
                    }
                    KeyCode::Char('/') if !opening && !search_focused => {
                        if let Some(ClientShellOverlay::WorktreeOpen(open)) = self.overlay.as_mut()
                        {
                            open.search_focused = true;
                        }
                        outcome.repaint = true;
                    }
                    _ => {}
                }
                true
            }
            Some(ClientShellOverlay::WorktreeRemove(_)) => {
                let removing = matches!(
                    self.overlay,
                    Some(ClientShellOverlay::WorktreeRemove(
                        ClientWorktreeRemoveOverlay { removing: true, .. }
                    ))
                );
                match code {
                    KeyCode::Esc if !removing => {
                        self.overlay = None;
                        outcome.repaint = true;
                    }
                    KeyCode::Enter => self.submit_worktree_remove(outcome),
                    _ => {}
                }
                true
            }
            _ => false,
        }
    }

    pub(super) fn begin_worktree_action(
        &mut self,
        action: crate::input::KeybindAction,
        outcome: &mut ClientShellInput,
    ) {
        let Some(workspace_id) = self.workspace_action_id() else {
            return;
        };
        self.begin_worktree_action_for(action, workspace_id, outcome);
    }

    pub(super) fn begin_worktree_action_for(
        &mut self,
        action: crate::input::KeybindAction,
        workspace_id: String,
        outcome: &mut ClientShellInput,
    ) {
        use crate::api::schema::{Method, WorktreeListParams};
        use crate::input::KeybindAction;

        let external = self
            .endpoints
            .iter()
            .find(|endpoint| endpoint.endpoint_id == self.active_endpoint_id)
            .and_then(|endpoint| Self::endpoint_workspace_vcs(endpoint, &workspace_id));
        if let Some(vcs) = external {
            let has = |capability: &str| vcs.capabilities.iter().any(|value| value == capability);
            match action {
                KeybindAction::NewWorktree if vcs.can_create_checkout && has("checkout.create") => {
                    self.open_external_checkout_create(workspace_id, outcome);
                }
                KeybindAction::OpenWorktree if has("checkout.list") => {
                    self.begin_external_checkout_open(workspace_id, outcome);
                }
                KeybindAction::RemoveWorktree
                    if has("checkout.remove")
                        && vcs
                            .checkout
                            .as_ref()
                            .is_some_and(|checkout| checkout.managed) =>
                {
                    self.submit_external_checkout_remove(workspace_id, outcome);
                }
                _ => {
                    self.set_endpoint_error(
                        "This checkout action is not supported by the selected VCS provider.",
                    );
                    outcome.repaint = true;
                }
            }
            return;
        }

        let workspace = self.snapshot.as_deref().and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .find(|workspace| workspace.workspace_id == workspace_id)
        });
        let linked = workspace
            .and_then(|workspace| workspace.worktree.as_ref())
            .is_some_and(|worktree| worktree.is_linked_worktree);
        let kind = match action {
            KeybindAction::NewWorktree | KeybindAction::OpenWorktree if linked => {
                self.set_endpoint_error(
                    "New and open worktree actions start from the repo parent workspace.",
                );
                outcome.repaint = true;
                return;
            }
            KeybindAction::NewWorktree => PendingEndpointKind::PrepareWorktreeCreate {
                workspace_id: workspace_id.clone(),
            },
            KeybindAction::OpenWorktree => PendingEndpointKind::PrepareWorktreeOpen {
                workspace_id: workspace_id.clone(),
            },
            KeybindAction::RemoveWorktree if !linked => {
                self.set_endpoint_error("This workspace is not a Herdr-managed worktree checkout.");
                outcome.repaint = true;
                return;
            }
            KeybindAction::RemoveWorktree => PendingEndpointKind::PrepareWorktreeRemove {
                workspace_id: workspace_id.clone(),
            },
            _ => return,
        };
        self.push_endpoint_method_with_kind(
            Method::WorktreeList(WorktreeListParams {
                workspace_id: Some(workspace_id),
                cwd: None,
                trust_repository: false,
            }),
            kind,
            outcome,
        );
    }

    pub(super) fn sync_worktree_create_path(&mut self) {
        let Some(worktree_directory) = self.endpoint_worktree_directory() else {
            return;
        };
        let Some(ClientShellOverlay::WorktreeCreate(create)) = self.overlay.as_mut() else {
            return;
        };
        if create.external_checkout {
            create.error = None;
            return;
        }
        create.checkout_path =
            checkout_path_preview(&worktree_directory, &create.repo_name, &create.branch);
        create.error = None;
    }

    pub(super) fn submit_worktree_create(&mut self, outcome: &mut ClientShellInput) {
        let worktree_directory = self.endpoint_worktree_directory();
        let Some(ClientShellOverlay::WorktreeCreate(create)) = self.overlay.as_mut() else {
            return;
        };
        if create.creating {
            return;
        }
        let branch = create.branch.trim().to_owned();
        if branch.is_empty() {
            create.error = Some(
                if create.external_checkout {
                    "name is required"
                } else {
                    "branch is required"
                }
                .to_owned(),
            );
            outcome.repaint = true;
            return;
        }
        create.branch.trim_and_accept();
        if create.external_checkout {
            create.creating = true;
            create.error = None;
            let workspace_id = create.source_workspace_id.clone();
            if !self.push_endpoint_method_with_kind(
                crate::api::schema::Method::CheckoutCreate(
                    crate::api::schema::CheckoutCreateParams {
                        workspace_id: Some(workspace_id),
                        cwd: None,
                        name: branch,
                        destination: None,
                        label: None,
                        focus: false,
                    },
                ),
                PendingEndpointKind::WorktreeCreate,
                outcome,
            ) {
                if let Some(ClientShellOverlay::WorktreeCreate(create)) = self.overlay.as_mut() {
                    create.creating = false;
                }
            }
            outcome.repaint = true;
            return;
        }
        let Some(worktree_directory) = worktree_directory else {
            return;
        };
        create.checkout_path =
            checkout_path_preview(&worktree_directory, &create.repo_name, &branch);
        create.creating = true;
        create.error = None;
        let workspace_id = create.source_workspace_id.clone();
        if !self.push_endpoint_method_with_kind(
            crate::api::schema::Method::WorktreeCreate(crate::api::schema::WorktreeCreateParams {
                workspace_id: Some(workspace_id),
                cwd: None,
                branch: Some(branch),
                base: Some("HEAD".to_owned()),
                path: None,
                label: None,
                focus: false,
                trust_repository: false,
            }),
            PendingEndpointKind::WorktreeCreate,
            outcome,
        ) {
            if let Some(ClientShellOverlay::WorktreeCreate(create)) = self.overlay.as_mut() {
                create.creating = false;
            }
        }
        outcome.repaint = true;
    }

    pub(super) fn move_worktree_open_selection(&mut self, delta: isize) {
        let Some(ClientShellOverlay::WorktreeOpen(open)) = self.overlay.as_mut() else {
            return;
        };
        let filtered = open.filtered_indices();
        if filtered.is_empty() {
            open.selected = 0;
            return;
        }
        let current = filtered
            .iter()
            .position(|index| *index == open.selected)
            .unwrap_or(0);
        let next = (current as isize + delta).clamp(0, filtered.len() as isize - 1) as usize;
        open.selected = filtered[next];
    }

    pub(super) fn submit_worktree_open(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::WorktreeOpen(open)) = self.overlay.as_mut() else {
            return;
        };
        if open.opening {
            return;
        }
        let Some(index) = open.selected_entry_index() else {
            return;
        };
        let Some(entry) = open.entries.get(index) else {
            return;
        };
        let workspace_id = open.source_workspace_id.clone();
        let path = entry.path.clone();
        let checkout_id = entry.checkout_id.clone();
        let external_checkout = open.external_checkout;
        open.selected = index;
        open.opening = true;
        open.error = None;
        let method = if let Some(checkout_id) = checkout_id {
            crate::api::schema::Method::CheckoutOpen(crate::api::schema::CheckoutOpenParams {
                workspace_id: Some(workspace_id),
                cwd: None,
                checkout_id,
                label: None,
                focus: true,
            })
        } else {
            crate::api::schema::Method::WorktreeOpen(crate::api::schema::WorktreeOpenParams {
                workspace_id: Some(workspace_id),
                cwd: None,
                path: Some(path),
                branch: None,
                label: None,
                focus: true,
                trust_repository: false,
            })
        };
        if !self.push_endpoint_method_with_kind(
            method,
            if external_checkout {
                PendingEndpointKind::CheckoutOpen
            } else {
                PendingEndpointKind::WorktreeOpen
            },
            outcome,
        ) {
            if let Some(ClientShellOverlay::WorktreeOpen(open)) = self.overlay.as_mut() {
                open.opening = false;
            }
        }
        outcome.repaint = true;
    }

    pub(super) fn submit_worktree_remove(&mut self, outcome: &mut ClientShellInput) {
        let Some(ClientShellOverlay::WorktreeRemove(remove)) = self.overlay.as_mut() else {
            return;
        };
        if remove.removing {
            return;
        }
        let workspace_id = remove.workspace_id.clone();
        let forced = remove.force_confirmation;
        let external_checkout = remove.external_checkout;
        remove.removing = true;
        remove.error = None;
        let method = if external_checkout {
            crate::api::schema::Method::CheckoutRemove(crate::api::schema::CheckoutRemoveParams {
                workspace_id,
                force: forced,
            })
        } else {
            crate::api::schema::Method::WorktreeRemove(crate::api::schema::WorktreeRemoveParams {
                workspace_id,
                force: forced,
                trust_repository: false,
            })
        };
        if !self.push_endpoint_method_with_kind(
            method,
            if external_checkout {
                PendingEndpointKind::CheckoutRemove { forced }
            } else {
                PendingEndpointKind::WorktreeRemove { forced }
            },
            outcome,
        ) {
            if let Some(ClientShellOverlay::WorktreeRemove(remove)) = self.overlay.as_mut() {
                remove.removing = false;
            }
        }
        outcome.repaint = true;
    }

    pub(super) fn handle_worktree_endpoint_result(
        &mut self,
        kind: PendingEndpointKind,
        result: Result<crate::api::schema::ResponseResult, ClientShellEndpointError>,
        outcome: &mut ClientShellInput,
    ) -> bool {
        use crate::api::schema::ResponseResult;

        match (kind, result) {
            (
                PendingEndpointKind::PrepareWorktreeCreate { workspace_id },
                Ok(ResponseResult::WorktreeList { source, .. }),
            ) => {
                let seed = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|duration| duration.as_micros().min(u128::from(u64::MAX)) as u64)
                    .unwrap_or(0);
                let branch = crate::worktree::generated_branch_slug(seed);
                let Some(worktree_directory) = self.endpoint_worktree_directory() else {
                    return false;
                };
                let checkout_path =
                    checkout_path_preview(&worktree_directory, &source.repo_name, &branch);
                self.overlay = Some(ClientShellOverlay::WorktreeCreate(
                    ClientWorktreeCreateOverlay {
                        external_checkout: false,
                        source_workspace_id: workspace_id,
                        repo_name: source.repo_name,
                        branch: TextEditor::new(&branch, true),
                        checkout_path,
                        error: None,
                        creating: false,
                    },
                ));
                true
            }
            (
                PendingEndpointKind::PrepareWorktreeOpen { workspace_id },
                Ok(ResponseResult::WorktreeList { worktrees, .. }),
            ) => {
                let entries = worktrees
                    .into_iter()
                    .filter(|entry| !entry.is_bare && !entry.is_prunable)
                    .map(|entry| {
                        let label = entry.branch.clone().unwrap_or_else(|| entry.label.clone());
                        ClientWorktreeOpenEntry {
                            checkout_id: None,
                            path: entry.path,
                            branch: entry.branch,
                            is_linked_worktree: entry.is_linked_worktree,
                            is_detached: entry.is_detached,
                            open_workspace_id: entry.open_workspace_id,
                            label,
                        }
                    })
                    .collect::<Vec<_>>();
                if entries.is_empty() {
                    self.set_endpoint_error("No Git worktrees found for this repo.");
                } else {
                    self.overlay = Some(ClientShellOverlay::WorktreeOpen(
                        ClientWorktreeOpenOverlay {
                            external_checkout: false,
                            source_workspace_id: workspace_id,
                            entries,
                            selected: 0,
                            query: TextEditor::default(),
                            search_focused: false,
                            error: None,
                            opening: false,
                        },
                    ));
                }
                true
            }
            (
                PendingEndpointKind::PrepareWorktreeRemove { workspace_id },
                Ok(ResponseResult::WorktreeList { worktrees, .. }),
            ) => {
                let path = worktrees
                    .into_iter()
                    .find(|entry| entry.open_workspace_id.as_deref() == Some(&workspace_id))
                    .map(|entry| entry.path);
                if let Some(path) = path {
                    self.overlay = Some(ClientShellOverlay::WorktreeRemove(
                        ClientWorktreeRemoveOverlay {
                            external_checkout: false,
                            workspace_id,
                            path,
                            error: None,
                            removing: false,
                            force_confirmation: false,
                        },
                    ));
                } else {
                    self.set_endpoint_error(
                        "This workspace is not a Herdr-managed worktree checkout.",
                    );
                }
                true
            }
            (
                PendingEndpointKind::PrepareCheckoutOpen { workspace_id },
                Ok(ResponseResult::CheckoutList { checkouts, .. }),
            ) => {
                let entries = checkouts
                    .into_iter()
                    .map(|entry| ClientWorktreeOpenEntry {
                        checkout_id: Some(entry.id),
                        path: entry.path,
                        branch: None,
                        is_linked_worktree: true,
                        is_detached: false,
                        open_workspace_id: entry.open_workspace_id,
                        label: entry.name,
                    })
                    .collect::<Vec<_>>();
                if entries.is_empty() {
                    self.set_endpoint_error("No checkouts found for this repository.");
                } else {
                    self.overlay = Some(ClientShellOverlay::WorktreeOpen(
                        ClientWorktreeOpenOverlay {
                            external_checkout: true,
                            source_workspace_id: workspace_id,
                            entries,
                            selected: 0,
                            query: TextEditor::default(),
                            search_focused: false,
                            error: None,
                            opening: false,
                        },
                    ));
                }
                true
            }
            (
                PendingEndpointKind::WorktreeCreate,
                Ok(ResponseResult::WorktreeCreated { tab, .. }),
            ) => {
                self.overlay = None;
                self.push_endpoint_method(
                    crate::api::schema::Method::TabFocus(crate::api::schema::TabTarget {
                        tab_id: tab.tab_id,
                    }),
                    outcome,
                );
                true
            }
            (
                PendingEndpointKind::WorktreeCreate,
                Ok(ResponseResult::CheckoutCreated { tab, .. }),
            ) => {
                self.overlay = None;
                self.push_endpoint_method(
                    crate::api::schema::Method::TabFocus(crate::api::schema::TabTarget {
                        tab_id: tab.tab_id,
                    }),
                    outcome,
                );
                true
            }
            (PendingEndpointKind::CheckoutOpen, Ok(ResponseResult::CheckoutOpened { .. }))
            | (
                PendingEndpointKind::CheckoutRemove { .. },
                Ok(ResponseResult::CheckoutRemoved { .. }),
            ) => {
                self.overlay = None;
                true
            }
            (PendingEndpointKind::WorktreeOpen, Ok(ResponseResult::WorktreeOpened { .. }))
            | (
                PendingEndpointKind::WorktreeRemove { .. },
                Ok(ResponseResult::WorktreeRemoved { .. }),
            ) => {
                self.overlay = None;
                true
            }
            (PendingEndpointKind::WorktreeCreate, Err(error)) => {
                if let Some(ClientShellOverlay::WorktreeCreate(create)) = self.overlay.as_mut() {
                    create.creating = false;
                    create.error = Some(error.message);
                }
                true
            }
            (PendingEndpointKind::WorktreeOpen, Err(error)) => {
                if let Some(ClientShellOverlay::WorktreeOpen(open)) = self.overlay.as_mut() {
                    open.opening = false;
                    open.error = Some(error.message);
                }
                true
            }
            (PendingEndpointKind::CheckoutOpen, Err(error)) => {
                if let Some(ClientShellOverlay::WorktreeOpen(open)) = self.overlay.as_mut() {
                    open.opening = false;
                    open.error = Some(error.message);
                }
                true
            }
            (PendingEndpointKind::CheckoutRemove { forced: false }, Err(error))
                if error.code.as_deref() == Some("dirty_checkout_requires_force") =>
            {
                if let Some(ClientShellOverlay::WorktreeRemove(remove)) = self.overlay.as_mut() {
                    let can_force = self
                        .endpoints
                        .iter()
                        .find(|endpoint| endpoint.endpoint_id == self.active_endpoint_id)
                        .and_then(|endpoint| {
                            Self::endpoint_workspace_vcs(endpoint, &remove.workspace_id)
                        })
                        .is_some_and(|vcs| {
                            vcs.capabilities
                                .iter()
                                .any(|capability| capability == "checkout.remove.force")
                        });
                    remove.removing = false;
                    remove.force_confirmation = can_force;
                    remove.error = (!can_force).then_some(error.message);
                }
                true
            }
            (PendingEndpointKind::CheckoutRemove { .. }, Err(error)) => {
                if let Some(ClientShellOverlay::WorktreeRemove(remove)) = self.overlay.as_mut() {
                    remove.removing = false;
                    remove.error = Some(error.message);
                }
                true
            }
            (PendingEndpointKind::WorktreeRemove { forced: false }, Err(error))
                if error.code.as_deref() == Some("dirty_worktree_requires_force")
                    || (error.code.as_deref() == Some("worktree_remove_failed")
                        && crate::worktree::is_not_working_tree_remove_error(&error.message)) =>
            {
                if let Some(ClientShellOverlay::WorktreeRemove(remove)) = self.overlay.as_mut() {
                    remove.removing = false;
                    remove.force_confirmation = true;
                    remove.error = None;
                }
                true
            }
            (PendingEndpointKind::WorktreeRemove { .. }, Err(error)) => {
                if let Some(ClientShellOverlay::WorktreeRemove(remove)) = self.overlay.as_mut() {
                    remove.removing = false;
                    remove.error = Some(error.message);
                }
                true
            }
            (
                PendingEndpointKind::PrepareWorktreeCreate { .. }
                | PendingEndpointKind::PrepareWorktreeOpen { .. }
                | PendingEndpointKind::PrepareWorktreeRemove { .. }
                | PendingEndpointKind::PrepareCheckoutOpen { .. },
                Err(_),
            ) => true,
            (_, Ok(_)) => {
                self.set_endpoint_error("endpoint returned an unexpected worktree result");
                true
            }
            (
                PendingEndpointKind::Generic
                | PendingEndpointKind::ProductAnnouncementDismiss { .. }
                | PendingEndpointKind::ReleaseNotesDismiss
                | PendingEndpointKind::PopupCommand
                | PendingEndpointKind::ReloadConfig
                | PendingEndpointKind::IntegrationList
                | PendingEndpointKind::IntegrationInstall
                | PendingEndpointKind::SelectionCopy
                | PendingEndpointKind::PaneScroll { .. }
                | PendingEndpointKind::WordSelection { .. }
                | PendingEndpointKind::PaneLinkActivate { .. }
                | PendingEndpointKind::PaneLinkResolve { .. }
                | PendingEndpointKind::CopyMotion { .. }
                | PendingEndpointKind::CopySearch { .. },
                Err(_),
            ) => true,
        }
    }
}

#[cfg(test)]
mod path_tests {
    use super::checkout_path_preview;

    #[test]
    fn endpoint_path_style_is_independent_of_the_client_os() {
        assert_eq!(
            checkout_path_preview("/worktrees/", "repo", "feature/a"),
            "/worktrees/repo/feature-a"
        );
        assert_eq!(
            checkout_path_preview(r"C:\worktrees\", "repo", "feature/a"),
            r"C:\worktrees\repo\feature-a"
        );
        assert_eq!(
            checkout_path_preview(r"\\server\share", "repo", "feature/a"),
            r"\\server\share\repo\feature-a"
        );
    }
}
