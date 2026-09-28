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
pub(crate) use protocol::{Checkout, InspectResult, ProviderFailure};
pub(crate) use provider::{ActivatedProvider, ExternalProvider};

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
