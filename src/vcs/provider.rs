use std::{
    collections::{BTreeSet, HashSet},
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{
    discovery::Marker,
    protocol::{
        RequestOperation, ResponseOperation, WireCheckout, WireInspectResult, WireRequest,
        WireResponse, WireResult,
    },
    Capability, Checkout, ExactPath, InspectResult, ProviderFailure,
};

const PROTOCOL_VERSION: u32 = 1;
const MAX_BATCH_ITEMS: usize = 256;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_STATUS_TEXT_BYTES: usize = 4096;

#[derive(Debug, Clone)]
pub(crate) struct ExternalProvider {
    pub(super) id: String,
    pub(super) display_name: String,
    pub(super) command: Vec<String>,
    pub(super) priority: i32,
    pub(super) markers: Vec<Marker>,
    pub(super) allowed_capabilities: BTreeSet<Capability>,
    pub(super) status_timeout: Duration,
    pub(super) operation_timeout: Duration,
    pub(super) config_directory: PathBuf,
    pub(super) checkout_directory: Option<PathBuf>,
    pub(super) process_slots: Arc<tokio::sync::Semaphore>,
}

impl ExternalProvider {
    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn display_name(&self) -> &str {
        &self.display_name
    }

    pub(crate) fn priority(&self) -> i32 {
        self.priority
    }

    pub(crate) fn checkout_directory(&self) -> Option<&Path> {
        self.checkout_directory.as_deref()
    }

    pub(crate) async fn activate(&self) -> Result<ActivatedProvider, ProviderFailure> {
        match self
            .request(RequestOperation::Describe, self.status_timeout)
            .await?
        {
            ResponseOperation::Describe { capabilities } => {
                let effective_capabilities =
                    negotiate_capabilities(&self.allowed_capabilities, &capabilities);
                Ok(ActivatedProvider {
                    provider: self.clone(),
                    effective_capabilities,
                })
            }
            response => Err(ProviderFailure::Protocol(format!(
                "provider {} returned {} for describe",
                self.id,
                response.operation_name()
            ))),
        }
    }

    async fn request(
        &self,
        operation: RequestOperation,
        timeout: Duration,
    ) -> Result<ResponseOperation, ProviderFailure> {
        let deadline = tokio::time::Instant::now() + timeout;
        let _permit =
            tokio::time::timeout_at(deadline, Arc::clone(&self.process_slots).acquire_owned())
                .await
                .map_err(|_| ProviderFailure::Timeout(timeout))?
                .map_err(|_| {
                    ProviderFailure::Launch("provider process limiter is closed".into())
                })?;
        let request_id = next_request_id();
        let request = WireRequest {
            protocol_version: PROTOCOL_VERSION,
            request_id,
            operation,
        };
        let input = serde_json::to_vec(&request)
            .map_err(|error| ProviderFailure::Protocol(error.to_string()))?;
        if input.len() > MAX_REQUEST_BYTES {
            return Err(ProviderFailure::Protocol(format!(
                "request exceeds {MAX_REQUEST_BYTES} bytes"
            )));
        }

        let program = self.command.first().ok_or_else(|| {
            ProviderFailure::Launch(format!("provider {} has no command", self.id))
        })?;
        let resolved_program =
            crate::plugin_command::program_for_cwd(program, &self.config_directory);
        if is_batch_program(&resolved_program) {
            return Err(ProviderFailure::Launch(
                "Windows .cmd/.bat providers are not supported; configure a native executable"
                    .into(),
            ));
        }
        let mut command = crate::noninteractive_process::command(&resolved_program);
        command
            .args(&self.command[1..])
            .current_dir(&self.config_directory)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        crate::platform::configure_status_command(&mut command);

        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|error| ProviderFailure::Launch(error.to_string()))?;
        let _guard = crate::platform::StatusCommandGuard::new(&child)
            .map_err(|error| ProviderFailure::Launch(error.to_string()))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| ProviderFailure::Launch("provider stdin was not piped".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ProviderFailure::Launch("provider stdout was not piped".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ProviderFailure::Launch("provider stderr was not piped".into()))?;

        let operation = async move {
            stdin
                .write_all(&input)
                .await
                .map_err(|error| ProviderFailure::Io(error.to_string()))?;
            stdin
                .shutdown()
                .await
                .map_err(|error| ProviderFailure::Io(error.to_string()))?;
            drop(stdin);

            let read_stdout = read_capped(stdout, MAX_RESPONSE_BYTES);
            let read_stderr = read_capped(stderr, MAX_STDERR_BYTES);
            let (status, stdout, stderr) = tokio::join!(child.wait(), read_stdout, read_stderr);
            let status = status.map_err(|error| ProviderFailure::Io(error.to_string()))?;
            let stdout = stdout?;
            let stderr = stderr?;
            if !status.success() {
                return Err(ProviderFailure::Exit {
                    status: status.to_string(),
                    stderr: String::from_utf8_lossy(&stderr).trim().to_string(),
                });
            }
            Ok(stdout)
        };

        let stdout = tokio::time::timeout_at(deadline, operation)
            .await
            .map_err(|_| ProviderFailure::Timeout(timeout))??;
        let response: WireResponse = serde_json::from_slice(&stdout).map_err(|error| {
            ProviderFailure::Protocol(format!("invalid JSON response: {error}"))
        })?;
        if response.protocol_version != PROTOCOL_VERSION {
            return Err(ProviderFailure::Protocol(format!(
                "unsupported response protocol version {}",
                response.protocol_version
            )));
        }
        if response.request_id != request_id {
            return Err(ProviderFailure::Protocol(format!(
                "response request_id {} does not match {request_id}",
                response.request_id
            )));
        }
        match response.result {
            WireResult::Ok { response } => Ok(response),
            WireResult::Error { error } => Err(ProviderFailure::Provider(error)),
        }
    }
}

pub(super) fn negotiate_capabilities(
    allowed: &BTreeSet<Capability>,
    advertised: &[String],
) -> BTreeSet<Capability> {
    let advertised = advertised
        .iter()
        .filter_map(|value| Capability::parse(value))
        .collect::<BTreeSet<_>>();
    let mut effective = allowed
        .intersection(&advertised)
        .copied()
        .collect::<BTreeSet<_>>();
    if !effective.contains(&Capability::CheckoutList) {
        effective.remove(&Capability::CheckoutRemove);
        effective.remove(&Capability::CheckoutRemoveForce);
    }
    effective
}

fn validate_response_path(path: &ExactPath) -> Result<PathBuf, ProviderFailure> {
    let path = path.to_path_buf().map_err(ProviderFailure::Protocol)?;
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(ProviderFailure::Protocol(
            "provider returned a non-absolute path".into(),
        ))
    }
}

fn normalize_checkout(checkout: WireCheckout) -> Result<Checkout, ProviderFailure> {
    if checkout.id.is_empty() || checkout.id.len() > 512 {
        return Err(ProviderFailure::Protocol(
            "checkout id must contain 1-512 bytes".into(),
        ));
    }
    if checkout.name.is_empty() || checkout.name.len() > 256 {
        return Err(ProviderFailure::Protocol(
            "checkout name must contain 1-256 bytes".into(),
        ));
    }
    let path = validate_response_path(&checkout.path)?;
    Ok(Checkout {
        id: checkout.id,
        name: checkout.name,
        path,
        managed: checkout.managed,
    })
}

fn normalize_inspection(
    items: Vec<WireInspectResult>,
) -> Result<Vec<InspectResult>, ProviderFailure> {
    if items.len() > MAX_BATCH_ITEMS {
        return Err(ProviderFailure::Protocol(format!(
            "inspect response exceeds {MAX_BATCH_ITEMS} items"
        )));
    }
    let mut seen_roots = HashSet::new();
    let mut normalized = Vec::with_capacity(items.len());
    for item in items {
        let root = validate_response_path(&item.root)?;
        if !seen_roots.insert(root.clone()) {
            return Err(ProviderFailure::Protocol(
                "inspect response contains a duplicate root".into(),
            ));
        }
        if item
            .branch
            .as_ref()
            .is_some_and(|branch| branch.len() > MAX_STATUS_TEXT_BYTES)
        {
            return Err(ProviderFailure::Protocol(format!(
                "inspect branch exceeds {MAX_STATUS_TEXT_BYTES} bytes"
            )));
        }
        normalized.push(InspectResult {
            root,
            branch: item.branch,
            ahead: item.ahead,
            behind: item.behind,
        });
    }
    Ok(normalized)
}

fn normalize_checkouts(checkouts: Vec<WireCheckout>) -> Result<Vec<Checkout>, ProviderFailure> {
    if checkouts.len() > MAX_BATCH_ITEMS {
        return Err(ProviderFailure::Protocol(format!(
            "checkout.list response exceeds {MAX_BATCH_ITEMS} items"
        )));
    }
    let checkouts = checkouts
        .into_iter()
        .map(normalize_checkout)
        .collect::<Result<Vec<_>, _>>()?;
    let mut seen_ids = HashSet::new();
    let mut seen_paths = HashSet::new();
    for checkout in &checkouts {
        if !seen_ids.insert(&checkout.id) {
            return Err(ProviderFailure::Protocol(
                "checkout.list response contains a duplicate checkout id".into(),
            ));
        }
        if !seen_paths.insert(&checkout.path) {
            return Err(ProviderFailure::Protocol(
                "checkout.list response contains a duplicate checkout path".into(),
            ));
        }
    }
    Ok(checkouts)
}

async fn read_capped<R>(reader: R, limit: usize) -> Result<Vec<u8>, ProviderFailure>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let cap = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    let mut bytes = Vec::new();
    reader
        .take(cap)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| ProviderFailure::Io(error.to_string()))?;
    if bytes.len() > limit {
        return Err(ProviderFailure::Protocol(format!(
            "provider output exceeds {limit} bytes"
        )));
    }
    Ok(bytes)
}

pub(super) fn next_request_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
    NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

fn is_batch_program(path: &Path) -> bool {
    cfg!(windows)
        && path
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|extension| {
                extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
            })
}

#[derive(Debug, Clone)]
pub(crate) struct ActivatedProvider {
    provider: ExternalProvider,
    effective_capabilities: BTreeSet<Capability>,
}

impl ActivatedProvider {
    pub(crate) fn id(&self) -> &str {
        self.provider.id()
    }

    pub(crate) fn display_name(&self) -> &str {
        self.provider.display_name()
    }

    pub(crate) fn checkout_directory(&self) -> Option<&Path> {
        self.provider.checkout_directory()
    }

    pub(crate) fn capabilities(&self) -> BTreeSet<Capability> {
        self.effective_capabilities.clone()
    }

    pub(crate) fn supports(&self, capability: Capability) -> bool {
        self.effective_capabilities.contains(&capability)
    }

    pub(crate) async fn inspect(
        &self,
        roots: Vec<ExactPath>,
    ) -> Result<Vec<InspectResult>, ProviderFailure> {
        self.require_effective(Capability::Inspect)?;
        if roots.len() > MAX_BATCH_ITEMS {
            return Err(ProviderFailure::Protocol(format!(
                "inspect batch exceeds {MAX_BATCH_ITEMS} items"
            )));
        }
        for root in &roots {
            validate_response_path(root)?;
        }
        match self
            .provider
            .request(
                RequestOperation::Inspect { roots },
                self.provider.status_timeout,
            )
            .await?
        {
            ResponseOperation::Inspect { items } => normalize_inspection(items),
            response => Err(ProviderFailure::Protocol(format!(
                "provider {} returned {} for inspect",
                self.id(),
                response.operation_name()
            ))),
        }
    }

    pub(crate) async fn checkout_list(
        &self,
        root: ExactPath,
    ) -> Result<Vec<Checkout>, ProviderFailure> {
        self.require_effective(Capability::CheckoutList)?;
        validate_response_path(&root)?;
        match self
            .provider
            .request(
                RequestOperation::CheckoutList { root },
                self.provider.operation_timeout,
            )
            .await?
        {
            ResponseOperation::CheckoutList { checkouts } => normalize_checkouts(checkouts),
            response => Err(ProviderFailure::Protocol(format!(
                "provider {} returned {} for checkout.list",
                self.id(),
                response.operation_name()
            ))),
        }
    }

    pub(crate) async fn checkout_create(
        &self,
        root: ExactPath,
        name: String,
        destination: ExactPath,
    ) -> Result<Checkout, ProviderFailure> {
        self.require_effective(Capability::CheckoutCreate)?;
        validate_response_path(&root)?;
        validate_response_path(&destination)?;
        if name.is_empty() || name.len() > 256 {
            return Err(ProviderFailure::Protocol(
                "checkout name must contain 1-256 bytes".into(),
            ));
        }
        match self
            .provider
            .request(
                RequestOperation::CheckoutCreate {
                    root,
                    name,
                    destination,
                },
                self.provider.operation_timeout,
            )
            .await?
        {
            ResponseOperation::CheckoutCreate { checkout } => normalize_checkout(checkout),
            response => Err(ProviderFailure::Protocol(format!(
                "provider {} returned {} for checkout.create",
                self.id(),
                response.operation_name()
            ))),
        }
    }

    pub(crate) async fn checkout_remove(
        &self,
        root: ExactPath,
        checkout: ExactPath,
        force: bool,
    ) -> Result<(), ProviderFailure> {
        if force {
            self.require_effective(Capability::CheckoutRemove)?;
        }
        self.require_effective(if force {
            Capability::CheckoutRemoveForce
        } else {
            Capability::CheckoutRemove
        })?;
        validate_response_path(&root)?;
        validate_response_path(&checkout)?;
        match self
            .provider
            .request(
                RequestOperation::CheckoutRemove {
                    root,
                    checkout,
                    force,
                },
                self.provider.operation_timeout,
            )
            .await?
        {
            ResponseOperation::CheckoutRemove {} => Ok(()),
            response => Err(ProviderFailure::Protocol(format!(
                "provider {} returned {} for checkout.remove",
                self.id(),
                response.operation_name()
            ))),
        }
    }

    fn require_effective(&self, capability: Capability) -> Result<(), ProviderFailure> {
        if self.supports(capability) {
            Ok(())
        } else {
            Err(ProviderFailure::Protocol(format!(
                "provider {} did not negotiate capability {:?}",
                self.provider.id(),
                capability.as_str()
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_rejects_ambiguous_or_invalid_provider_paths() {
        let root = std::env::temp_dir().join("herdr-provider-normalization");
        let checkout = |id: &str, path: &Path| WireCheckout {
            id: id.into(),
            name: "topic".into(),
            path: ExactPath::from_path(path),
            managed: true,
        };
        let normalized = normalize_checkout(checkout("one", &root)).unwrap();
        assert_eq!(normalized.path, root);
        assert!(normalize_checkout(checkout("one", Path::new("relative"))).is_err());
        assert!(normalize_checkouts(vec![checkout("one", &root), checkout("two", &root)]).is_err());
        assert!(normalize_checkouts(vec![
            checkout("one", &root),
            checkout("one", &root.join("other"))
        ])
        .is_err());
        let inspection = || WireInspectResult {
            root: ExactPath::from_path(&root),
            branch: Some("topic".into()),
            ahead: Some(2),
            behind: Some(3),
        };
        assert!(normalize_inspection(vec![inspection(), inspection()]).is_err());
        let normalized = normalize_inspection(vec![inspection()]).unwrap().remove(0);
        assert_eq!(normalized.root, root);
        assert_eq!(
            (
                normalized.branch.as_deref(),
                normalized.ahead,
                normalized.behind
            ),
            (Some("topic"), Some(2), Some(3))
        );
    }

    fn activated(allowed: &[Capability], advertised: &[&str]) -> ActivatedProvider {
        let allowed_capabilities = allowed.iter().copied().collect();
        let effective_capabilities = negotiate_capabilities(
            &allowed_capabilities,
            &advertised
                .iter()
                .map(|value| (*value).to_owned())
                .collect::<Vec<_>>(),
        );
        ActivatedProvider {
            provider: ExternalProvider {
                id: "test".into(),
                display_name: "Test".into(),
                command: vec!["herdr-test-provider-does-not-exist".into()],
                priority: 0,
                markers: Vec::new(),
                allowed_capabilities,
                status_timeout: Duration::from_secs(1),
                operation_timeout: Duration::from_secs(1),
                config_directory: std::env::temp_dir(),
                checkout_directory: None,
                process_slots: Arc::new(tokio::sync::Semaphore::new(1)),
            },
            effective_capabilities,
        }
    }

    fn assert_capability_denied(result: Result<(), ProviderFailure>) {
        assert!(
            matches!(result, Err(ProviderFailure::Protocol(ref message))
            if message.contains("did not negotiate capability")),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn operations_require_both_advertisement_and_configuration_before_launch() {
        let all = [
            Capability::Inspect,
            Capability::CheckoutList,
            Capability::CheckoutCreate,
            Capability::CheckoutRemove,
            Capability::CheckoutRemoveForce,
        ];
        let advertised = all.map(Capability::as_str);
        // Each side of negotiation must independently be able to deny execution.
        for provider in [activated(&all, &[]), activated(&[], &advertised)] {
            let root = ExactPath::from_path(&std::env::temp_dir());
            for result in [
                provider.inspect(vec![root.clone()]).await.map(|_| ()),
                provider.checkout_list(root.clone()).await.map(|_| ()),
                provider
                    .checkout_create(root.clone(), "topic".into(), root.clone())
                    .await
                    .map(|_| ()),
                provider
                    .checkout_remove(root.clone(), root.clone(), false)
                    .await,
                provider.checkout_remove(root.clone(), root, true).await,
            ] {
                assert_capability_denied(result);
            }
        }
    }

    #[tokio::test]
    async fn force_remove_requires_both_remove_capabilities_before_launch() {
        let allowed = [
            Capability::CheckoutList,
            Capability::CheckoutRemove,
            Capability::CheckoutRemoveForce,
        ];
        let root = ExactPath::from_path(&std::env::temp_dir());
        for advertised in [
            ["checkout.list", "checkout.remove"],
            ["checkout.list", "checkout.remove.force"],
        ] {
            let provider = activated(&allowed, &advertised);
            assert_capability_denied(
                provider
                    .checkout_remove(root.clone(), root.clone(), true)
                    .await,
            );
        }
    }
}
