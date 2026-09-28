//! Version-control provider boundary.
//!
//! Git remains built in. External providers normalize their native CLI and data
//! model to this protocol, so Herdr never needs to know a proprietary VCS's
//! command line or output format.

use std::{
    collections::{BTreeSet, HashSet},
    ffi::OsStr,
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[cfg(test)]
use crate::config::VcsProviderConfig;
use crate::config::{VcsConfig, VcsDiscoveryMarkerConfig, VcsDiscoveryMarkerKind};

pub(crate) const BUILTIN_GIT_PROVIDER_ID: &str = "builtin.git";
const PROTOCOL_NAME: &str = "stdio-json-v1";
const PROTOCOL_VERSION: u32 = 1;
const MAX_PROVIDERS: usize = 32;
const MAX_MARKERS_PER_PROVIDER: usize = 8;
const MAX_COMMAND_ARGS: usize = 64;
const MAX_BATCH_ITEMS: usize = 256;
const MAX_CONCURRENT_PROVIDER_PROCESSES: usize = 4;
const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_STATUS_TEXT_BYTES: usize = 4096;
const MIN_TIMEOUT_MS: u64 = 50;
const MAX_STATUS_TIMEOUT_MS: u64 = 30_000;
const MAX_OPERATION_TIMEOUT_MS: u64 = 30 * 60 * 1_000;

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

#[derive(Debug, Clone)]
struct Marker {
    path: PathBuf,
    kind: VcsDiscoveryMarkerKind,
}

#[derive(Debug, Clone)]
pub(crate) struct ExternalProvider {
    id: String,
    display_name: String,
    command: Vec<String>,
    priority: i32,
    markers: Vec<Marker>,
    allowed_capabilities: BTreeSet<Capability>,
    status_timeout: Duration,
    operation_timeout: Duration,
    config_directory: PathBuf,
    checkout_directory: Option<PathBuf>,
    process_slots: Arc<tokio::sync::Semaphore>,
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

fn negotiate_capabilities(
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

/// Stable, lossless identity for grouping repositories in projections and
/// persisted checkout membership. The tag avoids ambiguity between native path
/// encodings while base64 keeps arbitrary UTF-8 provider IDs and paths safe.
pub(crate) fn repository_key(provider_id: &str, root: &Path) -> String {
    let provider = base64::engine::general_purpose::STANDARD.encode(provider_id.as_bytes());
    let (encoding, value) = match ExactPath::from_path(root) {
        ExactPath::Utf8(value) => (
            "utf8",
            base64::engine::general_purpose::STANDARD.encode(value.as_bytes()),
        ),
        ExactPath::UnixBytesBase64(value) => ("unix", value),
        ExactPath::WindowsUtf16LeBase64(value) => ("windows", value),
    };
    format!("{provider}:{encoding}:{value}")
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

fn next_request_id() -> u64 {
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

#[derive(Debug, Clone)]
pub(crate) struct Registry {
    generation: u64,
    providers: Vec<ExternalProvider>,
}

impl Registry {
    pub(crate) fn from_config(config: &VcsConfig, generation: u64) -> Result<Self, Vec<String>> {
        let diagnostics = validate_config(config);
        if !diagnostics.is_empty() {
            return Err(diagnostics);
        }
        let config_directory = crate::config::config_path()
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let config_directory = absolute_path(config_directory)?;
        let process_slots = Arc::new(tokio::sync::Semaphore::new(
            MAX_CONCURRENT_PROVIDER_PROCESSES,
        ));
        let providers = config
            .providers
            .iter()
            .filter(|provider| platform_enabled(&provider.platforms))
            .map(|provider| ExternalProvider {
                id: provider.id.clone(),
                display_name: provider.display_name.clone(),
                command: provider.command.clone(),
                priority: provider.priority,
                markers: provider
                    .discovery
                    .iter()
                    .map(|marker| Marker {
                        path: PathBuf::from(&marker.path),
                        kind: marker.kind,
                    })
                    .collect(),
                allowed_capabilities: provider
                    .allow
                    .iter()
                    .filter_map(|value| Capability::parse(value))
                    .collect(),
                status_timeout: Duration::from_millis(provider.status_timeout_ms),
                operation_timeout: Duration::from_millis(provider.operation_timeout_ms),
                config_directory: config_directory.clone(),
                checkout_directory: provider
                    .checkout_directory
                    .as_ref()
                    .map(|value| resolve_config_path(value, &config_directory)),
                process_slots: Arc::clone(&process_slots),
            })
            .collect();
        Ok(Self {
            generation,
            providers,
        })
    }

    pub(crate) fn empty(generation: u64) -> Self {
        Self {
            generation,
            providers: Vec::new(),
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn providers(&self) -> &[ExternalProvider] {
        &self.providers
    }

    pub(crate) fn provider(&self, id: &str) -> Option<&ExternalProvider> {
        self.providers.iter().find(|provider| provider.id() == id)
    }

    /// Discover a configured external provider without executing it.
    ///
    /// The deepest matching repository root wins, then provider priority. An
    /// exact tie is rejected instead of depending on config ordering.
    pub(crate) fn discover(&self, cwd: &Path) -> Result<Option<DiscoveredRepository<'_>>, String> {
        let cwd = if cwd.is_absolute() {
            cwd.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|error| format!("failed to resolve VCS discovery directory: {error}"))?
                .join(cwd)
        };
        let mut best: Option<DiscoveredRepository<'_>> = None;
        let mut tied = false;
        for root in cwd.ancestors() {
            for provider in &self.providers {
                if !provider
                    .markers
                    .iter()
                    .any(|marker| marker_matches(root, marker))
                {
                    continue;
                }
                let candidate = DiscoveredRepository {
                    provider,
                    root: root.to_path_buf(),
                };
                match &best {
                    None => {
                        best = Some(candidate);
                        tied = false;
                    }
                    Some(current) => {
                        let current_depth = current.root.components().count();
                        let candidate_depth = candidate.root.components().count();
                        let candidate_rank = (candidate_depth, candidate.provider.priority());
                        let current_rank = (current_depth, current.provider.priority());
                        if candidate_rank > current_rank {
                            best = Some(candidate);
                            tied = false;
                        } else if candidate_rank == current_rank
                            && candidate.provider.id() != current.provider.id()
                        {
                            tied = true;
                        }
                    }
                }
            }
        }
        if tied {
            let selected = best.as_ref().expect("a tie requires a selected provider");
            Err(format!(
                "multiple VCS providers match {} with priority {}",
                selected.root.display(),
                selected.provider.priority()
            ))
        } else {
            Ok(best)
        }
    }
}

fn absolute_path(path: PathBuf) -> Result<PathBuf, Vec<String>> {
    if path.is_absolute() {
        Ok(path)
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|error| vec![format!("failed to resolve config directory: {error}")])
    }
}

#[derive(Debug)]
pub(crate) struct DiscoveredRepository<'a> {
    pub(crate) provider: &'a ExternalProvider,
    pub(crate) root: PathBuf,
}

fn marker_matches(root: &Path, marker: &Marker) -> bool {
    let candidate = root.join(&marker.path);
    match marker.kind {
        VcsDiscoveryMarkerKind::File => candidate.is_file(),
        VcsDiscoveryMarkerKind::Directory => candidate.is_dir(),
        VcsDiscoveryMarkerKind::Any => candidate.exists(),
    }
}

fn platform_enabled(platforms: &[String]) -> bool {
    platforms.is_empty()
        || platforms.iter().any(|platform| match platform.as_str() {
            "linux" => cfg!(target_os = "linux"),
            "macos" => cfg!(target_os = "macos"),
            "windows" => cfg!(windows),
            _ => false,
        })
}

pub(crate) fn validate_config(config: &VcsConfig) -> Vec<String> {
    let mut diagnostics = Vec::new();
    if config.providers.len() > MAX_PROVIDERS {
        diagnostics.push(format!(
            "at most {MAX_PROVIDERS} vcs.providers entries are supported"
        ));
    }
    let mut ids = HashSet::new();
    for (index, provider) in config.providers.iter().enumerate() {
        let label = if provider.id.is_empty() {
            format!("vcs.providers[{index}]")
        } else {
            format!("vcs provider {:?}", provider.id)
        };
        if !valid_provider_id(&provider.id) {
            diagnostics.push(format!(
                "{label} id must be 1-64 lowercase ASCII letters, digits, '.', '-', or '_'"
            ));
        } else if provider.id == BUILTIN_GIT_PROVIDER_ID {
            diagnostics.push(format!("{label} uses the reserved built-in provider id"));
        } else if !ids.insert(provider.id.as_str()) {
            diagnostics.push(format!("duplicate VCS provider id {:?}", provider.id));
        }
        if provider.display_name.trim().is_empty() || provider.display_name.len() > 80 {
            diagnostics.push(format!("{label} display_name must contain 1-80 bytes"));
        }
        if provider.protocol != PROTOCOL_NAME {
            diagnostics.push(format!("{label} protocol must be {PROTOCOL_NAME:?}"));
        }
        if provider.command.is_empty() || provider.command.len() > MAX_COMMAND_ARGS {
            diagnostics.push(format!(
                "{label} command must contain 1-{MAX_COMMAND_ARGS} argv entries"
            ));
        } else if provider.command.iter().any(|part| part.is_empty()) {
            diagnostics.push(format!("{label} command entries must not be empty"));
        } else if provider.command.iter().any(|part| part.len() > 16 * 1024)
            || provider.command.iter().map(String::len).sum::<usize>() > 64 * 1024
        {
            diagnostics.push(format!(
                "{label} command entries must be at most 16 KiB each and 64 KiB total"
            ));
        }
        if provider.discovery.is_empty() || provider.discovery.len() > MAX_MARKERS_PER_PROVIDER {
            diagnostics.push(format!(
                "{label} discovery must contain 1-{MAX_MARKERS_PER_PROVIDER} markers"
            ));
        }
        for marker in &provider.discovery {
            if let Some(message) = invalid_marker(marker) {
                diagnostics.push(format!(
                    "{label} discovery path {:?} {message}",
                    marker.path
                ));
            }
        }
        for platform in &provider.platforms {
            if !matches!(platform.as_str(), "linux" | "macos" | "windows") {
                diagnostics.push(format!("{label} has unsupported platform {platform:?}"));
            }
        }
        for capability in &provider.allow {
            if Capability::parse(capability).is_none() {
                diagnostics.push(format!(
                    "{label} has unsupported allowed capability {capability:?}"
                ));
            }
        }
        if provider
            .allow
            .iter()
            .any(|capability| capability == "checkout.remove.force")
            && !provider
                .allow
                .iter()
                .any(|capability| capability == "checkout.remove")
        {
            diagnostics.push(format!(
                "{label} allows checkout.remove.force without checkout.remove"
            ));
        }
        if provider
            .allow
            .iter()
            .any(|capability| capability == "checkout.remove")
            && !provider
                .allow
                .iter()
                .any(|capability| capability == "checkout.list")
        {
            diagnostics.push(format!(
                "{label} allows checkout.remove without checkout.list"
            ));
        }
        if !(MIN_TIMEOUT_MS..=MAX_STATUS_TIMEOUT_MS).contains(&provider.status_timeout_ms) {
            diagnostics.push(format!(
                "{label} status_timeout_ms must be between {MIN_TIMEOUT_MS} and {MAX_STATUS_TIMEOUT_MS}"
            ));
        }
        if !(MIN_TIMEOUT_MS..=MAX_OPERATION_TIMEOUT_MS).contains(&provider.operation_timeout_ms) {
            diagnostics.push(format!(
                "{label} operation_timeout_ms must be between {MIN_TIMEOUT_MS} and {MAX_OPERATION_TIMEOUT_MS}"
            ));
        }
    }
    diagnostics
}

fn resolve_config_path(value: &str, config_directory: &Path) -> PathBuf {
    if value == "~" || value.starts_with("~/") || value.starts_with("~\\") {
        return crate::worktree::expand_tilde_absolute_path(value);
    }
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        config_directory.join(path)
    }
}

fn valid_provider_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
}

fn invalid_marker(marker: &VcsDiscoveryMarkerConfig) -> Option<&'static str> {
    if marker.path.is_empty() || marker.path.len() > 256 {
        return Some("must contain 1-256 bytes");
    }
    let path = Path::new(&marker.path);
    if path.components().count() > 16 {
        return Some("must contain at most 16 components");
    }
    if path.components().any(|component| {
        !matches!(component, Component::Normal(_))
            || component
                .as_os_str()
                .to_string_lossy()
                .contains(['*', '?', '[', ']'])
    }) {
        return Some(
            "must be a relative literal path without '.', '..', roots, prefixes, or globs",
        );
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "encoding", content = "value", rename_all = "snake_case")]
pub(crate) enum ExactPath {
    Utf8(String),
    UnixBytesBase64(String),
    WindowsUtf16LeBase64(String),
}

impl ExactPath {
    pub(crate) fn from_path(path: &Path) -> Self {
        if let Some(value) = path.to_str() {
            return Self::Utf8(value.to_string());
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            return Self::UnixBytesBase64(
                base64::engine::general_purpose::STANDARD.encode(path.as_os_str().as_bytes()),
            );
        }
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStrExt;
            let bytes = path
                .as_os_str()
                .encode_wide()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>();
            return Self::WindowsUtf16LeBase64(
                base64::engine::general_purpose::STANDARD.encode(bytes),
            );
        }
        #[allow(unreachable_code)]
        Self::Utf8(path.to_string_lossy().into_owned())
    }

    pub(crate) fn to_path_buf(&self) -> Result<PathBuf, String> {
        match self {
            Self::Utf8(value) => {
                if value.contains('\0') {
                    Err("path contains NUL".into())
                } else {
                    Ok(PathBuf::from(value))
                }
            }
            Self::UnixBytesBase64(value) => decode_unix_path(value),
            Self::WindowsUtf16LeBase64(value) => decode_windows_path(value),
        }
    }
}

#[cfg(unix)]
fn decode_unix_path(value: &str) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;
    let bytes = decode_path_bytes(value)?;
    if bytes.contains(&0) {
        return Err("path contains NUL".into());
    }
    Ok(std::ffi::OsString::from_vec(bytes).into())
}

#[cfg(not(unix))]
fn decode_unix_path(_value: &str) -> Result<PathBuf, String> {
    Err("unix byte paths are not native on this platform".into())
}

#[cfg(windows)]
fn decode_windows_path(value: &str) -> Result<PathBuf, String> {
    use std::os::windows::ffi::OsStringExt;
    let bytes = decode_path_bytes(value)?;
    if bytes.len() % 2 != 0 {
        return Err("UTF-16LE path has an odd byte length".into());
    }
    let units = bytes
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();
    if units.contains(&0) {
        return Err("path contains NUL".into());
    }
    Ok(std::ffi::OsString::from_wide(&units).into())
}

#[cfg(not(windows))]
fn decode_windows_path(_value: &str) -> Result<PathBuf, String> {
    Err("Windows UTF-16 paths are not native on this platform".into())
}

fn decode_path_bytes(value: &str) -> Result<Vec<u8>, String> {
    if value.len() > 128 * 1024 {
        return Err("encoded path is too large".into());
    }
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|error| format!("invalid base64 path: {error}"))
}

#[derive(Debug, Clone, Serialize)]
struct WireRequest {
    protocol_version: u32,
    request_id: u64,
    #[serde(flatten)]
    operation: RequestOperation,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "operation")]
enum RequestOperation {
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
struct WireResponse {
    protocol_version: u32,
    request_id: u64,
    #[serde(flatten)]
    result: WireResult,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum WireResult {
    Ok { response: ResponseOperation },
    Error { error: ProviderError },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "operation")]
enum ResponseOperation {
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
    fn operation_name(&self) -> &'static str {
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

#[cfg(test)]
mod tests {
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
        std::fs::set_permissions(&executable, permissions)
            .expect("make provider fixture executable");

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
}
