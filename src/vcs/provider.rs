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
    protocol::{RequestOperation, ResponseOperation, WireRequest, WireResponse, WireResult},
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

    async fn inspect(&self, roots: Vec<ExactPath>) -> Result<Vec<InspectResult>, ProviderFailure> {
        self.require_allowed(Capability::Inspect)?;
        if roots.len() > MAX_BATCH_ITEMS {
            return Err(ProviderFailure::Protocol(format!(
                "inspect batch exceeds {MAX_BATCH_ITEMS} items"
            )));
        }
        for root in &roots {
            validate_response_path(root)?;
        }
        match self
            .request(RequestOperation::Inspect { roots }, self.status_timeout)
            .await?
        {
            ResponseOperation::Inspect { items } => {
                if items.len() > MAX_BATCH_ITEMS {
                    return Err(ProviderFailure::Protocol(format!(
                        "inspect response exceeds {MAX_BATCH_ITEMS} items"
                    )));
                }
                let mut seen_roots = HashSet::new();
                for item in &items {
                    let root = validate_response_path(&item.root)?;
                    if !seen_roots.insert(root) {
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
                }
                Ok(items)
            }
            response => Err(ProviderFailure::Protocol(format!(
                "provider {} returned {} for inspect",
                self.id,
                response.operation_name()
            ))),
        }
    }

    async fn checkout_list(&self, root: ExactPath) -> Result<Vec<Checkout>, ProviderFailure> {
        self.require_allowed(Capability::CheckoutList)?;
        validate_response_path(&root)?;
        match self
            .request(
                RequestOperation::CheckoutList { root },
                self.operation_timeout,
            )
            .await?
        {
            ResponseOperation::CheckoutList { checkouts } => {
                if checkouts.len() > MAX_BATCH_ITEMS {
                    return Err(ProviderFailure::Protocol(format!(
                        "checkout.list response exceeds {MAX_BATCH_ITEMS} items"
                    )));
                }
                let mut seen_ids = HashSet::new();
                let mut seen_paths = HashSet::new();
                for checkout in &checkouts {
                    let path = validate_checkout(checkout)?;
                    if !seen_ids.insert(&checkout.id) {
                        return Err(ProviderFailure::Protocol(
                            "checkout.list response contains a duplicate checkout id".into(),
                        ));
                    }
                    if !seen_paths.insert(path) {
                        return Err(ProviderFailure::Protocol(
                            "checkout.list response contains a duplicate checkout path".into(),
                        ));
                    }
                }
                Ok(checkouts)
            }
            response => Err(ProviderFailure::Protocol(format!(
                "provider {} returned {} for checkout.list",
                self.id,
                response.operation_name()
            ))),
        }
    }

    async fn checkout_create(
        &self,
        root: ExactPath,
        name: String,
        destination: ExactPath,
    ) -> Result<Checkout, ProviderFailure> {
        self.require_allowed(Capability::CheckoutCreate)?;
        validate_response_path(&root)?;
        validate_response_path(&destination)?;
        if name.is_empty() || name.len() > 256 {
            return Err(ProviderFailure::Protocol(
                "checkout name must contain 1-256 bytes".into(),
            ));
        }
        match self
            .request(
                RequestOperation::CheckoutCreate {
                    root,
                    name,
                    destination,
                },
                self.operation_timeout,
            )
            .await?
        {
            ResponseOperation::CheckoutCreate { checkout } => {
                validate_checkout(&checkout)?;
                Ok(checkout)
            }
            response => Err(ProviderFailure::Protocol(format!(
                "provider {} returned {} for checkout.create",
                self.id,
                response.operation_name()
            ))),
        }
    }

    async fn checkout_remove(
        &self,
        root: ExactPath,
        checkout: ExactPath,
        force: bool,
    ) -> Result<(), ProviderFailure> {
        validate_response_path(&root)?;
        validate_response_path(&checkout)?;
        if force {
            self.require_allowed(Capability::CheckoutRemove)?;
        }
        self.require_allowed(if force {
            Capability::CheckoutRemoveForce
        } else {
            Capability::CheckoutRemove
        })?;
        match self
            .request(
                RequestOperation::CheckoutRemove {
                    root,
                    checkout,
                    force,
                },
                self.operation_timeout,
            )
            .await?
        {
            ResponseOperation::CheckoutRemove {} => Ok(()),
            response => Err(ProviderFailure::Protocol(format!(
                "provider {} returned {} for checkout.remove",
                self.id,
                response.operation_name()
            ))),
        }
    }

    fn require_allowed(&self, capability: Capability) -> Result<(), ProviderFailure> {
        if self.allowed_capabilities.contains(&capability) {
            Ok(())
        } else {
            Err(ProviderFailure::Protocol(format!(
                "capability {:?} is not allowed for provider {}",
                capability.as_str(),
                self.id
            )))
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

fn validate_checkout(checkout: &Checkout) -> Result<PathBuf, ProviderFailure> {
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
    validate_response_path(&checkout.path)
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
    pub(crate) effective_capabilities: BTreeSet<Capability>,
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
        self.provider.inspect(roots).await
    }

    pub(crate) async fn checkout_list(
        &self,
        root: ExactPath,
    ) -> Result<Vec<Checkout>, ProviderFailure> {
        self.require_effective(Capability::CheckoutList)?;
        self.provider.checkout_list(root).await
    }

    pub(crate) async fn checkout_create(
        &self,
        root: ExactPath,
        name: String,
        destination: ExactPath,
    ) -> Result<Checkout, ProviderFailure> {
        self.require_effective(Capability::CheckoutCreate)?;
        self.provider.checkout_create(root, name, destination).await
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
        self.provider.checkout_remove(root, checkout, force).await
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
