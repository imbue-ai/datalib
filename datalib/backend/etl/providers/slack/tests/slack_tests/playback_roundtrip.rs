//! Slack synth → playback → download round-trip.

use datalib_etl_slack::ingest::{block_on_load_all, db_path_for};
use datalib_etl_slack::recorded::History;
use serde_json::json;

use crate::support::{fetch_into, msg, record_general, Tree};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slack_synth_playback_extract_roundtrip() {
    let t = Tree::new();
    record_general(&t.api);
    History::cold("C1")
        .record(&t.api, json!([msg("1735689600.000000", "hello")]))
        .unwrap();
    assert_eq!(t.serve(), 4);

    let summary = fetch_into(&t.out, |o| o).await.unwrap();
    assert_eq!(summary.messages, 1);

    // One workspace, one channel, one user, one message — sourced from the
    // playback responses verbatim.
    let db_path = db_path_for(&t.out);
    assert!(db_path.exists(), "expected DB at {}", db_path.display());
    let raw = block_on_load_all(&db_path).expect("load db");
    let ws = raw.workspace.expect("workspace");
    assert_eq!(ws["team_id"], "T1");
    assert_eq!(raw.users.len(), 1);
    assert_eq!(raw.channels.len(), 1);
    assert_eq!(raw.messages.len(), 1);
    let m = &raw.messages[0];
    assert_eq!(m.channel_id, "C1");
    assert_eq!(m.ts, "1735689600.000000");
    assert_eq!(m.payload["text"], "hello");
}
