//! File-level manifest of a clone workspace's definition layer.
//!
//! Used by the `dup` VCS and the opencarrier dup-remote endpoints to do
//! git-style file-level sync (no packed archive). The manifest is a map of
//! relative path -> SHA-256 for every definition-layer file, plus a top-level
//! `hash` (SHA-256 of the sorted `path:hash` serialization) used as a state id
//! for fast-forward comparison.

use std::collections::BTreeMap;
use std::path::{Component, Path};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

/// Runtime dirs/files excluded from the definition-layer manifest: agent
/// runtime state (output/sessions/history/logs/...), the `.dup/` VCS state dir,
/// and `admins.json` (deployment-specific admin list).
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
/// file. Selection uses `iter_definition_files` (the shared definition-layer
/// walk, excludes runtime dirs + `.dup/` VCS state), so the manifest tracks
/// exactly the files sent file-level to DupHub.
pub fn build_manifest(workspace: &Path) -> Result<Manifest> {
    let entries = iter_definition_files(workspace)?;
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    for (rel, abs) in &entries {
        let data = std::fs::read(abs).with_context(|| format!("read {}", abs.display()))?;
        files.insert(rel.clone(), sha256_hex(&data));
    }
    let hash = manifest_hash(&files);
    Ok(Manifest { files, hash })
}

/// Read every definition-layer file in `workspace` into a `path -> bytes` map.
/// The local-side mirror of `hub::fetch_dup_files`: shares `iter_definition_files`
/// with `build_manifest`, so the file set (and thus the manifest hash) is
/// guaranteed identical. Used by `hub push` to send file-level content to DupHub.
///
/// Uses `SKIP` (which includes `.dup/`), so the local `.dup/` VCS state is NOT
/// leaked into the pushed payload.
pub fn collect_definition_files(workspace: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let entries = iter_definition_files(workspace)?;
    let mut files: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for (rel, abs) in &entries {
        let data = std::fs::read(abs).with_context(|| format!("read {}", abs.display()))?;
        files.insert(rel.clone(), data);
    }
    Ok(files)
}

/// Walk `workspace` and return the sorted `(rel_path, abs_path)` pairs for every
/// definition-layer file. Applies `SKIP` (runtime dirs, `agent.toml`,
/// `AGENT.json`, `admins.json`, `.dup/`) with `is_test_dir` and `is_bak`, and
/// skips macOS `._`/`.DS_Store` plus dup VCS artifacts (`.dup-theirs`,
/// `.duptmp`). Shared by `build_manifest` and `collect_definition_files` so both
/// enumerate the same file set.
fn iter_definition_files(workspace: &Path) -> Result<Vec<(String, std::path::PathBuf)>> {
    let mut out: Vec<(String, std::path::PathBuf)> = Vec::new();
    walk_collect(workspace, workspace, &mut out)?;
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn walk_collect(
    base: &Path,
    cur: &Path,
    out: &mut Vec<(String, std::path::PathBuf)>,
) -> Result<()> {
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
            walk_collect(base, &path, out)?;
        } else {
            let rel = path.strip_prefix(base).unwrap_or(&path);
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            let top = rel_str.split('/').next().unwrap_or(&rel_str);
            // Skip runtime layer + .dup VCS + test-workspace dirs + backup files.
            if SKIP.contains(&top) || is_test_dir(top) || is_bak(top) {
                continue;
            }
            out.push((rel_str, path));
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
/// This writes individually-fetched files (e.g. pulled from a DupHub manifest)
/// into the workspace, enforcing the definition-layer boundary. Returns security
/// warnings (empty on clean).
pub fn write_files_to_workspace(
    files: &BTreeMap<String, Vec<u8>>,
    workspace: &Path,
) -> Result<Vec<String>> {
    let mut warnings = Vec::new();
    // The workspace may not exist yet (fresh install, unlike the dup push path
    // where it always does) - create it so canonicalize works.
    std::fs::create_dir_all(workspace).with_context(|| format!("mkdir {}", workspace.display()))?;
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
        // Defense-in-depth: for an EXISTING entry, canonicalize and ensure it
        // stays within the workspace (catches symlink escape). New files are
        // already confined by the component check above (no `..` / root), so we
        // don't canonicalize their (possibly non-existent) parent - that would
        // compare a canonical workspace against a non-canonical join and false-
        // reject when the workspace ancestor is a symlink (e.g. macOS /var).
        if file_path.exists() {
            let canon = file_path
                .canonicalize()
                .unwrap_or_else(|_| file_path.clone());
            if !canon.starts_with(&ws_canonical) {
                anyhow::bail!("path traversal denied: {rel}");
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn tmp_dir(name: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("oc-manifest-{name}"));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn write_files_creates_fresh_workspace_and_writes() {
        let tmp = tmp_dir("fresh");
        assert!(!tmp.exists(), "precondition: workspace must not exist");
        let mut files = BTreeMap::new();
        files.insert("SOUL.md".to_string(), b"hello".to_vec());
        files.insert("knowledge/nested/deep.md".to_string(), b"world".to_vec());
        let warnings = write_files_to_workspace(&files, &tmp).unwrap();
        assert!(warnings.is_empty());
        assert_eq!(std::fs::read(tmp.join("SOUL.md")).unwrap(), b"hello");
        assert_eq!(
            std::fs::read(tmp.join("knowledge/nested/deep.md")).unwrap(),
            b"world"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn write_files_rejects_traversal() {
        let tmp = tmp_dir("trav");
        let mut files = BTreeMap::new();
        files.insert("../escape.md".to_string(), b"x".to_vec());
        assert!(write_files_to_workspace(&files, &tmp).is_err());
        assert!(!tmp.join("../escape.md").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn write_files_skips_runtime_layer_with_warnings() {
        let tmp = tmp_dir("skip");
        let mut files = BTreeMap::new();
        files.insert("agent.toml".to_string(), b"regen".to_vec());
        files.insert("sessions/x.json".to_string(), b"runtime".to_vec());
        files.insert("admins.json".to_string(), b"adm".to_vec());
        files.insert("SOUL.md".to_string(), b"keep".to_vec());
        let warnings = write_files_to_workspace(&files, &tmp).unwrap();
        // agent.toml + sessions/ + admins.json skipped; SOUL.md kept.
        assert_eq!(warnings.len(), 3);
        assert!(!tmp.join("agent.toml").exists());
        assert!(!tmp.join("sessions/x.json").exists());
        assert!(!tmp.join("admins.json").exists());
        assert_eq!(std::fs::read(tmp.join("SOUL.md")).unwrap(), b"keep");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn build_manifest_and_collect_definition_files_agree_and_exclude_dup() {
        let tmp = tmp_dir("agree");
        std::fs::create_dir_all(tmp.join("knowledge")).unwrap();
        std::fs::create_dir_all(tmp.join(".dup")).unwrap();
        std::fs::create_dir_all(tmp.join("sessions")).unwrap();
        std::fs::write(tmp.join("SOUL.md"), b"soul").unwrap();
        std::fs::write(tmp.join("knowledge/a.md"), b"a").unwrap();
        // .dup/ VCS state + sessions/ runtime must be excluded.
        std::fs::write(tmp.join(".dup/state"), b"vcs").unwrap();
        std::fs::write(tmp.join("sessions/x.json"), b"rt").unwrap();

        let manifest = build_manifest(&tmp).unwrap();
        let collected = collect_definition_files(&tmp).unwrap();

        // Identical file set (shared walk).
        let manifest_keys: Vec<&String> = manifest.files.keys().collect();
        let collect_keys: Vec<&String> = collected.keys().collect();
        assert_eq!(manifest_keys, collect_keys);

        // Definition files present, .dup + sessions excluded.
        assert!(manifest.files.contains_key("SOUL.md"));
        assert!(manifest.files.contains_key("knowledge/a.md"));
        assert!(!manifest.files.contains_key(".dup/state"));
        assert!(!manifest.files.contains_key("sessions/x.json"));

        // Per-file sha in manifest matches sha256 of collected bytes.
        assert_eq!(manifest.files["SOUL.md"], sha256_hex(b"soul"));
        assert_eq!(collected["SOUL.md"], b"soul");

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
