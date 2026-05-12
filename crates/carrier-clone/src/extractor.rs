//! .agx archive extraction and packing — v3 "解压即 workspace" flow.
//!
//! v3 principle: .agx IS the workspace, just compressed. Extract directly,
//! no intermediate CloneData representation.


use std::path::Path;

use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use tracing::{debug, info, warn};

/// Files/dirs that belong to runtime, not the .agx package.
const SKIP_EXTRACT: &[&str] = &["agent.toml", "AGENT.json"];
const SKIP_PACK: &[&str] = &[
    "agent.toml",
    "AGENT.json",
    "output",
    "sessions",
    "history",
    "logs",
    "users",
    "data",
];

/// Extract a .agx (tar.gz) byte stream directly to a workspace directory.
///
/// This is the v3 install primitive: no temp file, no CloneData intermediate.
/// The .agx IS the workspace, just compressed.
pub fn extract_agx(agx_data: &[u8], workspace: &Path) -> Result<Vec<String>> {
    let decoder = GzDecoder::new(agx_data);
    let mut archive = tar::Archive::new(decoder);

    std::fs::create_dir_all(workspace)
        .with_context(|| format!("Failed to create workspace: {}", workspace.display()))?;

    let mut file_count = 0usize;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.to_string_lossy().to_string();

        // Skip directories
        if name.ends_with('/') {
            continue;
        }

        // Normalize: strip leading "./"
        let name = name.strip_prefix("./").unwrap_or(&name).to_string();

        // Skip macOS Apple Double files (._*)
        if Path::new(&name)
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("._"))
            .unwrap_or(false)
        {
            continue;
        }

        // Skip runtime files that don't belong in .agx
        let top_name = name.split('/').next().unwrap_or(&name);
        if SKIP_EXTRACT.contains(&top_name) {
            debug!("Skipping runtime file during extract: {}", name);
            continue;
        }

        // Security: prevent tar slip (path traversal)
        let dest = workspace.join(&name);
        let canonical_workspace = workspace.canonicalize().unwrap_or_else(|_| workspace.to_path_buf());
        if let Some(parent) = dest.parent() {
            if let Ok(canonical_parent) = parent.canonicalize() {
                if !canonical_parent.starts_with(&canonical_workspace) {
                    warn!("Tar slip detected, skipping: {}", name);
                    continue;
                }
            }
        }

        // Ensure parent directory exists
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }

        entry.unpack(&dest).with_context(|| {
            format!(
                "Failed to extract {} to {}",
                name,
                dest.display()
            )
        })?;
        file_count += 1;
    }

    info!(
        "Extracted .agx to {}: {} files",
        workspace.display(),
        file_count
    );

    // Security scan on extracted workspace
    let warnings = scan_workspace_security(workspace);
    if !warnings.is_empty() {
        warn!("Security scan found {} warnings", warnings.len());
        for w in &warnings {
            warn!("  - {}", w);
        }
    }

    Ok(warnings)
}

/// Security scan on an extracted workspace directory.
///
/// Checks for injection keywords, oversized files, and non-HTTPS URLs.
pub fn scan_workspace_security(workspace: &Path) -> Vec<String> {
    let mut warnings = Vec::new();

    let injection_keywords = [
        "ignore previous instructions",
        "ignore all previous",
        "jailbreak",
        "you are now",
        "new instructions:",
        "system override",
    ];

    // Scan key text files for injection keywords
    let files_to_scan = ["system_prompt.md", "SOUL.md"];
    for filename in &files_to_scan {
        let path = workspace.join(filename);
        if let Ok(content) = std::fs::read_to_string(&path) {
            let lower = content.to_lowercase();
            for keyword in &injection_keywords {
                if lower.contains(keyword) {
                    warnings.push(format!(
                        "{} contains potential injection keyword: '{}'",
                        filename, keyword
                    ));
                }
            }
            if content.len() > 1_000_000 {
                warnings.push(format!("{} is very large: {} bytes", filename, content.len()));
            }
        }
    }

    // Check knowledge files for size
    let knowledge_dir = workspace.join("knowledge");
    if knowledge_dir.is_dir() {
        if let Ok(entries) = walk_dir(&knowledge_dir) {
            for path in &entries {
                if path.extension().map(|e| e == "md").unwrap_or(false) {
                    if let Ok(metadata) = path.metadata() {
                        if metadata.len() > 1_000_000 {
                            let rel = path.strip_prefix(workspace).unwrap_or(path);
                            warnings.push(format!(
                                "knowledge/{} is very large: {} bytes",
                                rel.display(),
                                metadata.len()
                            ));
                        }
                    }
                }
            }
        }
    }

    // Check skill scripts for non-HTTPS URLs
    let skills_dir = workspace.join("skills");
    if skills_dir.is_dir() {
        if let Ok(entries) = walk_dir(&skills_dir) {
            for path in &entries {
                if path.extension().map(|e| e == "toml").unwrap_or(false) {
                    if let Ok(content) = std::fs::read_to_string(path) {
                        if content.contains("http://") && !content.contains("localhost") {
                            let rel = path.strip_prefix(workspace).unwrap_or(path);
                            warnings.push(format!(
                                "Skill script {} uses non-HTTPS URL",
                                rel.display()
                            ));
                        }
                    }
                }
            }
        }
    }

    warnings
}

/// Pack a workspace directory into .agx (tar.gz) bytes.
///
/// This is the v3 export primitive: just compress the workspace, excluding
/// runtime files that don't belong in the package.
pub fn pack_workspace_as_agx(workspace: &Path) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    {
        let enc = GzEncoder::new(&mut buf, flate2::Compression::default());
        let mut tar = tar::Builder::new(enc);

        let files = walk_dir(workspace).context("Failed to walk workspace directory")?;

        for file_path in &files {
            let rel = file_path
                .strip_prefix(workspace)
                .with_context(|| format!("Path outside workspace: {}", file_path.display()))?;
            let rel_str = rel.to_string_lossy();

            // Skip runtime files/directories
            let top = rel_str.split('/').next().unwrap_or(&rel_str);
            if SKIP_PACK.contains(&top) {
                continue;
            }

            // Skip macOS Apple Double files
            if rel
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("._"))
                .unwrap_or(false)
            {
                continue;
            }

            let data = std::fs::read(file_path)
                .with_context(|| format!("Failed to read {}", file_path.display()))?;

            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar.append_data(&mut header, rel_str.as_ref(), data.as_slice())
                .with_context(|| format!("Failed to add {} to archive", rel_str))?;
        }

        tar.into_inner()
            .context("Failed to finalize tar archive")?
            .finish()
            .context("Failed to finalize gzip")?;
    }

    info!(
        "Packed workspace {} as .agx ({} bytes)",
        workspace.display(),
        buf.len()
    );

    Ok(buf)
}

/// Recursively collect all files in a directory.
fn walk_dir(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut files = Vec::new();
    walk_dir_recursive(dir, dir, &mut files)?;
    Ok(files)
}

fn walk_dir_recursive(_base: &Path, current: &Path, files: &mut Vec<std::path::PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk_dir_recursive(_base, &path, files)?;
        } else if path.is_file() {
            files.push(path);
        }
    }
    Ok(())
}
