//! Path validation utilities for MCP tool parameter sanitization.

/// Maximum file size allowed for reads (10 MB).
pub const MAX_FILE_SIZE: usize = 10 * 1024 * 1024;

/// Validate a file path for safe reading.
///
/// Rejects:
/// - Absolute paths (starting with `/`)
/// - Path traversal (`..` components)
///
/// This is a lightweight check suitable for MCP tool parameters where the path
/// comes from an LLM agent and should not escape the working directory.
pub fn validate_path(path: &str) -> Result<(), String> {
    if path.starts_with('/') {
        return Err(format!("Absolute paths not allowed: {path}"));
    }
    if path.is_empty() {
        return Err("Path must not be empty".to_string());
    }
    if path.split('/').any(|c| c == "..") {
        return Err(format!("Path traversal (..) not allowed: {path}"));
    }
    Ok(())
}

/// Validate that a byte slice is within the allowed file size limit.
pub fn validate_size(data: &[u8]) -> Result<(), String> {
    if data.len() > MAX_FILE_SIZE {
        return Err(format!(
            "File too large: {} bytes (max {} bytes / {} MB)",
            data.len(),
            MAX_FILE_SIZE,
            MAX_FILE_SIZE / (1024 * 1024)
        ));
    }
    Ok(())
}
