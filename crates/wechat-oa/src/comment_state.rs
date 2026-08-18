//! Reader-comment dedup ledger for the `CommentPull` cron arm (2026-08-18).
//!
//! The arm pulls every published article's comments on a schedule; this file
//! remembers which `user_comment_id`s have already been ingested so each run
//! only writes the NEW ones into the clone's knowledge. State file:
//! `<home>/data/wechat_comment_seen.json`, map of
//! `app_id -> { msg_data_id -> [seen comment ids] }` (per-article capped at
//! the most recent `CAP` ids so the file stays bounded). Same tmp+rename
//! atomic-write discipline as `publish_tracker`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use types::error::CarrierResult;

/// Max remembered comment ids per article — WeChat shows ~50 newest per page;
/// 4x that comfortably absorbs pagination drift while keeping the file small.
const CAP: usize = 200;

type Seen = BTreeMap<String, BTreeMap<String, Vec<i64>>>;

fn state_path(home: &Path) -> PathBuf {
    home.join("data").join("wechat_comment_seen.json")
}

fn read_state(home: &Path) -> Seen {
    std::fs::read_to_string(state_path(home))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_state(home: &Path, state: &Seen) -> CarrierResult<()> {
    let path = state_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(state)
            .map_err(|e| types::error::CarrierError::Serialization(e.to_string()))?,
    )?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Which of `ids` have NOT been ingested yet, in the given order.
pub fn filter_new(home: &Path, app_id: &str, msg_data_id: &str, ids: &[i64]) -> Vec<i64> {
    let seen = read_state(home);
    let known = seen
        .get(app_id)
        .and_then(|m| m.get(msg_data_id))
        .map(|v| v.as_slice())
        .unwrap_or(&[]);
    ids.iter().copied().filter(|id| !known.contains(id)).collect()
}

/// Record `ids` as ingested for the article, capping the remembered set at
/// [`CAP`] most-recent entries (ids are pushed in arrival order, so the tail
/// is the newest).
pub fn mark_seen(home: &Path, app_id: &str, msg_data_id: &str, ids: &[i64]) -> CarrierResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut state = read_state(home);
    let entry = state
        .entry(app_id.to_string())
        .or_default()
        .entry(msg_data_id.to_string())
        .or_default();
    for id in ids {
        if !entry.contains(id) {
            entry.push(*id);
        }
    }
    if entry.len() > CAP {
        let drop = entry.len() - CAP;
        entry.drain(..drop);
    }
    write_state(home, &state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("oc-wechat-cmtn-{name}"));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn filter_then_mark_roundtrip() {
        let home = tmp_home("basic");
        let ids = [1i64, 2, 3];
        assert_eq!(filter_new(&home, "wxAAA", "2247", &ids), vec![1, 2, 3]);
        mark_seen(&home, "wxAAA", "2247", &ids).unwrap();
        // All known now — nothing new.
        assert!(filter_new(&home, "wxAAA", "2247", &ids).is_empty());
        // A later page carrying one old + one new id yields only the new one.
        assert_eq!(filter_new(&home, "wxAAA", "2247", &[2, 4]), vec![4]);
        // State is per-app and per-article.
        assert_eq!(filter_new(&home, "wxBBB", "2247", &ids), vec![1, 2, 3]);
        assert_eq!(filter_new(&home, "wxAAA", "9999", &ids), vec![1, 2, 3]);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn cap_keeps_newest_tail() {
        let home = tmp_home("cap");
        let first: Vec<i64> = (1..=CAP as i64).collect();
        mark_seen(&home, "wxAAA", "2247", &first).unwrap();
        mark_seen(&home, "wxAAA", "2247", &[CAP as i64 + 1]).unwrap();
        // The oldest id fell off — it looks "new" again, which is harmless
        // (re-ingesting one ancient comment at most once per CAP window).
        assert!(filter_new(&home, "wxAAA", "2247", &[1]).contains(&1));
        assert!(filter_new(&home, "wxAAA", "2247", &[CAP as i64 + 1]).is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn missing_file_is_empty() {
        let home = tmp_home("missing");
        assert_eq!(filter_new(&home, "wxAAA", "2247", &[7]), vec![7]);
        let _ = std::fs::remove_dir_all(&home);
    }
}
