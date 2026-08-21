//! Shared utilities for reading/writing plugin.toml files.
//!
//! Used by `weixin.rs` for thread-safe, atomic TOML manipulation.

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
