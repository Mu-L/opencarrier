//! File-level manifest of a clone workspace's definition layer.
//!
//! Used by the `dup` VCS and the opencarrier dup-remote endpoints to do
//! git-style file-level sync (no packed archive). The manifest is a map of
//! relative path -> SHA-256 for every definition-layer file (exactly what
//! would go into a .agx pack), plus a top-level `hash` (SHA-256 of the sorted
//! `path:hash` serialization) used as a state id for fast-forward comparison.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Runtime dirs/files excluded from the manifest (mirror `extractor::SKIP_PACK`
/// plus `.dup/` VCS state dir + `admins.json` deployment-specific admin list).
const SKIP: &[&str] = &[
    "agent.toml",
    "AGENT.json",
    "admins.json",
    "output",
    "sessions",
    "history",
    "logs",
    "users",
    "data",
    "senders",
    ".lifecycle",
    ".dup",
];

/// True if a top-level entry is a test-workspace dir: `test`, `test2`, ... or
/// `test-foo`. (Catches `test`/`testN` that the old `test-` prefix missed.)
pub fn is_test_dir(top: &str) -> bool {
    top == "test"
        || top.starts_with("test-")
        || (top.starts_with("test") && top[4..].chars().all(|c| c.is_ascii_digit()))
}

/// True if a file name is a backup: `foo.bak` or `foo.bak.<timestamp>`.
pub fn is_bak(top: &str) -> bool {
    top.ends_with(".bak") || top.contains(".bak.")
}

/// A file-level snapshot of a workspace's definition layer.
///
/// `files` maps relative path -> hex SHA-256 of file content. `hash` is the
/// SHA-256 of the sorted `path:hash` serialization, a stable state id used to
/// detect fast-forward / divergence without transferring file contents.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    /// relative path -> hex SHA-256 of file content
    pub files: BTreeMap<String, String>,
    /// SHA-256 of the sorted `path:hash` serialization - state id
    pub hash: String,
}

impl Manifest {
    /// An empty manifest (no tracked files). Its `hash` is the SHA-256 of "".
    pub fn empty() -> Self {
        let mut files = BTreeMap::new();
        let hash = manifest_hash(&files);
        files.clear(); // keep clippy happy; empty either way
        Manifest { files, hash }
    }
}

/// Build a manifest by walking `workspace` and hashing every definition-layer
/// file. Selection mirrors `extractor::pack_workspace_as_agx` (same skip rules),
/// so the manifest tracks exactly the files that would be packed.
pub fn build_manifest(workspace: &Path) -> Result<Manifest> {
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    walk(workspace, workspace, &mut files)?;
    let hash = manifest_hash(&files);
    Ok(Manifest { files, hash })
}

fn walk(base: &Path, cur: &Path, files: &mut BTreeMap<String, String>) -> Result<()> {
    let entries = std::fs::read_dir(cur).with_context(|| format!("read_dir {}", cur.display()))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let name_str = entry.file_name().to_string_lossy().into_owned();

        // Skip macOS Apple Double / .DS_Store
        if name_str.starts_with("._") || name_str == ".DS_Store" {
            continue;
        }
        // Skip dup VCS artifacts: conflict sidecars + transient write tmps.
        if name_str.ends_with(".dup-theirs") || name_str.ends_with(".duptmp") {
            continue;
        }

        if path.is_dir() {
            walk(base, &path, files)?;
        } else {
            let rel = path.strip_prefix(base).unwrap_or(&path);
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            let top = rel_str.split('/').next().unwrap_or(&rel_str);
            // Skip runtime layer + test-workspace dirs + backup files.
            if SKIP.contains(&top) || is_test_dir(top) || is_bak(top) {
                continue;
            }
            let data = std::fs::read(&path)
                .with_context(|| format!("read {}", path.display()))?;
            let mut h = Sha256::new();
            h.update(&data);
            files.insert(rel_str, format!("{:x}", h.finalize()));
        }
    }
    Ok(())
}

/// Compute the manifest state id: SHA-256 of the sorted `path:hash` lines.
pub fn manifest_hash(files: &BTreeMap<String, String>) -> String {
    let mut h = Sha256::new();
    for (p, sha) in files {
        h.update(p.as_bytes());
        h.update(b":");
        h.update(sha.as_bytes());
        h.update(b"\n");
    }
    format!("{:x}", h.finalize())
}

/// Compute SHA-256 of arbitrary bytes (hex).
pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    format!("{:x}", h.finalize())
}

/// Write a set of files (`path -> bytes`) into `workspace`, enforcing the same
/// definition-layer + traversal safety as the `dup` push endpoint. Creates
/// parent dirs and writes atomically (`.duptmp` + rename). Files outside the
/// definition layer (runtime dirs, `agent.toml`/`AGENT.json`, test dirs, `.bak`)
/// are skipped with a warning rather than written.
///
/// This is the file-level counterpart of `extract_agx`: instead of unpacking a
/// tar.gz blob, it writes individually-fetched files (e.g. pulled from a DupHub
/// manifest). Returns security warnings (empty on clean).
pub fn write_files_to_workspace(
    files: &BTreeMap<String, Vec<u8>>,
    workspace: &Path,
) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    let ws_canonical = workspace
        .canonicalize()
        .with_context(|| format!("canonicalize {}", workspace.display()))?;
    for (rel, content) in files {
        let p = Path::new(rel);
        if p
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::RootDir))
        {
            anyhow::bail!("path traversal denied: {rel}");
        }
        let top = rel.split('/').next().unwrap_or(rel);
        if SKIP.contains(&top) || is_test_dir(top) || is_bak(top) {
            warnings.push(format!("skipped non-definition-layer file: {rel}"));
            continue;
        }
        let file_path = workspace.join(rel);
        // For new files, validate via the parent dir; for existing, via the file.
        let check = if file_path.exists() {
            file_path
                .canonicalize()
                .unwrap_or_else(|_| file_path.clone())
        } else {
            file_path
                .parent()
                .and_then(|p| p.canonicalize().ok())
                .map(|p| p.join(file_path.file_name().unwrap_or_default()))
                .unwrap_or_else(|| file_path.clone())
        };
        if !check.starts_with(&ws_canonical) {
            anyhow::bail!("path traversal denied: {rel}");
        }
        if let Some(parent) = file_path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("mkdir {rel}"))?;
        }
        // Atomic write: sibling .duptmp then rename.
        let filename = file_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let tmp = file_path.with_file_name(format!(".{filename}.duptmp"));
        std::fs::write(&tmp, content).with_context(|| format!("write {rel}"))?;
        if let Err(e) = std::fs::rename(&tmp, &file_path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e).with_context(|| format!("rename {rel}"));
        }
    }
    Ok(warnings)
}
