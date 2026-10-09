//! A conversation branched off another ("Branch in new chat") carries
//! the original's messages under the same ids.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use datalib_etl_chatgpt_render::render::parse::parse_api_dir;
use datalib_etl_chatgpt_render::render::render::render_all;
use serde_json::json;

/// `(id, parent, role, text)`; each message a second after the last.
type Message<'a> = (&'a str, &'a str, &'a str, &'a str);

fn write_conversation(dir: &Path, id: &str, title: &str, messages: &[Message<'_>]) {
    let mut mapping = serde_json::Map::new();
    mapping.insert(
        "client-created-root".to_string(),
        json!({"id": "client-created-root", "message": null, "parent": null}),
    );
    for (i, (mid, parent, role, text)) in messages.iter().enumerate() {
        let message = json!({
            "id": mid,
            "author": {"role": role},
            "create_time": 12648384001.0 + i as f64,
            "content": {"content_type": "text", "parts": [text]},
            "metadata": {},
        });
        mapping.insert(
            mid.to_string(),
            json!({"id": mid, "message": message, "parent": parent}),
        );
    }
    let conv = json!({
        "conversation_id": id,
        "title": title,
        "create_time": 12648384000.0,
        "current_node": messages.last().unwrap().0,
        "mapping": mapping,
    });
    fs::write(
        dir.join("conversations").join(format!("{id}.json")),
        serde_json::to_string(&conv).unwrap(),
    )
    .unwrap();
}

/// Rendering every branch (#1055) put the original's copy of a branched
/// prefix on its page beside the branch's own, and a message id keyed
/// alone minted one row uuid for both: the index load refused the
/// second with "UNIQUE constraint failed: grid_rows.uuid".
#[test]
fn a_conversation_branched_into_a_new_chat_mints_its_own_row_ids() {
    let tmp = tempfile::tempdir().unwrap();
    let api = tmp.path().join("chatgpt_api");
    fs::create_dir_all(api.join("conversations")).unwrap();

    // The original: Picard asked, then edited his prompt; the version
    // shown is the edit, the first exchange a folded branch.
    const ROOT: &str = "client-created-root";
    const ASKED: Message = ("msg-tng-holo-0001", ROOT, "user", "Load Dixon Hill.");
    const ANSWERED: Message = (
        "msg-tng-holo-0002",
        "msg-tng-holo-0001",
        "assistant",
        "Loaded.",
    );
    write_conversation(
        &api,
        "68fb0001-fake-7000-8000-holodeck00001",
        "Dixon Hill, first draft",
        &[
            ASKED,
            ANSWERED,
            ("msg-tng-holo-0003", ROOT, "user", "Load Dixon Hill, 1941."),
            (
                "msg-tng-holo-0004",
                "msg-tng-holo-0003",
                "assistant",
                "1941.",
            ),
        ],
    );
    // Branched into a new chat from the first exchange: its prefix is
    // the original's messages, ids and all.
    write_conversation(
        &api,
        "68fb0002-fake-7000-8000-holodeck00002",
        "Dixon Hill, branched",
        &[
            ASKED,
            ANSWERED,
            (
                "msg-tng-holo-0005",
                "msg-tng-holo-0002",
                "user",
                "Add Whalen.",
            ),
            (
                "msg-tng-holo-0006",
                "msg-tng-holo-0005",
                "assistant",
                "At the bar.",
            ),
        ],
    );

    let parsed = parse_api_dir(&api, "chatgpt_api").expect("parse");
    let mut docs = Vec::new();
    render_all(
        &parsed,
        &tmp.path().join("out"),
        "chatgpt_api",
        &datalib_etl::progress::Progress::noop(),
        &mut |doc| {
            docs.push(doc);
            Ok(())
        },
    )
    .expect("render");
    assert_eq!(docs.len(), 2);

    let mut owner: HashMap<String, String> = HashMap::new();
    for doc in &docs {
        for row in &doc.rows {
            if let Some(other) = owner.insert(row.uuid.clone(), doc.markdown_uuid.clone()) {
                panic!(
                    "row {} ({:?}) is in markdown {other} and {}",
                    row.uuid, row.upstream_id, doc.markdown_uuid
                );
            }
        }
    }
    // The shared prefix is on both pages: 4 + 4 messages, 2 chat rows.
    assert_eq!(owner.len(), 10);
}
