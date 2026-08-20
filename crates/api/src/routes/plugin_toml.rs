//! Shared utilities for reading/writing plugin.toml files.
//!
//! Used by `bots.rs` and `weixin.rs` for thread-safe, atomic TOML manipulation.

use fs4::fs_std::FileExt;
use types::error::{CarrierError, CarrierResult};

/// Atomic file write: write to `<path>.tmp` then rename over target.
pub fn atomic_write(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    let tmp_path = {
        let mut s = path.as_os_str().to_owned();
        s.push(".tmp");
        std::path::PathBuf::from(s)
    };
    std::fs::write(&tmp_path, content)?;
    std::fs::rename(&tmp_path, path)
}

/// Maximum length for config string fields (corp_id, secret, etc.).
pub const CHANNEL_FIELD_MAX_LEN: usize = 512;

/// Validate a config string field: non-empty after trim, max length, no control chars.
pub fn channel_validate_field(value: &str, field_name: &str) -> CarrierResult<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(CarrierError::InvalidInput(format!(
            "{field_name} is required"
        )));
    }
    if trimmed.len() > CHANNEL_FIELD_MAX_LEN {
        return Err(CarrierError::InvalidInput(format!(
            "{field_name} exceeds max length ({CHANNEL_FIELD_MAX_LEN} chars)"
        )));
    }
    if trimmed.chars().any(|c| c.is_control() && c != ' ') {
        return Err(CarrierError::InvalidInput(format!(
            "{field_name} contains invalid characters"
        )));
    }
    Ok(trimmed.to_string())
}

/// Sanitize tenant name for plugin.toml entries.
pub fn channel_sanitize_name(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.len() > 64 {
        return None;
    }
    if trimmed
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
    {
        Some(trimmed.to_string())
    } else {
        None
    }
}

/// Execute a function while holding an exclusive file lock on the TOML file.
pub fn with_lock<F, R>(toml_path: &std::path::Path, f: F) -> CarrierResult<R>
where
    F: FnOnce(&std::path::Path) -> CarrierResult<R>,
{
    let lock_path = {
        let mut s = toml_path.as_os_str().to_owned();
        s.push(".lock");
        std::path::PathBuf::from(s)
    };

    if let Some(parent) = toml_path.parent() {
        std::fs::create_dir_all(parent).map_err(CarrierError::Io)?;
    }

    let lock_file = std::fs::File::create(&lock_path).map_err(CarrierError::Io)?;
    lock_file.lock_exclusive().map_err(CarrierError::Io)?;

    let result = f(toml_path);

    drop(lock_file);
    let _ = std::fs::remove_file(&lock_path);

    result
}

/// Read and parse a TOML file. Returns an empty table if the file doesn't exist.
pub fn read_toml(path: &std::path::Path) -> CarrierResult<toml::Value> {
    if path.exists() {
        let content = std::fs::read_to_string(path).map_err(CarrierError::Io)?;
        content
            .parse::<toml::Value>()
            .map_err(|e| CarrierError::Serialization(format!("Failed to parse plugin config: {e}")))
    } else {
        Ok(toml::Value::Table(Default::default()))
    }
}

/// Serialize a TOML value and write it atomically.
pub fn write_toml(path: &std::path::Path, doc: &toml::Value) -> CarrierResult<()> {
    let content = toml::to_string_pretty(doc).map_err(|e| {
        CarrierError::Serialization(format!("Failed to serialize plugin config: {e}"))
    })?;
    atomic_write(path, &content).map_err(CarrierError::Io)
}
