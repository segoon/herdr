use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use crate::config::{VcsConfig, VcsDiscoveryMarkerConfig, VcsDiscoveryMarkerKind};

use super::{Capability, ExternalProvider, BUILTIN_GIT_PROVIDER_ID};

const PROTOCOL_NAME: &str = "stdio-json-v1";
const MAX_PROVIDERS: usize = 32;
const MAX_MARKERS_PER_PROVIDER: usize = 8;
const MAX_COMMAND_ARGS: usize = 64;
const MAX_CONCURRENT_PROVIDER_PROCESSES: usize = 4;
const MIN_TIMEOUT_MS: u64 = 50;
const MAX_STATUS_TIMEOUT_MS: u64 = 30_000;
const MAX_OPERATION_TIMEOUT_MS: u64 = 30 * 60 * 1_000;

#[derive(Debug, Clone)]
pub(super) struct Marker {
    pub(super) path: PathBuf,
    pub(super) kind: VcsDiscoveryMarkerKind,
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
