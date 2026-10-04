use crate::api::schema::{ErrorResponse, Method, Request, ResponseResult, SuccessResponse};
use crate::config::Config;
use crate::events::{
    AppEvent, ExternalCheckoutContext, ExternalCheckoutMutation, ExternalCheckoutOutcome,
    ExternalCheckoutRemovalRecovery, ExternalCheckoutResult, ExternalCheckoutSource,
};
use crate::workspace::{CheckoutSpaceMembership, Workspace};

use crate::app::App;

fn test_app() -> App {
    test_app_with_event_hub(crate::api::EventHub::default())
}

fn test_app_with_event_hub(event_hub: crate::api::EventHub) -> App {
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    App::new(
        &Config::default(),
        crate::app::AppPolicy::TEST,
        None,
        api_rx,
        event_hub,
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

fn unique_temp_path(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!("herdr-{name}-{}-{nanos}", std::process::id()))
}

#[tokio::test]
async fn external_checkout_open_creates_then_reuses_workspace() {
    let root = unique_temp_path("external-checkout-open");
    let checkout_path = root.join("topic");
    std::fs::create_dir_all(&checkout_path).expect("create checkout directory");
    let event_hub = crate::api::EventHub::default();
    let mut app = test_app_with_event_hub(event_hub.clone());
    let source = ExternalCheckoutSource {
        repository_root: root.clone(),
        source_cwd: root.clone(),
        ..source()
    };
    let checkout = crate::vcs::Checkout {
        id: "topic-id".into(),
        name: "topic".into(),
        path: checkout_path.clone(),
        managed: true,
    };

    let created: SuccessResponse = serde_json::from_str(&app.finish_external_checkout_open(
        "create".into(),
        source.clone(),
        checkout.clone(),
        Some("Topic".into()),
        true,
        true,
    ))
    .expect("created checkout response");
    let ResponseResult::CheckoutCreated {
        workspace,
        tab,
        root_pane,
        ..
    } = created.result
    else {
        panic!("expected checkout_created response");
    };
    assert_eq!(tab.workspace_id, workspace.workspace_id);
    assert_eq!(root_pane.workspace_id, workspace.workspace_id);
    assert_eq!(app.state.workspaces.len(), 1);
    assert_eq!(app.state.active, Some(0));
    assert_eq!(
        app.state.workspaces[0].custom_name.as_deref(),
        Some("Topic")
    );
    assert_eq!(
        app.state.workspaces[0]
            .checkout_space
            .as_ref()
            .map(|membership| membership.checkout_id.as_str()),
        Some("topic-id")
    );
    let created_events = event_hub
        .events_after(0)
        .into_iter()
        .map(|(_, event)| event.event)
        .collect::<Vec<_>>();
    assert_eq!(
        created_events,
        vec![
            crate::api::schema::EventKind::WorkspaceCreated,
            crate::api::schema::EventKind::TabCreated,
            crate::api::schema::EventKind::PaneCreated,
            crate::api::schema::EventKind::LayoutUpdated,
        ]
    );
    let sequence_after_create = event_hub.current_sequence();

    let opened: SuccessResponse = serde_json::from_str(&app.finish_external_checkout_open(
        "open".into(),
        source,
        checkout,
        None,
        true,
        false,
    ))
    .expect("opened checkout response");
    let ResponseResult::CheckoutOpened { already_open, .. } = opened.result else {
        panic!("expected checkout_opened response");
    };
    assert!(already_open);
    assert_eq!(app.state.workspaces.len(), 1);
    assert_eq!(app.state.active, Some(0));
    assert!(event_hub.events_after(sequence_after_create).is_empty());

    for (_, runtime) in app.terminal_runtimes.drain() {
        runtime.shutdown();
    }
    drop(app);
    std::fs::remove_dir_all(root).expect("remove checkout directory");
}

#[tokio::test]
async fn failed_external_remove_restores_its_checkout_runtime() {
    let root = unique_temp_path("external-checkout-remove-failure");
    let checkout_path = root.join("external");
    let git_path = root.join("git");
    std::fs::create_dir_all(&checkout_path).expect("create external checkout");
    std::fs::create_dir_all(&git_path).expect("create git checkout");
    let mut app = test_app();
    let mut workspace = Workspace::test_new("external");
    workspace.identity_cwd = checkout_path.clone();
    workspace.checkout_space = Some(CheckoutSpaceMembership {
        provider_id: "arc".into(),
        provider_display_name: "Arc".into(),
        repository_key: "arc:repo".into(),
        repository_root: root.clone(),
        checkout_id: "external-id".into(),
        checkout_name: "external".into(),
        checkout_path: checkout_path.clone(),
        managed: true,
        source_workspace_id: None,
    });
    // Explicitly exercise a workspace with both legacy memberships. Runtime
    // recovery must use the backend that initiated the removal.
    workspace.worktree_space = Some(crate::workspace::WorktreeSpaceMembership {
        key: "git:repo".into(),
        label: "git".into(),
        repo_root: git_path.clone(),
        checkout_path: git_path,
        is_linked_worktree: true,
    });
    let workspace_id = workspace.id.clone();
    let pane_id = workspace.tabs[0].root_pane;
    let terminal_id = workspace.terminal_id(pane_id).cloned().unwrap();
    let foreground = Workspace::test_new("foreground");
    let foreground_id = foreground.id.clone();
    app.state.workspaces = vec![workspace, foreground];
    app.state.active = Some(1);
    app.state.selected = 1;
    app.state.ensure_test_terminals();
    app.state.terminals.get_mut(&terminal_id).unwrap().cwd = checkout_path.clone();
    let (runtime, _input_rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
    app.terminal_runtimes.insert(terminal_id.clone(), runtime);

    let shutdown_panes = app.shutdown_workspace_terminal_runtimes_for_checkout_remove(0);
    assert_eq!(shutdown_panes, vec![pane_id]);
    let checkout_key = crate::worktree::canonical_or_original(&checkout_path);
    let operation_id = app
        .checkout_requests
        .reserve_remove(workspace_id.clone(), checkout_key.clone())
        .unwrap();
    let (respond_to, response_rx) = std::sync::mpsc::channel();

    app.handle_external_checkout_finished(ExternalCheckoutResult {
        context: ExternalCheckoutContext {
            id: "remove".into(),
            source: ExternalCheckoutSource {
                repository_root: root.clone(),
                source_cwd: checkout_path.clone(),
                ..source()
            },
            registry_generation: app.vcs_registry.generation(),
            respond_to,
        },
        mutation: Some(ExternalCheckoutMutation::Remove {
            operation_id,
            workspace_id: workspace_id.clone(),
            checkout_key,
        }),
        result: Err(("vcs_operation_failed".into(), "simulated failure".into())),
        removal_recovery: Some(ExternalCheckoutRemovalRecovery {
            path: checkout_path.clone(),
            shutdown_panes,
            operation_id,
        }),
    });

    let response: ErrorResponse = serde_json::from_str(&response_rx.recv().unwrap()).unwrap();
    assert_eq!(response.error.code, "vcs_operation_failed");
    assert!(app.checkout_requests.is_empty());
    assert_eq!(app.state.workspaces.len(), 2);
    assert_eq!(app.state.terminals[&terminal_id].cwd, checkout_path);
    assert!(app
        .pending_checkout_remove_runtime_restores
        .contains_key(&pane_id));

    app.handle_internal_event_with_pane_updates(AppEvent::PaneDied {
        pane_id,
        exit_reason: crate::platform::ChildExitReason::Exited,
    });
    assert!(app.pending_checkout_remove_runtime_exits.is_empty());
    assert!(app.pending_checkout_remove_runtime_restores.is_empty());
    assert!(app.terminal_runtimes.get(&terminal_id).is_some());
    assert_eq!(
        app.state.active.map(|idx| &app.state.workspaces[idx].id),
        Some(&foreground_id)
    );
    assert_eq!(app.state.workspaces[app.state.selected].id, foreground_id);

    crate::app::api::test_support::shutdown_test_runtimes(&mut app);
    std::fs::remove_dir_all(root).expect("remove test checkout tree");
}

#[tokio::test]
async fn external_remove_keeps_runtime_alive_during_provider_preflight() {
    let mut config = Config::default();
    config.vcs.providers = vec![crate::config::VcsProviderConfig {
        id: "arc".into(),
        display_name: "Arc".into(),
        command: vec!["herdr-test-provider-does-not-exist".into()],
        discovery: vec![crate::config::VcsDiscoveryMarkerConfig {
            path: ".arc/HEAD".into(),
            kind: crate::config::VcsDiscoveryMarkerKind::File,
        }],
        allow: vec!["checkout.list".into(), "checkout.remove".into()],
        ..crate::config::VcsProviderConfig::default()
    }];
    let (_api_tx, api_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(
        &config,
        crate::app::AppPolicy::TEST,
        None,
        api_rx,
        crate::api::EventHub::default(),
    );
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
    let pane_id = workspace.tabs[0].root_pane;
    let terminal_id = workspace
        .terminal_id(pane_id)
        .cloned()
        .expect("root terminal");
    app.state.workspaces = vec![workspace];
    app.state.ensure_test_terminals();
    let (runtime, _input_rx) = crate::terminal::TerminalRuntime::test_with_channel(80, 24);
    app.terminal_runtimes.insert(terminal_id.clone(), runtime);
    let (respond_to, response_rx) = std::sync::mpsc::channel();

    assert!(app.handle_deferred_checkout_api_request(
        Request {
            id: "remove".into(),
            method: Method::CheckoutRemove(crate::api::schema::CheckoutRemoveParams {
                workspace_id,
                force: false,
            }),
        },
        respond_to,
    ));

    assert!(app.terminal_runtimes.get(&terminal_id).is_some());
    assert!(app.pending_checkout_remove_runtime_exits.is_empty());
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), app.event_rx.recv())
        .await
        .expect("provider preflight timeout")
        .expect("provider preflight event");
    assert!(matches!(event, AppEvent::ExternalCheckoutFinished(_)));
    app.handle_internal_event(event);
    let response: ErrorResponse = serde_json::from_str(
        &response_rx
            .recv_timeout(std::time::Duration::from_secs(1))
            .expect("provider failure response"),
    )
    .expect("error response");
    assert_eq!(response.error.code, "vcs_provider_failed");
    assert!(app.terminal_runtimes.get(&terminal_id).is_some());
    assert!(app.checkout_requests.is_empty());
    crate::app::api::test_support::shutdown_test_runtimes(&mut app);
}

#[test]
fn stale_read_completion_is_rejected_after_config_reload() {
    let mut app = test_app();
    let (respond_to, response_rx) = std::sync::mpsc::channel();

    app.handle_external_checkout_finished(ExternalCheckoutResult {
        context: ExternalCheckoutContext {
            id: "request".into(),
            source: source(),
            registry_generation: app.vcs_registry.generation().saturating_sub(1),
            respond_to,
        },
        mutation: None,
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
        context: ExternalCheckoutContext {
            id: "request".into(),
            source: source(),
            registry_generation: app.vcs_registry.generation().saturating_sub(1),
            respond_to,
        },
        mutation: Some(ExternalCheckoutMutation::Remove {
            operation_id,
            workspace_id: workspace_id.clone(),
            checkout_key,
        }),
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
    for removing in [false, true] {
        let mut app = test_app();
        let path = std::path::PathBuf::from("/repo/topic");
        let checkout_key = crate::worktree::canonical_or_original(&path);
        let workspace_id = "workspace".to_owned();
        let mutation = if removing {
            let operation_id = app
                .checkout_requests
                .reserve_remove(workspace_id.clone(), checkout_key.clone())
                .unwrap();
            ExternalCheckoutMutation::Remove {
                operation_id,
                workspace_id: workspace_id.clone(),
                checkout_key: checkout_key.clone(),
            }
        } else {
            let operation_id = app
                .checkout_requests
                .reserve_create(checkout_key.clone())
                .unwrap();
            ExternalCheckoutMutation::Create {
                operation_id,
                checkout_key: checkout_key.clone(),
            }
        };
        let (respond_to, response_rx) = std::sync::mpsc::channel();

        app.handle_external_checkout_finished(ExternalCheckoutResult {
            context: ExternalCheckoutContext {
                id: "request".into(),
                source: source(),
                registry_generation: app.vcs_registry.generation(),
                respond_to,
            },
            mutation: Some(mutation),
            result: Err(("checkout_outcome_unknown".into(), "timed out".into())),
            removal_recovery: None,
        });

        let response: ErrorResponse = serde_json::from_str(&response_rx.recv().unwrap()).unwrap();
        assert_eq!(response.error.code, "checkout_outcome_unknown");
        assert!(app
            .checkout_requests
            .reserve_create(checkout_key.clone())
            .is_err());
        assert!(app
            .checkout_requests
            .reserve_remove(workspace_id.clone(), checkout_key)
            .is_err());
        let other_path = std::path::PathBuf::from("/repo/other");
        if removing {
            assert!(app
                .checkout_requests
                .reserve_remove(workspace_id, other_path.clone())
                .is_err());
        }
        assert!(app.checkout_requests.reserve_create(other_path).is_ok());
    }
}
