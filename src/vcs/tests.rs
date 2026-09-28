use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::ffi::OsStr;

use crate::config::{
    VcsConfig, VcsDiscoveryMarkerConfig, VcsDiscoveryMarkerKind, VcsProviderConfig,
};

use super::*;

fn provider(id: &str, priority: i32, marker: &str) -> VcsProviderConfig {
    VcsProviderConfig {
        id: id.into(),
        display_name: id.into(),
        command: vec!["provider".into()],
        platforms: vec![std::env::consts::OS.into()],
        priority,
        discovery: vec![VcsDiscoveryMarkerConfig {
            path: marker.into(),
            kind: VcsDiscoveryMarkerKind::File,
        }],
        allow: vec!["inspect".into()],
        ..VcsProviderConfig::default()
    }
}

fn temp_dir(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "herdr-vcs-{name}-{}-{}",
        std::process::id(),
        next_request_id()
    ));
    std::fs::create_dir_all(&path).expect("create VCS test directory");
    path
}

#[test]
fn validates_provider_boundaries() {
    let mut config = VcsConfig {
        providers: vec![provider("private", 10, ".private/HEAD")],
    };
    assert!(validate_config(&config).is_empty());

    config.providers[0].discovery[0].path = "../secret".into();
    assert!(validate_config(&config)
        .iter()
        .any(|message| message.contains("relative literal path")));
    config.providers[0].discovery[0].path = ".private/HEAD".into();
    config
        .providers
        .push(provider("private", 20, ".other/HEAD"));
    assert!(validate_config(&config)
        .iter()
        .any(|message| message.contains("duplicate VCS provider id")));

    let mut remove_without_list = provider("remove-only", 10, ".private/HEAD");
    remove_without_list.allow = vec!["checkout.remove".into()];
    let diagnostics = validate_config(&VcsConfig {
        providers: vec![remove_without_list],
    });
    assert!(diagnostics
        .iter()
        .any(|message| message.contains("checkout.remove without checkout.list")));
}

#[test]
fn discovery_uses_deepest_root_then_priority_and_rejects_ties() {
    let root = temp_dir("discovery");
    let nested = root.join("nested");
    let cwd = nested.join("src");
    std::fs::create_dir_all(root.join(".one")).expect("create marker parent");
    std::fs::write(root.join(".one/HEAD"), "head").expect("create marker");
    std::fs::create_dir_all(nested.join(".two")).expect("create nested marker parent");
    std::fs::write(nested.join(".two/HEAD"), "head").expect("create nested marker");
    std::fs::create_dir_all(&cwd).expect("create cwd");

    let config = VcsConfig {
        providers: vec![
            provider("one", 100, ".one/HEAD"),
            provider("two", 1, ".two/HEAD"),
        ],
    };
    let registry = Registry::from_config(&config, 7).expect("valid registry");
    let found = registry
        .discover(&cwd)
        .expect("unambiguous discovery")
        .expect("provider discovered");
    assert_eq!(found.provider.id(), "two");
    assert_eq!(found.root, nested);

    std::fs::create_dir_all(root.join(".two")).expect("create tied marker parent");
    std::fs::write(root.join(".two/HEAD"), "head").expect("create tied marker");
    let tied = VcsConfig {
        providers: vec![
            provider("one", 5, ".one/HEAD"),
            provider("two", 5, ".two/HEAD"),
        ],
    };
    let registry = Registry::from_config(&tied, 8).expect("valid tied registry");
    assert!(registry.discover(&root).is_err());

    std::fs::remove_dir_all(root).expect("remove VCS test directory");
}

#[test]
fn exact_paths_round_trip_native_paths() {
    let path = Path::new("/tmp/a path");
    assert_eq!(ExactPath::from_path(path).to_path_buf().unwrap(), path);
}

#[cfg(unix)]
#[test]
fn exact_paths_round_trip_non_utf8_unix_paths() {
    use std::os::unix::ffi::OsStrExt;
    let path = Path::new(OsStr::from_bytes(b"/tmp/non-utf8-\xff"));
    assert_eq!(ExactPath::from_path(path).to_path_buf().unwrap(), path);
}

#[test]
fn capability_negotiation_ignores_unknowns_and_requires_allowlist() {
    let configured = [Capability::Inspect, Capability::CheckoutList]
        .into_iter()
        .collect::<BTreeSet<_>>();
    let advertised = ["inspect", "checkout.create", "future.capability"]
        .into_iter()
        .filter_map(Capability::parse)
        .collect::<BTreeSet<_>>();
    let effective = configured
        .intersection(&advertised)
        .copied()
        .collect::<BTreeSet<_>>();
    assert_eq!(effective, [Capability::Inspect].into_iter().collect());
    assert_eq!(Capability::Inspect.as_str(), "inspect");
}

#[test]
fn removal_is_not_effective_without_list_capability() {
    let allowed = [
        Capability::CheckoutList,
        Capability::CheckoutRemove,
        Capability::CheckoutRemoveForce,
    ]
    .into_iter()
    .collect();
    let advertised = vec!["checkout.remove".into(), "checkout.remove.force".into()];

    assert!(negotiate_capabilities(&allowed, &advertised).is_empty());

    let advertised = vec!["checkout.list".into(), "checkout.remove".into()];
    assert_eq!(
        negotiate_capabilities(&allowed, &advertised),
        [Capability::CheckoutList, Capability::CheckoutRemove]
            .into_iter()
            .collect()
    );
}

#[test]
fn protocol_uses_dotted_checkout_operations_and_tagged_paths() {
    let request = WireRequest {
        protocol_version: 1,
        request_id: 9,
        operation: RequestOperation::CheckoutCreate {
            root: ExactPath::Utf8("/repo".into()),
            name: "topic".into(),
            destination: ExactPath::Utf8("/checkouts/topic".into()),
        },
    };
    let value = serde_json::to_value(request).expect("serialize request");
    assert_eq!(value["operation"], "checkout.create");
    assert_eq!(value["root"]["encoding"], "utf8");
    assert_eq!(value["root"]["value"], "/repo");
    assert_eq!(value["destination"]["value"], "/checkouts/topic");
}

#[test]
fn provider_protocol_v1_matches_frozen_examples() {
    let fixture: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/vcs-provider-protocol-v1.json"
    )))
    .expect("VCS provider protocol fixture");
    let requests = fixture["requests"].as_object().expect("request examples");
    let examples = [
        (1, RequestOperation::Describe),
        (
            2,
            RequestOperation::Inspect {
                roots: vec![ExactPath::Utf8("/repo".into())],
            },
        ),
        (
            3,
            RequestOperation::CheckoutList {
                root: ExactPath::Utf8("/repo".into()),
            },
        ),
        (
            4,
            RequestOperation::CheckoutCreate {
                root: ExactPath::Utf8("/repo".into()),
                name: "topic".into(),
                destination: ExactPath::Utf8("/checkouts/topic".into()),
            },
        ),
        (
            5,
            RequestOperation::CheckoutRemove {
                root: ExactPath::Utf8("/repo".into()),
                checkout: ExactPath::Utf8("/checkouts/topic".into()),
                force: false,
            },
        ),
    ];
    for (request_id, operation) in examples {
        let name = match &operation {
            RequestOperation::Describe => "describe",
            RequestOperation::Inspect { .. } => "inspect",
            RequestOperation::CheckoutList { .. } => "checkout.list",
            RequestOperation::CheckoutCreate { .. } => "checkout.create",
            RequestOperation::CheckoutRemove { .. } => "checkout.remove",
        };
        assert_eq!(
            serde_json::to_value(WireRequest {
                protocol_version: 1,
                request_id,
                operation,
            })
            .unwrap(),
            requests[name]
        );
    }

    for (name, value) in fixture["responses"].as_object().expect("response examples") {
        let response: WireResponse = serde_json::from_value(value.clone())
            .unwrap_or_else(|error| panic!("invalid {name} response example: {error}"));
        assert_eq!(response.protocol_version, 1);
        assert!(response.request_id > 0);
    }
}

#[test]
fn protocol_ignores_unknown_response_fields_and_capabilities() {
    let response: WireResponse = serde_json::from_str(
        r#"{
                "protocol_version": 1,
                "request_id": 4,
                "status": "ok",
                "future_envelope_field": true,
                "response": {
                    "operation": "describe",
                    "capabilities": ["inspect", "future.capability"],
                    "future_response_field": 42
                }
            }"#,
    )
    .expect("forward-compatible response");
    let WireResult::Ok {
        response: ResponseOperation::Describe { capabilities },
    } = response.result
    else {
        panic!("expected describe response");
    };
    assert_eq!(capabilities, vec!["inspect", "future.capability"]);
}

#[cfg(unix)]
#[tokio::test]
async fn provider_runner_waits_for_eof_and_negotiates_capabilities() {
    use std::os::unix::fs::PermissionsExt;

    let root = temp_dir("runner");
    let executable = root.join("provider.sh");
    std::fs::write(
            &executable,
            r#"#!/bin/sh
input=$(sed -n '1,$p')
request_id=$(printf '%s' "$input" | sed -n 's/.*"request_id":\([0-9][0-9]*\).*/\1/p')
printf '{"protocol_version":1,"request_id":%s,"status":"ok","response":{"operation":"describe","capabilities":["inspect","checkout.create","future.capability"]}}' "$request_id"
"#,
        )
        .expect("write provider fixture");
    let mut permissions = std::fs::metadata(&executable)
        .expect("provider fixture metadata")
        .permissions();
    permissions.set_mode(0o755);
    std::fs::set_permissions(&executable, permissions).expect("make provider fixture executable");

    let config = VcsConfig {
        providers: vec![VcsProviderConfig {
            id: "private".into(),
            display_name: "Private".into(),
            command: vec![executable.display().to_string()],
            platforms: vec![std::env::consts::OS.into()],
            discovery: vec![VcsDiscoveryMarkerConfig {
                path: ".private/HEAD".into(),
                kind: VcsDiscoveryMarkerKind::File,
            }],
            allow: vec!["inspect".into(), "checkout.list".into()],
            ..VcsProviderConfig::default()
        }],
    };
    let registry = Registry::from_config(&config, 1).expect("valid provider registry");
    let description = registry.providers()[0]
        .activate()
        .await
        .expect("describe provider");
    assert_eq!(
        description.effective_capabilities,
        [Capability::Inspect].into_iter().collect()
    );

    std::fs::remove_dir_all(root).expect("remove provider fixture");
}
