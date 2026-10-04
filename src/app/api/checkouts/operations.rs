use crate::api::schema::{CheckoutCreateParams, CheckoutSourceInfo};
use crate::events::{ExternalCheckoutOutcome, ExternalCheckoutSource};
use crate::vcs::ExactPath;

/// Removal returns to the app thread after preflight; the other operations can
/// finish entirely in the provider task.
pub(super) enum PreparedCheckoutOperation {
    ReadOrCreate(CheckoutOperation),
    Remove(crate::events::ExternalCheckoutRemoveOperation),
}

pub(super) enum CheckoutOperation {
    List,
    Open {
        checkout_id: String,
        label: Option<String>,
        focus: bool,
    },
    Create {
        name: String,
        destination: std::path::PathBuf,
        label: Option<String>,
        focus: bool,
        operation_id: u64,
        checkout_key: std::path::PathBuf,
    },
}

impl PreparedCheckoutOperation {
    pub(super) fn failed(
        self,
        error: (String, String),
    ) -> crate::events::ExternalCheckoutCompletion {
        match self {
            Self::Remove(operation) => crate::events::ExternalCheckoutCompletion::Remove {
                operation,
                shutdown_panes: Vec::new(),
                result: Err(error),
            },
            Self::ReadOrCreate(operation) => {
                crate::events::ExternalCheckoutCompletion::ReadOrCreate {
                    creation: operation.creation(),
                    result: Err(error),
                }
            }
        }
    }
}

impl CheckoutOperation {
    pub(super) fn creation(&self) -> Option<crate::events::ExternalCheckoutCreateReservation> {
        match self {
            Self::Create {
                operation_id,
                checkout_key,
                ..
            } => Some(crate::events::ExternalCheckoutCreateReservation {
                operation_id: *operation_id,
                checkout_key: checkout_key.clone(),
            }),
            Self::List | Self::Open { .. } => None,
        }
    }
}

pub(super) async fn run_checkout_operation(
    provider: &crate::vcs::ActivatedProvider,
    source: &mut ExternalCheckoutSource,
    operation: CheckoutOperation,
) -> Result<ExternalCheckoutOutcome, (String, String)> {
    let root = ExactPath::from_path(&source.repository_root);
    match operation {
        CheckoutOperation::List => {
            let checkouts = provider
                .checkout_list(root)
                .await
                .map_err(provider_failure)?;
            Ok(ExternalCheckoutOutcome::Listed(checkouts))
        }
        CheckoutOperation::Open {
            checkout_id,
            label,
            focus,
            ..
        } => {
            let checkouts = provider
                .checkout_list(root)
                .await
                .map_err(provider_failure)?;
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
        CheckoutOperation::Create {
            name,
            destination,
            label,
            focus,
            ..
        } => {
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
                        .find(|checkout| same_checkout_path(&checkout.path, &destination))
                        .ok_or_else(|| {
                            (
                                "checkout_outcome_unknown".into(),
                                format!(
                                    "checkout creation timed out and the destination was not reported during reconciliation: {error}"
                                ),
                            )
                        })?
                }
                Err(error) => return Err(provider_failure(error)),
            };
            if !same_checkout_path(&checkout.path, &destination) {
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
    }
}

pub(super) async fn preflight_checkout_remove(
    provider: &crate::vcs::ActivatedProvider,
    source: &ExternalCheckoutSource,
    membership: &crate::workspace::CheckoutSpaceMembership,
) -> Result<(), (String, String)> {
    let listed = provider
        .checkout_list(ExactPath::from_path(&source.repository_root))
        .await
        .map_err(provider_failure)?;
    let valid = listed.iter().any(|checkout| {
        checkout.id == membership.checkout_id
            && checkout.managed
            && same_checkout_path(&checkout.path, &membership.checkout_path)
    });
    if valid {
        Ok(())
    } else {
        Err((
            "checkout_not_managed".into(),
            "provider no longer reports this checkout as managed".into(),
        ))
    }
}

pub(super) async fn execute_checkout_remove(
    provider: &crate::vcs::ActivatedProvider,
    source: &ExternalCheckoutSource,
    operation: &crate::events::ExternalCheckoutRemoveOperation,
) -> Result<(), (String, String)> {
    let crate::events::ExternalCheckoutRemoveOperation {
        membership, force, ..
    } = operation;
    let root = ExactPath::from_path(&source.repository_root);
    let remove_result = provider
        .checkout_remove(
            root.clone(),
            ExactPath::from_path(&membership.checkout_path),
            *force,
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
            return Err(provider_failure(error));
        }
    }
    Ok(())
}

fn provider_failure(error: crate::vcs::ProviderFailure) -> (String, String) {
    match &error {
        crate::vcs::ProviderFailure::Provider(provider) if provider.code == "checkout_dirty" => (
            "dirty_checkout_requires_force".into(),
            provider.message.clone(),
        ),
        _ => ("vcs_operation_failed".into(), error.to_string()),
    }
}

fn capture_source_checkout(
    source: &mut ExternalCheckoutSource,
    checkouts: &[crate::vcs::Checkout],
) {
    source.source_checkout = checkouts
        .iter()
        .filter_map(|checkout| {
            let path = crate::worktree::canonical_or_original(&checkout.path);
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

pub(super) fn checkout_create_destination(
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

pub(super) fn checkout_source_info(source: &ExternalCheckoutSource) -> CheckoutSourceInfo {
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
