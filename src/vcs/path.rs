use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde::{Deserialize, Serialize};

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
