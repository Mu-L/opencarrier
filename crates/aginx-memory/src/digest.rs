//! End-of-day digest builder for the global activity tree (PG-backed).
//!
//! Ported from `memory::tree::tree_global::digest` - same logic, stores swapped
//! to the PG async stores. Once per calendar day we walk every active source
//! tree, collect the summary material that covers that day, fold it into one
//! cross-source recap, and persist it as an L0 node in the singleton global
//! tree. Constants (`GLOBAL_SCOPE` etc.) and `content_store`/`Summariser` are
//! reused verbatim from the memory crate.

use std::path::Path;
use std::sync::Arc;

use chrono::{Datelike, Duration, NaiveDate, TimeZone, Utc};
use deadpool_postgres::Pool;
use memory::tree::content_store::ContentStore;
use memory::tree::summariser::{Summariser, SummaryContext, SummaryInput};
use memory::tree::summariser::inert::InertSummariser;
use memory::tree::tree_global::{GLOBAL_SCOPE, GLOBAL_TOKEN_BUDGET, WEEKLY_SEAL_THRESHOLD};
use memory::tree::types::{SummaryNode, Tree};
use types::error::CarrierResult;
use types::memory_tree::TreeKind;

use crate::bucket_seal::BucketSealEngine;
use crate::pg::chunk_store::ChunkStore;
use crate::pg::tree_store::TreeStore;

/// Topic stamped on each L0 daily node so later runs can find "this day's"
/// digest. Format: `digest:YYYY-MM-DD` (calendar date from the job).
pub const DIGEST_TOPIC_PREFIX: &str = "digest:";

/// Calendar-day timezone for daily digests: Asia/Shanghai (UTC+8).
/// A user's "day" ends at local midnight — with UTC the split lands at
/// 08:00 local, so an evening chat and the 2am follow-up land in different
/// digest days. All `digest:*` date math (topic stamps, day windows,
/// scheduler wake) uses this offset.
pub fn digest_tz() -> chrono::FixedOffset {
    chrono::FixedOffset::east_opt(8 * 3600).expect("valid offset")
}

/// Topic tag for the daily digest of `date`.
pub fn digest_topic(date: NaiveDate) -> String {
    format!("{DIGEST_TOPIC_PREFIX}{}", date.format("%Y-%m-%d"))
}

/// Inclusive millisecond bounds of `date` in [`DIGEST_TZ`]
/// (00:00:00.000 .. 23:59:59.999 local, expressed as absolute UTC ms).
pub fn day_bounds_ms(date: NaiveDate) -> (i64, i64) {
    let tz = digest_tz();
    let start = tz
        .with_ymd_and_hms(date.year(), date.month(), date.day(), 0, 0, 0)
        .single()
        .expect("valid midnight")
        .timestamp_millis();
    let next_date = date + Duration::days(1);
    let next = tz
        .with_ymd_and_hms(
            next_date.year(),
            next_date.month(),
            next_date.day(),
            0,
            0,
            0,
        )
        .single()
        .expect("valid next midnight")
        .timestamp_millis();
    (start, next - 1)
}

/// True when `node` is the L0 daily recap for `date`.
///
/// Match on the `digest:YYYY-MM-DD` topic (new nodes) or on an exact local-day
/// time range (same contract, in case a writer omitted the topic). A leftover
/// L0 whose range is the source material — not a calendar day — does not match,
/// so it cannot block the next day's digest.
pub fn is_daily_for_date(node: &SummaryNode, date: NaiveDate) -> bool {
    let tag = digest_topic(date);
    if node.topics.iter().any(|t| t == &tag) {
        return true;
    }
    let (start, end) = day_bounds_ms(date);
    node.time_range_start_ms == start && node.time_range_end_ms == end
}

/// Outcome of a single `end_of_day_digest` call.
#[derive(Debug, Clone)]
pub enum DigestOutcome {
    /// Emitted one L0 daily node and possibly cascaded into higher-level seals.
    Emitted { daily_id: String, source_count: usize },
    /// No source tree had material for the target day.
    EmptyDay,
    /// An L0 daily node already exists for this day.
    Skipped { existing_id: String },
}

/// Run an end-of-day digest for a given owner and calendar `date` (in
/// [`digest_tz`]).
pub async fn end_of_day_digest(
    pool: &Pool,
    content_root: &Path,
    owner_id: &str,
    date: NaiveDate,
    summariser: &dyn Summariser,
) -> CarrierResult<DigestOutcome> {
    let tree_store = TreeStore::new(pool.clone());
    let content_store = ContentStore::new(content_root.to_path_buf());
    let chunk_store = ChunkStore::new(pool.clone());

    // Get or create the global tree (owner-shared: user_id = "").
    let global = tree_store
        .get_or_create_tree(owner_id, "", TreeKind::Global, GLOBAL_SCOPE)
        .await?;

    let now_ms = Utc::now().timestamp_millis();
    let (day_start_ms, day_end_ms) = day_bounds_ms(date);

    // Idempotent per (owner, date): one L0 daily node per UTC day.
    if let Some(existing) =
        find_existing_daily(&tree_store, owner_id, &global.id, date).await?
    {
        return Ok(DigestOutcome::Skipped {
            existing_id: existing,
        });
    }

    // Gather one contribution per active source tree (cross-user by design -
    // the daily digest folds every user's activity under this owner).
    let source_trees = tree_store
        .list_trees(owner_id, None, Some(TreeKind::Source), 1000)
        .await?;
    let mut inputs: Vec<SummaryInput> = Vec::with_capacity(source_trees.len());

    for tree_summary in &source_trees {
        if let Some(tree) = tree_store
            .get_tree(owner_id, None, &tree_summary.tree_id)
            .await?
        {
            if let Some(inp) =
                pick_source_contribution(&tree_store, &chunk_store, &content_store, owner_id, &tree)
                    .await?
            {
                inputs.push(inp);
            }
        }
    }

    if inputs.is_empty() {
        return Ok(DigestOutcome::EmptyDay);
    }

    // Fold cross-source material into one daily recap
    let ctx = SummaryContext {
        tree_id: &global.id,
        tree_kind: TreeKind::Global,
        target_level: 0,
        token_budget: GLOBAL_TOKEN_BUDGET,
    };
    let output = summariser.summarise(&inputs, &ctx);

    // Union entities from all inputs; always stamp the calendar-day topic so
    // the next day's run can tell this L0 apart from yesterday's.
    let mut entities_set = std::collections::BTreeSet::new();
    let mut topics_set = std::collections::BTreeSet::new();
    for inp in &inputs {
        for e in &inp.entities {
            entities_set.insert(e.clone());
        }
        for t in &inp.topics {
            topics_set.insert(t.clone());
        }
    }
    topics_set.insert(digest_topic(date));

    let score = inputs
        .iter()
        .map(|i| i.score)
        .fold(f32::NEG_INFINITY, f32::max)
        .max(0.0);

    let daily_id = format!("sum_L0_{}", uuid::Uuid::new_v4().simple());
    let daily = SummaryNode {
        id: daily_id.clone(),
        tree_id: global.id.clone(),
        // Owner-shared: the daily digest spans every user under this owner.
        user_id: String::new(),
        tree_kind: TreeKind::Global,
        level: 0,
        parent_id: None,
        child_ids: inputs.iter().map(|i| i.id.clone()).collect(),
        content: output.content.clone(),
        token_count: output.token_count,
        entities: entities_set.into_iter().collect(),
        topics: topics_set.into_iter().collect(),
        // L0 identity is the calendar day, not the source material's span.
        time_range_start_ms: day_start_ms,
        time_range_end_ms: day_end_ms,
        score,
        sealed_at_ms: now_ms,
        deleted: false,
        embedding: None,
    };

    // Write content (file I/O - sync, reused from memory crate)
    content_store.ensure_dirs(owner_id)?;
    content_store.write_summary(owner_id, &daily)?;

    // Persist
    tree_store.insert_summary(owner_id, &daily).await?;

    // Append into the global tree's L0 buffer
    let seal_engine = BucketSealEngine::new(
        pool.clone(),
        content_root.to_path_buf(),
        Arc::new(InertSummariser),
    );
    seal_engine
        .append_to_buffer(owner_id, &global.id, 0, &daily_id, daily.token_count as i64, now_ms)
        .await?;

    // Check if weekly seal should trigger
    let buf = seal_engine
        .get_or_create_buffer(owner_id, &global.id, 0)
        .await?;
    if buf.item_ids.len() >= WEEKLY_SEAL_THRESHOLD {
        seal_engine.cascade_seals(owner_id, &global, 0, true).await?;
    }

    Ok(DigestOutcome::Emitted {
        daily_id,
        source_count: inputs.len(),
    })
}

async fn find_existing_daily(
    tree_store: &TreeStore,
    owner_id: &str,
    global_tree_id: &str,
    date: NaiveDate,
) -> CarrierResult<Option<String>> {
    // Enough for several years of one-L0-per-day (weekly seals keep L0 rows).
    let summaries = tree_store
        .list_summaries(owner_id, None, global_tree_id, Some(0), 2000)
        .await?;
    Ok(summaries
        .into_iter()
        .find(|n| is_daily_for_date(n, date))
        .map(|n| n.id))
}

async fn pick_source_contribution(
    tree_store: &TreeStore,
    _chunk_store: &ChunkStore,
    _content_store: &ContentStore,
    owner_id: &str,
    source_tree: &Tree,
) -> CarrierResult<Option<SummaryInput>> {
    // Pick the highest-level summary (root) from this source tree
    if source_tree.root_id.is_none() {
        // No sealed summaries yet
        return Ok(None);
    }

    let root_id = source_tree.root_id.as_ref().unwrap();
    match tree_store.get_summary(owner_id, None, root_id).await? {
        Some(node) => Ok(Some(SummaryInput {
            id: node.id,
            content: format!("[{}]\n{}", source_tree.scope, node.content),
            token_count: node.token_count,
            entities: node.entities,
            topics: node.topics,
            time_range_start_ms: node.time_range_start_ms,
            time_range_end_ms: node.time_range_end_ms,
            score: node.score,
        })),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use deadpool_postgres::Manager;
    use memory::tree::types::SummaryNode;
    use tempfile::TempDir;

    async fn setup() -> Option<(Pool, std::path::PathBuf, TempDir)> {
        let url = std::env::var("AGINX_MEMORY_TEST_PG").ok()?;
        let (mut client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await.ok()?;
        tokio::spawn(async move { let _ = conn.await; });
        crate::pg::reset_and_migrate(&mut client).await;
        drop(client);
        let cfg: tokio_postgres::Config = url.parse().ok()?;
        let mgr = Manager::new(cfg, tokio_postgres::NoTls);
        let pool = deadpool_postgres::Pool::builder(mgr).max_size(4).build().ok()?;
        let dir = TempDir::new().ok()?;
        Some((pool, dir.path().to_path_buf(), dir))
    }

    #[tokio::test]
    async fn empty_day() {
        let (pool, content_root, _dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip (set AGINX_MEMORY_TEST_PG)");
                return;
            }
        };
        let date = NaiveDate::from_ymd_opt(2026, 5, 16).expect("valid date");
        let result = end_of_day_digest(&pool, &content_root, "owner_1", date, &InertSummariser)
            .await
            .unwrap();
        assert!(matches!(result, DigestOutcome::EmptyDay));
    }

    #[tokio::test]
    async fn digest_creates_global_tree() {
        let (pool, content_root, _dir) = match setup().await {
            Some(x) => x,
            None => {
                eprintln!("skip");
                return;
            }
        };
        let tree_store = TreeStore::new(pool.clone());

        // Create a source tree with a root summary.
        let source_tree = tree_store
            .get_or_create_tree("owner_1", "", TreeKind::Source, "wechat:test:sender")
            .await
            .unwrap();

        let summary = SummaryNode {
            id: "sum_test".to_string(),
            tree_id: source_tree.id.clone(),
            user_id: String::new(),
            tree_kind: TreeKind::Source,
            level: 1,
            parent_id: None,
            child_ids: vec!["chunk_1".to_string()],
            content: "Discussion about project Phoenix".to_string(),
            token_count: 50,
            entities: vec!["person:Alice".to_string()],
            topics: vec!["project-phoenix".to_string()],
            time_range_start_ms: 1_700_000_000_000,
            time_range_end_ms: 1_700_000_060_000,
            score: 0.85,
            sealed_at_ms: 1_700_000_120_000,
            deleted: false,
            embedding: None,
        };
        tree_store.insert_summary("owner_1", &summary).await.unwrap();
        tree_store
            .update_tree_after_seal("owner_1", &source_tree.id, 1, 1_700_000_120_000)
            .await
            .unwrap();

        let content_store = ContentStore::new(content_root.to_path_buf());
        content_store.ensure_dirs("owner_1").unwrap();
        let date = NaiveDate::from_ymd_opt(2026, 5, 16).expect("valid date");
        let result =
            end_of_day_digest(&pool, &content_root, "owner_1", date, &InertSummariser)
                .await
                .unwrap();

        let first_id = match result {
            DigestOutcome::Emitted { source_count, daily_id } => {
                assert!(source_count >= 1);
                daily_id
            }
            other => panic!("expected Emitted, got {other:?}"),
        };

        // Global tree should now exist, stamped for this date.
        let global = tree_store
            .get_or_create_tree("owner_1", "", TreeKind::Global, GLOBAL_SCOPE)
            .await
            .unwrap();
        assert_eq!(global.kind, TreeKind::Global);
        let dailies = tree_store
            .list_summaries("owner_1", None, &global.id, Some(0), 10)
            .await
            .unwrap();
        assert_eq!(dailies.len(), 1);
        assert!(is_daily_for_date(&dailies[0], date));
        assert!(dailies[0].topics.contains(&digest_topic(date)));

        // Same date is idempotent.
        let again =
            end_of_day_digest(&pool, &content_root, "owner_1", date, &InertSummariser)
                .await
                .unwrap();
        match again {
            DigestOutcome::Skipped { existing_id } => assert_eq!(existing_id, first_id),
            other => panic!("expected Skipped, got {other:?}"),
        }

        // A different date emits a second L0 (the leftover day does not block).
        let next_date = NaiveDate::from_ymd_opt(2026, 5, 17).expect("valid date");
        let next =
            end_of_day_digest(&pool, &content_root, "owner_1", next_date, &InertSummariser)
                .await
                .unwrap();
        assert!(matches!(next, DigestOutcome::Emitted { .. }));
        let dailies = tree_store
            .list_summaries("owner_1", None, &global.id, Some(0), 10)
            .await
            .unwrap();
        assert_eq!(dailies.len(), 2);
    }

    fn sample_node(topics: Vec<String>, start: i64, end: i64) -> SummaryNode {
        SummaryNode {
            id: "sum_x".into(),
            tree_id: "tree_g".into(),
            user_id: String::new(),
            tree_kind: TreeKind::Global,
            level: 0,
            parent_id: None,
            child_ids: vec![],
            content: String::new(),
            token_count: 0,
            entities: vec![],
            topics,
            time_range_start_ms: start,
            time_range_end_ms: end,
            score: 0.0,
            sealed_at_ms: 0,
            deleted: false,
            embedding: None,
        }
    }

    #[test]
    fn daily_match_by_topic() {
        let date = NaiveDate::from_ymd_opt(2026, 8, 13).unwrap();
        let node = sample_node(vec![digest_topic(date)], 0, 1);
        assert!(is_daily_for_date(&node, date));
        assert!(!is_daily_for_date(
            &node,
            NaiveDate::from_ymd_opt(2026, 8, 14).unwrap()
        ));
    }

    #[test]
    fn daily_match_by_local_day_range() {
        let date = NaiveDate::from_ymd_opt(2026, 8, 13).unwrap();
        let (start, end) = day_bounds_ms(date);
        let node = sample_node(vec![], start, end);
        assert!(is_daily_for_date(&node, date));
        assert!(!is_daily_for_date(
            &node,
            NaiveDate::from_ymd_opt(2026, 8, 14).unwrap()
        ));
    }

    #[test]
    fn day_bounds_are_shanghai_midnights() {
        // 2026-08-13 00:00 +08:00 == 2026-08-12 16:00 UTC.
        let (start, end) = day_bounds_ms(NaiveDate::from_ymd_opt(2026, 8, 13).unwrap());
        let start_utc = chrono::DateTime::from_timestamp_millis(start).unwrap();
        assert_eq!(start_utc.to_rfc3339(), "2026-08-12T16:00:00+00:00");
        // end = next midnight - 1ms
        assert_eq!(end - start, 86_399_999);
    }

    #[test]
    fn leftover_source_span_does_not_block() {
        // Production L0s written before the date stamp used the source
        // material's time span, not a calendar day.
        let date = NaiveDate::from_ymd_opt(2026, 8, 13).unwrap();
        let node = sample_node(vec![], 1_700_000_000_000, 1_754_000_000_000);
        assert!(!is_daily_for_date(&node, date));
    }
}
