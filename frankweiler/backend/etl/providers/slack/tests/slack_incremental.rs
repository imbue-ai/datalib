//! End-to-end exercise of the cheap-probe + filtered-load + cursor
//! stamping path that frankweiler-sync uses to skip re-loading slack
//! payloads for unchanged threads.
//!
//! Shape of the test (the canonical "incremental" pattern: build,
//! mutate, re-render, assert what gets re-touched):
//!
//!   1. Build a slack doltlite_db with three threads: two reply
//!      threads and one standalone.
//!   2. Probe — assert one cursor per thread, formatted
//!      `"<MAX(fetched_at)>|<COUNT(*)>"`.
//!   3. Render pass 1 with empty priors → every thread gets rendered
//!      and stamps a non-NULL `upstream_cursor` on its `RenderedDoc`.
//!   4. "Mutate" the DB: add one new reply to thread A, add one new
//!      standalone message (a brand-new thread D), leave threads B
//!      and C untouched.
//!   5. Re-probe → A's cursor bumped (count and fetched_at both
//!      moved), B and C unchanged, D appears for the first time.
//!   6. Compute the filter set the same way main.rs does (current
//!      cursor differs from prior cursor) → expect exactly {A, D}.
//!   7. `translate_raw_dir_filtered(db, {A, D})` returns a
//!      TranslatedSlack whose messages cover only A's three messages
//!      and D's one — B and C never get pulled out of sqlite.
//!   8. Render pass 2 over the filtered TranslatedSlack → exactly
//!      two RenderedDocs fire the callback, each stamped with the
//!      probe's current cursor for its thread.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use frankweiler_etl_slack::extract::{
    block_on_load_filtered, block_on_probe_thread_cursors, MessageRow, RawDb,
};
use frankweiler_etl_slack::translate::{
    render::render_all, slack_thread_uuid, translate_loaded, translate_raw_dir_filtered, Message,
};
use serde_json::json;
use tempfile::tempdir;

async fn build_db(db_path: &Path, msgs: &[MessageRow]) {
    let db = RawDb::open(db_path).await.expect("open");
    for m in msgs {
        db.upsert_message(m).await.expect("upsert");
    }
}

fn msg(team: &str, chan: &str, ts: &str, thread_ts: Option<&str>, text: &str) -> MessageRow {
    let is_root = match thread_ts {
        None => true,
        Some(t) => t == ts,
    };
    MessageRow {
        team_id: team.into(),
        channel_id: chan.into(),
        ts: ts.into(),
        thread_ts: thread_ts.map(String::from),
        is_thread_root: is_root,
        user_id: Some("U1".into()),
        payload: json!({
            "ts": ts,
            "thread_ts": thread_ts,
            "user": "U1",
            "text": text,
        }),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cheap_probe_skips_unchanged_threads_on_resync() {
    let tmp = tempdir().expect("tmp");
    let db_path = tmp.path().join("s.doltlite_db");

    let thread_a_ts = "1700000000.000000";
    let thread_b_ts = "1700001000.000000";
    let thread_c_ts = "1700002000.000000"; // standalone
    let thread_d_ts = "1700003000.000000"; // added in pass 2

    // ── pass 1: build initial DB ────────────────────────────────────
    // Thread A: root + one reply. Thread B: root + two replies.
    // Thread C: a single standalone (no thread_ts).
    build_db(
        &db_path,
        &[
            msg("T1", "C1", thread_a_ts, Some(thread_a_ts), "A root"),
            msg("T1", "C1", "1700000010.000000", Some(thread_a_ts), "A r1"),
            msg("T1", "C1", thread_b_ts, Some(thread_b_ts), "B root"),
            msg("T1", "C1", "1700001010.000000", Some(thread_b_ts), "B r1"),
            msg("T1", "C1", "1700001020.000000", Some(thread_b_ts), "B r2"),
            msg("T1", "C1", thread_c_ts, None, "C standalone"),
        ],
    )
    .await;

    let uuid_a = slack_thread_uuid("T1", "C1", thread_a_ts);
    let uuid_b = slack_thread_uuid("T1", "C1", thread_b_ts);
    let uuid_c = slack_thread_uuid("T1", "C1", thread_c_ts);

    // Probe — three entries, each formatted "<max_fetched_at>|<count>".
    let cursors1 = block_on_probe_thread_cursors(&db_path).expect("probe");
    assert_eq!(cursors1.len(), 3, "expected one cursor per thread");
    assert!(cursors1.contains_key(&uuid_a));
    assert!(cursors1.contains_key(&uuid_b));
    assert!(cursors1.contains_key(&uuid_c));
    for cur in cursors1.values() {
        let (max_ts, count) = cur.split_once('|').expect("cursor shape");
        assert!(!max_ts.is_empty(), "max_ts in cursor: {cur:?}");
        assert!(count.parse::<u32>().is_ok(), "count parses as int: {cur:?}");
    }
    let count_a_1: u32 = cursors1[&uuid_a]
        .split_once('|')
        .unwrap()
        .1
        .parse()
        .unwrap();
    let count_b_1: u32 = cursors1[&uuid_b]
        .split_once('|')
        .unwrap()
        .1
        .parse()
        .unwrap();
    let count_c_1: u32 = cursors1[&uuid_c]
        .split_once('|')
        .unwrap()
        .1
        .parse()
        .unwrap();
    assert_eq!(count_a_1, 2); // root + 1 reply
    assert_eq!(count_b_1, 3); // root + 2 replies
    assert_eq!(count_c_1, 1); // standalone

    // Render pass 1 with empty priors — everything renders, captures
    // each thread's stamped cursor.
    let all_threads: HashSet<String> = cursors1.keys().cloned().collect();
    // translate_raw_dir_filtered runs `db_path_for(path)` internally,
    // which accepts either the `.doltlite_db` file or its dir-form
    // sibling. We built the DB at db_path directly, so pass that.
    let t1 = translate_raw_dir_filtered(&db_path, &all_threads).expect("filtered translate p1");
    let render_dir = tmp.path().join("rendered_p1");
    std::fs::create_dir_all(&render_dir).unwrap();

    let mut rendered_p1: HashMap<String, Option<String>> = HashMap::new();
    let _ = render_all(
        &t1,
        &render_dir,
        "slack_api",
        &frankweiler_etl::progress::Progress::noop(),
        &HashMap::new(),
        &cursors1,
        &mut |doc: frankweiler_etl::load::RenderedDoc| -> anyhow::Result<()> {
            rendered_p1.insert(doc.document_uuid.clone(), doc.upstream_cursor.clone());
            Ok(())
        },
    )
    .expect("render p1");
    assert_eq!(rendered_p1.len(), 3, "expected all three threads rendered");
    // Every rendered doc carries the probe's current cursor.
    for (tid, cur) in &rendered_p1 {
        assert_eq!(
            cur.as_deref(),
            Some(cursors1[tid].as_str()),
            "RenderedDoc.upstream_cursor must match the probe for {tid}",
        );
    }

    // ── pass 2: mutate the DB ───────────────────────────────────────
    // Add one new reply to thread A → its (max_fetched_at, count) both
    // move. Add a new standalone thread D → appears for the first
    // time. Leave B and C alone.
    build_db(
        &db_path,
        &[
            msg("T1", "C1", "1700000020.000000", Some(thread_a_ts), "A r2"),
            msg("T1", "C1", thread_d_ts, None, "D standalone"),
        ],
    )
    .await;

    let uuid_d = slack_thread_uuid("T1", "C1", thread_d_ts);

    let cursors2 = block_on_probe_thread_cursors(&db_path).expect("probe p2");
    assert_eq!(cursors2.len(), 4);
    assert_eq!(
        cursors2[&uuid_b], cursors1[&uuid_b],
        "untouched thread B's cursor must be byte-identical",
    );
    assert_eq!(
        cursors2[&uuid_c], cursors1[&uuid_c],
        "untouched thread C's cursor must be byte-identical",
    );
    assert_ne!(
        cursors2[&uuid_a], cursors1[&uuid_a],
        "mutated thread A's cursor must change",
    );
    let count_a_2: u32 = cursors2[&uuid_a]
        .split_once('|')
        .unwrap()
        .1
        .parse()
        .unwrap();
    assert_eq!(count_a_2, 3, "A's count should bump on the new reply");

    // Compute the orchestrator's filter set: priors are pass 1's
    // cursors; we expect exactly {A, D} to be selected for re-render.
    let prior_cursors = cursors1.clone();
    let changed: HashSet<String> = cursors2
        .iter()
        .filter(|(tid, cur)| prior_cursors.get(*tid).map(String::as_str) != Some(cur.as_str()))
        .map(|(tid, _)| tid.clone())
        .collect();
    let expected: HashSet<String> = [uuid_a.clone(), uuid_d.clone()].into_iter().collect();
    assert_eq!(
        changed, expected,
        "filter set should be {{A, D}}; B and C must be skipped",
    );

    // ── filtered load only pulls A and D ────────────────────────────
    let raw = block_on_load_filtered(&db_path, &changed).expect("filtered load");
    let loaded_thread_uuids: HashSet<String> = raw
        .messages
        .iter()
        .map(|m| {
            let eff = m.thread_ts.clone().unwrap_or_else(|| m.ts.clone());
            slack_thread_uuid(&m.team_id, &m.channel_id, &eff)
        })
        .collect();
    assert_eq!(
        loaded_thread_uuids, expected,
        "filtered load should yield messages only for the changed threads; \
         got {loaded_thread_uuids:?}",
    );
    // A: 3 messages (root + 2 replies). D: 1.
    assert_eq!(raw.messages.len(), 4);

    // ── render pass 2 over the filtered subset ──────────────────────
    let t2 = translate_loaded(raw);
    let messages_iter: Vec<&Message> = t2.messages.values().collect();
    assert!(
        messages_iter.iter().all(|m| {
            let u = m.thread_uuid();
            u == uuid_a || u == uuid_d
        }),
        "every message in the filtered TranslatedSlack must belong to A or D",
    );

    let render_dir2 = tmp.path().join("rendered_p2");
    std::fs::create_dir_all(&render_dir2).unwrap();
    let mut rendered_p2: HashMap<String, Option<String>> = HashMap::new();
    let _ = render_all(
        &t2,
        &render_dir2,
        "slack_api",
        &frankweiler_etl::progress::Progress::noop(),
        &HashMap::new(), // empty priors → both threads render
        &cursors2,
        &mut |doc: frankweiler_etl::load::RenderedDoc| -> anyhow::Result<()> {
            rendered_p2.insert(doc.document_uuid.clone(), doc.upstream_cursor.clone());
            Ok(())
        },
    )
    .expect("render p2");
    let rendered2_set: HashSet<String> = rendered_p2.keys().cloned().collect();
    assert_eq!(
        rendered2_set, expected,
        "render pass 2 callback should fire exactly for A and D",
    );
    // Stamped cursors match the current probe.
    for (tid, cur) in &rendered_p2 {
        assert_eq!(cur.as_deref(), Some(cursors2[tid].as_str()));
    }
}
