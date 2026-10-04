use crate::api::schema::{CheckoutCreateParams, CheckoutSourceInfo};
use crate::events::{ExternalCheckoutFailure, ExternalCheckoutOutcome, ExternalCheckoutSource};
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
        error: ExternalCheckoutFailure,
    ) -> crate::events::ExternalCheckoutCompletion {
        match self {
            Self::Remove(operation) => crate::events::ExternalCheckoutCompletion::Remove {
                operation: Box::new(operation),
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
) -> Result<ExternalCheckoutOutcome, ExternalCheckoutFailure> {
    let root = ExactPath::from_path(&source.repository_root);
    match operation {
        CheckoutOperation::List => {
            let checkouts = provider
                .checkout_list(root)
                .await
                .map_err(ExternalCheckoutFailure::Provider)?;
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
                .map_err(ExternalCheckoutFailure::Provider)?;
            capture_source_checkout(source, &checkouts);
            let checkout = checkouts
                .into_iter()
                .find(|checkout| checkout.id == checkout_id)
                .ok_or(ExternalCheckoutFailure::NotFound)?;
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
                            ExternalCheckoutFailure::OutcomeUnknown(format!(
                                    "checkout creation timed out and could not be reconciled: {reconcile_error}"
                                ))
                        })?
                        .into_iter()
                        .find(|checkout| same_checkout_path(&checkout.path, &destination))
                        .ok_or_else(|| {
                            ExternalCheckoutFailure::OutcomeUnknown(format!(
                                    "checkout creation timed out and the destination was not reported during reconciliation: {error}"
                                ))
                        })?
                }
                Err(error) => return Err(ExternalCheckoutFailure::Provider(error)),
            };
            if !same_checkout_path(&checkout.path, &destination) {
                return Err(ExternalCheckoutFailure::OutcomeUnknown(
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
) -> Result<(), ExternalCheckoutFailure> {
    let listed = provider
        .checkout_list(ExactPath::from_path(&source.repository_root))
        .await
        .map_err(ExternalCheckoutFailure::Provider)?;
    let valid = listed.iter().any(|checkout| {
        checkout.id == membership.checkout_id
            && checkout.managed
            && same_checkout_path(&checkout.path, &membership.checkout_path)
    });
    if valid {
        Ok(())
    } else {
        Err(ExternalCheckoutFailure::NotManaged)
    }
}

pub(super) async fn execute_checkout_remove(
    provider: &crate::vcs::ActivatedProvider,
    source: &ExternalCheckoutSource,
    operation: &crate::events::ExternalCheckoutRemoveOperation,
) -> Result<(), ExternalCheckoutFailure> {
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
                    return Err(ExternalCheckoutFailure::OutcomeUnknown(format!(
                            "checkout removal timed out and the checkout is still reported during reconciliation: {error}"
                        )));
                }
                Err(reconcile_error) => {
                    return Err(ExternalCheckoutFailure::OutcomeUnknown(format!(
                        "checkout removal timed out and could not be reconciled: {reconcile_error}"
                    )));
                }
            }
        } else {
            return Err(ExternalCheckoutFailure::Provider(error));
        }
    }
    Ok(())
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
