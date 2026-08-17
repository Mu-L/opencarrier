//! Pending freepublish tracker — closes the "blind submit" gap (2026-08-18).
//!
//! `freepublish_submit` returns a `publish_id` that used to survive only in a
//! log line. The publish tool now records it here; the daemon's `PublishPoll`
//! cron arm polls each pending id via `freepublish_get` and removes it once
//! terminal. The same file's `empty_streak` lets that arm self-delete after
//! consecutive empty rounds (no forever-poller).
//!
//! State file: `<home>/data/wechat_publish_pending.json`, map of
//! `app_id -> { publish_ids: [...], empty_streak: N }`. All operations are
//! synchronous std::fs with tmp+rename atomic writes (the file is ~100 bytes;
//! called from async contexts but never hot).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use types::error::CarrierResult;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AccountPending {
    #[serde(default)]
    publish_ids: Vec<String>,
    #[serde(default)]
    empty_streak: u32,
}

type Tracker = BTreeMap<String, AccountPending>;

fn tracker_path(home: &Path) -> PathBuf {
    home.join("data").join("wechat_publish_pending.json")
}

fn read_tracker(home: &Path) -> Tracker {
    std::fs::read_to_string(tracker_path(home))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_tracker(home: &Path, tracker: &Tracker) -> CarrierResult<()> {
    let path = tracker_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(tracker)
            .map_err(|e| types::error::CarrierError::Serialization(e.to_string()))?,
    )?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Record a freshly-submitted `publish_id` (dedup; resets the empty streak —
/// there is something to poll again).
pub fn track(home: &Path, app_id: &str, publish_id: &str) -> CarrierResult<()> {
    let mut tracker = read_tracker(home);
    let entry = tracker.entry(app_id.to_string()).or_default();
    if !entry.publish_ids.iter().any(|p| p == publish_id) {
        entry.publish_ids.push(publish_id.to_string());
    }
    entry.empty_streak = 0;
    write_tracker(home, &tracker)
}

/// Pending publish ids for `app_id`.
pub fn pending(home: &Path, app_id: &str) -> Vec<String> {
    read_tracker(home)
        .get(app_id)
        .map(|e| e.publish_ids.clone())
        .unwrap_or_default()
}

/// Remove a publish id after a terminal status was reported. The account key
/// is dropped entirely once nothing is pending and no streak is worth keeping.
pub fn remove(home: &Path, app_id: &str, publish_id: &str) -> CarrierResult<()> {
    let mut tracker = read_tracker(home);
    if let Some(entry) = tracker.get_mut(app_id) {
        entry.publish_ids.retain(|p| p != publish_id);
    }
    let drainable = tracker
        .get(app_id)
        .map(|e| e.publish_ids.is_empty() && e.empty_streak == 0)
        .unwrap_or(false);
    if drainable {
        tracker.remove(app_id);
    }
    write_tracker(home, &tracker)
}

/// Increment the consecutive-empty counter, returning the new value (used by
/// the poll arm to decide self-deletion).
pub fn bump_streak(home: &Path, app_id: &str) -> CarrierResult<u32> {
    let mut tracker = read_tracker(home);
    let entry = tracker.entry(app_id.to_string()).or_default();
    entry.empty_streak += 1;
    let streak = entry.empty_streak;
    write_tracker(home, &tracker)?;
    Ok(streak)
}

/// Whether any account has pending publishes (daemon reconcile check that
/// decides whether a PublishPoll job needs creating).
pub fn has_any_pending(home: &Path) -> bool {
    read_tracker(home)
        .values()
        .any(|e| !e.publish_ids.is_empty())
}

/// All app_ids that currently have pending publishes (daemon reconcile uses
/// this to ensure a poll job exists per account).
pub fn pending_accounts(home: &Path) -> Vec<String> {
    read_tracker(home)
        .into_iter()
        .filter(|(_, e)| !e.publish_ids.is_empty())
        .map(|(app_id, _)| app_id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_home(name: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("oc-wechat-pubtrack-{name}"));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn track_pending_remove_roundtrip() {
        let home = tmp_home("basic");
        track(&home, "wxAAA", "pub-1").unwrap();
        track(&home, "wxAAA", "pub-2").unwrap();
        // Duplicate track is a no-op.
        track(&home, "wxAAA", "pub-1").unwrap();
        assert_eq!(pending(&home, "wxAAA"), vec!["pub-1", "pub-2"]);
        assert!(has_any_pending(&home));

        remove(&home, "wxAAA", "pub-1").unwrap();
        assert_eq!(pending(&home, "wxAAA"), vec!["pub-2"]);
        remove(&home, "wxAAA", "pub-2").unwrap();
        assert!(pending(&home, "wxAAA").is_empty());
        assert!(!has_any_pending(&home));
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Streak resets on a fresh track (something to poll again) and the poll
    /// arm uses consecutive empties to self-delete.
    #[test]
    fn streak_bumps_and_resets() {
        let home = tmp_home("streak");
        track(&home, "wxAAA", "pub-1").unwrap();
        remove(&home, "wxAAA", "pub-1").unwrap();
        assert_eq!(bump_streak(&home, "wxAAA").unwrap(), 1);
        assert_eq!(bump_streak(&home, "wxAAA").unwrap(), 2);
        track(&home, "wxAAA", "pub-2").unwrap();
        // Fresh pending reset the streak — the next bump starts from 1 again
        // (would be 3 if track hadn't reset).
        assert_eq!(bump_streak(&home, "wxAAA").unwrap(), 1);
        assert_eq!(pending(&home, "wxAAA"), vec!["pub-2"]);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Missing file behaves as empty tracker, not an error.
    #[test]
    fn missing_file_is_empty() {
        let home = tmp_home("missing");
        assert!(pending(&home, "wxAAA").is_empty());
        assert!(!has_any_pending(&home));
        let _ = std::fs::remove_dir_all(&home);
    }
}
