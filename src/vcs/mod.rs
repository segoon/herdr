//! Version-control provider boundary.
//!
//! Git remains built in. External providers normalize their native CLI and data
//! model to this protocol, so Herdr never needs to know a proprietary VCS's
//! command line or output format.

mod discovery;
mod path;
mod protocol;
mod provider;

pub(crate) use discovery::{validate_config, Registry};
pub(crate) use path::{repository_key, ExactPath};
pub(crate) use protocol::ProviderFailure;
pub(crate) use provider::{ActivatedProvider, ExternalProvider};

/// Backend shared by checkout dialogs and runtime recovery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CheckoutBackend {
    GitWorktree,
    ExternalVcs,
}

impl CheckoutBackend {
    pub(crate) fn is_external(self) -> bool {
        matches!(self, Self::ExternalVcs)
    }
}

/// Provider output after wire paths have been decoded and validated.
#[derive(Debug, Clone)]
pub(crate) struct Checkout {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) path: std::path::PathBuf,
    pub(crate) managed: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct InspectResult {
    pub(crate) root: std::path::PathBuf,
    pub(crate) branch: Option<String>,
    pub(crate) ahead: Option<u64>,
    pub(crate) behind: Option<u64>,
}

pub(crate) const BUILTIN_GIT_PROVIDER_ID: &str = "builtin.git";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Capability {
    Inspect,
    CheckoutList,
    CheckoutCreate,
    CheckoutRemove,
    CheckoutRemoveForce,
}

impl Capability {
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "inspect" => Some(Self::Inspect),
            "checkout.list" => Some(Self::CheckoutList),
            "checkout.create" => Some(Self::CheckoutCreate),
            "checkout.remove" => Some(Self::CheckoutRemove),
            "checkout.remove.force" => Some(Self::CheckoutRemoveForce),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Inspect => "inspect",
            Self::CheckoutList => "checkout.list",
            Self::CheckoutCreate => "checkout.create",
            Self::CheckoutRemove => "checkout.remove",
            Self::CheckoutRemoveForce => "checkout.remove.force",
        }
    }
}

#[cfg(test)]
use protocol::{RequestOperation, ResponseOperation, WireRequest, WireResponse, WireResult};
#[cfg(test)]
use provider::{negotiate_capabilities, next_request_id};

#[cfg(test)]
mod tests;
