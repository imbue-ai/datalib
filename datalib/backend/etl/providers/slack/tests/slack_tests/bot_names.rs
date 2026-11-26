//! A bot posts with a `bot_id` and, most often, no user, so its name is
//! nowhere in its messages. Each bot a stored message names is asked
//! `bots.info` once, and its name kept in `bots`; one whose lookup fails
//! is a problem on its own row, owed still.

use datalib_etl_slack::ingest::{db_path_for, RawDb};
use datalib_etl_slack::recorded::{record_bot_info, record_call, record_workspace};
use serde_json::json;

use crate::support::{fetch_into, Tree};
use datalib_sql::test_sql;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_bot_a_message_names_is_looked_up_once() {
    let t = Tree::new();
    record_workspace(&t.api, &["C1"], |_, _| {
        json!([
            {"ts": "1735689600.000300", "subtype": "bot_message", "bot_id": "B_SENSORS",
             "text": "Contact at bearing 271 mark 4."},
            {"ts": "1735689600.000200", "subtype": "bot_message", "bot_id": "B_GONE",
             "text": "Shuttle bay doors open."},
            {"ts": "1735689600.000100", "subtype": "bot_message", "bot_id": "B_SENSORS",
             "text": "Sweep complete."},
        ])
    })
    .unwrap();
    record_bot_info(
        &t.api,
        json!({"id": "B_SENSORS", "name": "Sensor Array", "deleted": false, "updated": 1735689600}),
    )
    .unwrap();
    record_call(
        &t.api,
        "bots.info",
        json!({"bot": "B_GONE"}),
        json!({"ok": false, "error": "bot_not_found"}),
    )
    .unwrap();
    t.serve();

    fetch_into(&t.playback, &t.out, |o| o).await.unwrap();

    let db = RawDb::open(&db_path_for(&t.out)).await.unwrap();
    let bots: Vec<(String, Option<String>, bool)> =
        test_sql!("SELECT id, name, payload IS NOT NULL FROM bots ORDER BY id")
            .query_as()
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(
        bots,
        [
            ("B_GONE".to_string(), None, false),
            (
                "B_SENSORS".to_string(),
                Some("Sensor Array".to_string()),
                true
            ),
        ]
    );
    let updated: Option<String> =
        test_sql!("SELECT json_extract(payload, '$.updated') FROM bots WHERE id = 'B_SENSORS'")
            .query_scalar()
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(updated, None, "`updated` moves with the app, not the bot");
    let mut conn = db.pool().acquire().await.unwrap();
    let owed: Vec<String> = db
        .bots_owed(&mut conn)
        .await
        .unwrap()
        .into_iter()
        .map(|l| l.key)
        .collect();
    drop(conn);
    assert_eq!(
        owed,
        ["B_GONE"],
        "the bot Slack would not name is asked again"
    );
    let problems: Vec<String> =
        test_sql!("SELECT scope_key FROM problems WHERE scope_key LIKE 'bots:%'")
            .query_scalar()
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(problems, ["bots:B_GONE"]);
    db.close().await;
}
