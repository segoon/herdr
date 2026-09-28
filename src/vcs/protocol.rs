use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::ExactPath;

#[derive(Debug, Clone, Serialize)]
pub(super) struct WireRequest {
    pub(super) protocol_version: u32,
    pub(super) request_id: u64,
    #[serde(flatten)]
    pub(super) operation: RequestOperation,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "operation")]
pub(super) enum RequestOperation {
    #[serde(rename = "describe")]
    Describe,
    #[serde(rename = "inspect")]
    Inspect { roots: Vec<ExactPath> },
    #[serde(rename = "checkout.list")]
    CheckoutList { root: ExactPath },
    #[serde(rename = "checkout.create")]
    CheckoutCreate {
        root: ExactPath,
        name: String,
        destination: ExactPath,
    },
    #[serde(rename = "checkout.remove")]
    CheckoutRemove {
        root: ExactPath,
        checkout: ExactPath,
        force: bool,
    },
}

#[derive(Debug, Deserialize)]
pub(super) struct WireResponse {
    pub(super) protocol_version: u32,
    pub(super) request_id: u64,
    #[serde(flatten)]
    pub(super) result: WireResult,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(super) enum WireResult {
    Ok { response: ResponseOperation },
    Error { error: ProviderError },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "operation")]
pub(super) enum ResponseOperation {
    #[serde(rename = "describe")]
    Describe { capabilities: Vec<String> },
    #[serde(rename = "inspect")]
    Inspect { items: Vec<InspectResult> },
    #[serde(rename = "checkout.list")]
    CheckoutList { checkouts: Vec<Checkout> },
    #[serde(rename = "checkout.create")]
    CheckoutCreate { checkout: Checkout },
    #[serde(rename = "checkout.remove")]
    CheckoutRemove {},
}

impl ResponseOperation {
    pub(super) fn operation_name(&self) -> &'static str {
        match self {
            Self::Describe { .. } => "describe",
            Self::Inspect { .. } => "inspect",
            Self::CheckoutList { .. } => "checkout.list",
            Self::CheckoutCreate { .. } => "checkout.create",
            Self::CheckoutRemove { .. } => "checkout.remove",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct InspectResult {
    pub(crate) root: ExactPath,
    #[serde(default)]
    pub(crate) branch: Option<String>,
    #[serde(default)]
    pub(crate) ahead: Option<u64>,
    #[serde(default)]
    pub(crate) behind: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Checkout {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) path: ExactPath,
    #[serde(default)]
    pub(crate) managed: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ProviderError {
    pub(crate) code: String,
    pub(crate) message: String,
    #[serde(default)]
    pub(crate) retryable: bool,
}

#[derive(Debug)]
pub(crate) enum ProviderFailure {
    Launch(String),
    Io(String),
    Timeout(Duration),
    Exit { status: String, stderr: String },
    Protocol(String),
    Provider(ProviderError),
}

impl std::fmt::Display for ProviderFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Launch(message) => write!(formatter, "provider launch failed: {message}"),
            Self::Io(message) => write!(formatter, "provider I/O failed: {message}"),
            Self::Timeout(timeout) => write!(formatter, "provider timed out after {timeout:?}"),
            Self::Exit { status, stderr } if stderr.is_empty() => {
                write!(formatter, "provider exited with {status}")
            }
            Self::Exit { status, stderr } => {
                write!(formatter, "provider exited with {status}: {stderr}")
            }
            Self::Protocol(message) => write!(formatter, "provider protocol error: {message}"),
            Self::Provider(error) => write!(formatter, "{}: {}", error.code, error.message),
        }
    }
}

impl ProviderFailure {
    pub(crate) fn retryable(&self) -> bool {
        match self {
            Self::Provider(error) => error.retryable,
            Self::Protocol(_) => false,
            Self::Launch(_) | Self::Io(_) | Self::Timeout(_) | Self::Exit { .. } => true,
        }
    }
}
