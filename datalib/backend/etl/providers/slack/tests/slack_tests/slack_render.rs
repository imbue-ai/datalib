//! Golden test for `slack::render` against the TNG fixture.

use std::collections::BTreeMap;
use std::path::PathBuf;

use datalib_etl_render::section::join;
use datalib_etl_slack_render::render::{ids, parse_raw_json_dir, render::render_all};
use insta::{assert_json_snapshot, assert_snapshot};

fn fixture_root() -> PathBuf {
    if let Ok(d) = std::env::var("SLACK_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/slack_api")
}

#[test]
fn renders_tng_fixture() {
    let parsed = parse_raw_json_dir(&fixture_root()).expect("parse");
    let mut docs: Vec<datalib_etl_render::grid_index::RenderedMarkdown> = Vec::new();
    let mut on_done =
        |doc: datalib_etl_render::grid_index::RenderedMarkdown| -> anyhow::Result<()> {
            docs.push(doc);
            Ok(())
        };
    let summary = render_all(
        &parsed,
        "slack_api",
        &datalib_etl::progress::Progress::noop(),
        &mut on_done,
    )
    .expect("render");
    // 7 channel threads + 5 DM threads (two 1:1 DMs and a group DM;
    // each standalone message is its own thread bucket).
    assert_eq!(summary.threads_total, 12);
    assert_eq!(summary.threads_rendered, 12);
    assert_eq!(docs.len(), 12);

    let md_tree: BTreeMap<&str, String> = docs
        .iter()
        .map(|d| (d.markdown_uuid.as_str(), join(&d.sections)))
        .collect();
    let mut bundle = String::new();
    for (uuid, body) in &md_tree {
        bundle.push_str("=== ");
        bundle.push_str(uuid);
        bundle.push_str(" ===\n");
        bundle.push_str(body);
        bundle.push('\n');
    }
    assert_snapshot!("tng_md_tree", bundle);

    // Every forward, preview, card and block the fixture holds is drawn
    // (in the snapshot above) but three: the away team's call, Worf's
    // attachment of buttons alone, and the roster's input box. Each
    // document holding one says so, naming it.
    let mut not_shown: Vec<(&str, &str)> = docs
        .iter()
        .flat_map(|d| {
            d.problems
                .iter()
                .map(move |p| (d.markdown_uuid.as_str(), p))
        })
        .filter(|(_, p)| matches!(p.field.as_deref(), Some("blocks" | "attachments")))
        .map(|(doc, p)| (doc, p.sample.as_str()))
        .collect();
    not_shown.sort();
    let thread = |channel, ts| ids::thread("slack_api", "T_NCC1701D", channel, ts).uuid;
    let call = thread("G_AWAYTEAM", "12604000904.000904");
    let buttons = thread("G_AWAYTEAM", "12604000903.000903");
    let roster = thread("D_RIKER", "12604000901.000901");
    let mut expected = vec![
        (
            call.as_str(),
            "1 message here holds what is not shown: call block",
        ),
        (
            buttons.as_str(),
            "1 message here holds what is not shown: attachment with nothing to show",
        ),
        (
            roster.as_str(),
            "1 message here holds what is not shown: input block",
        ),
    ];
    expected.sort();
    assert_eq!(not_shown, expected);

    // Emitted-document spot-check. This used to walk the tree counting
    // `*.grid_rows.json` files; the renderer no longer writes those, so
    // it checks what it hands to its callback — which is what both the
    // store and the index consume.
    assert_eq!(docs.len(), 12, "one document per rendered thread");
    assert!(!docs[0].rows.is_empty(), "and the rows it projected");
}

#[test]
fn renders_tng_fixture_grid_rows() {
    let parsed = parse_raw_json_dir(&fixture_root()).expect("parse");

    // Capture every grid row the chat-common renderer emits.
    let mut rows: Vec<serde_json::Value> = Vec::new();
    {
        let mut on_done =
            |doc: datalib_etl_render::grid_index::RenderedMarkdown| -> anyhow::Result<()> {
                for r in &doc.rows {
                    rows.push(serde_json::to_value(r).unwrap());
                }
                Ok(())
            };
        render_all(
            &parsed,
            "slack_api",
            &datalib_etl::progress::Progress::noop(),
            &mut on_done,
        )
        .expect("render");
    }

    let picard_root_uuid =
        ids::message("slack_api", "T_NCC1701D", "C_BRIDGE", "12604000100.000100").uuid;
    let picard_thread_uuid =
        ids::thread("slack_api", "T_NCC1701D", "C_BRIDGE", "12604000100.000100").uuid;

    let field = |r: &serde_json::Value, k: &str| -> Option<String> {
        r.get(k).and_then(|v| v.as_str()).map(str::to_string)
    };

    // The thread-level row: stable thread uuid, channel, permalink in
    // source_url, title text preserved as conversation_name.
    let thread_row = rows
        .iter()
        .find(|r| {
            field(r, "kind").as_deref() == Some("Slack Thread")
                && field(r, "conversation_uuid").as_deref() == Some(&picard_thread_uuid)
        })
        .expect("Picard thread row");
    assert_eq!(
        field(thread_row, "uuid").as_deref(),
        Some(picard_thread_uuid.as_str())
    );
    // chat-common writes the single `display` ("#bridge") as the subject
    // alone; a chat is filed nowhere else, so it has no channel.
    assert_eq!(field(thread_row, "channel"), None);
    assert_eq!(
        field(thread_row, "conversation_name").as_deref(),
        Some("#bridge")
    );
    // On the workspace's own host, which `auth.test` named: a bare
    // slack.com link opens in whichever workspace the browser is in.
    assert_eq!(
        field(thread_row, "source_url").as_deref(),
        Some("https://enterprise-d.slack.com/archives/C_BRIDGE/p12604000100000100")
    );

    // The root message row keeps its own message uuid + index 0.
    let root_msg = rows
        .iter()
        .find(|r| {
            field(r, "kind").as_deref() == Some("Slack Message")
                && field(r, "uuid").as_deref() == Some(&picard_root_uuid)
        })
        .expect("Picard root message row");
    assert_eq!(
        root_msg.get("message_index").and_then(|v| v.as_i64()),
        Some(0)
    );
    assert_ne!(
        field(root_msg, "uuid").as_deref(),
        Some(picard_thread_uuid.as_str())
    );
    assert_eq!(
        field(root_msg, "source_url").as_deref(),
        Some("https://enterprise-d.slack.com/archives/C_BRIDGE/p12604000100000100")
    );
    let reply_uuid = ids::message("slack_api", "T_NCC1701D", "C_BRIDGE", "12604000200.000200").uuid;
    let reply = rows
        .iter()
        .find(|r| field(r, "uuid").as_deref() == Some(&reply_uuid))
        .expect("the thread's first reply row");
    assert_eq!(
        field(reply, "source_url").as_deref(),
        Some(
            "https://enterprise-d.slack.com/archives/C_BRIDGE/p12604000200000200\
             ?thread_ts=12604000100.000100&cid=C_BRIDGE"
        )
    );

    rows.sort_by_key(|v| {
        (
            field(v, "kind").unwrap_or_default(),
            field(v, "created_at").unwrap_or_default(),
            field(v, "uuid").unwrap_or_default(),
        )
    });
    assert_json_snapshot!("tng_grid_rows", rows);
}
