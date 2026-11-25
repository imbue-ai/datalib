//! A TNG account downloaded for real, over the synthesizer's tapes, then
//! read back and rendered: the store reads (JSONB payloads, the diff that
//! narrows a run, what each conversation declared reading) are what the
//! unit tests cannot reach.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;

use datalib_etl::doltlite_raw::commit_run;
use datalib_etl::progress::Progress;
use datalib_etl_discord::ingest::{self, db_path_for, RawDb};
use datalib_etl_discord::synthesize::write_world;
use datalib_etl_discord_config::DiscordSync;
use datalib_etl_discord_render::{ids, render, store};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::RawRange;
use datalib_etl_render::pages::PageSize;
use datalib_etl_web::playback;
use serde_json::{json, Value};

const PICARD: &str = "1701";
const RIKER: &str = "1702";
const SERVER: &str = "7000";
const BRIDGE: &str = "7001";
const THREAD: &str = "1235000000000000500";
const DM: &str = "9001";

fn user(id: &str, name: &str) -> Value {
    json!({"id": id, "username": name.to_lowercase(), "global_name": name, "discriminator": "0",
           "avatar": null, "banner": "x"})
}

fn message(channel: &str, id: &str, author: &Value, content: &str) -> Value {
    json!({"id": id, "channel_id": channel, "type": 0, "content": content, "author": author,
           "mentions": [], "mention_roles": [], "attachments": [], "embeds": [],
           "components": [], "edited_timestamp": null})
}

fn world(riker_is: &str, channel_is: &str) -> Value {
    let picard = user(PICARD, "Jean-Luc Picard");
    let riker = user(RIKER, riker_is);
    let thread = json!({"id": THREAD, "type": 11, "name": "Warp core", "guild_id": SERVER,
        "parent_id": BRIDGE, "last_message_id": "1235000000000000600",
        "thread_metadata": {"archived": true, "locked": false}});
    let mut starter = message(BRIDGE, THREAD, &riker, "The core is failing.");
    starter["thread"] = thread.clone();
    starter["attachments"] = json!([{"id": "5", "filename": "core.txt", "size": 4,
        "content_type": "text/plain",
        "url": "https://cdn.discordapp.com/attachments/1/5/core.txt?ex=7fffffff"}]);
    let mut reply = message(BRIDGE, "1235000000000000700", &picard, "Eject it, <@1702>.");
    reply["type"] = json!(19);
    reply["message_reference"] =
        json!({"type": 0, "channel_id": BRIDGE, "message_id": THREAD, "guild_id": SERVER});
    json!({
        "me": {"id": PICARD, "username": "jlpicard", "global_name": "Jean-Luc Picard",
               "email": "picard@enterprise.starfleet"},
        "guilds": [{"id": SERVER, "name": "USS Enterprise", "owner": true, "features": []}],
        "dms": [{"id": DM, "type": 1, "last_message_id": "1235000000000000800",
                 "recipients": [riker]}],
        "channels": {(SERVER): [{"id": BRIDGE, "type": 0, "name": channel_is,
            "guild_id": SERVER, "parent_id": null, "permission_overwrites": [],
            "last_message_id": "1235000000000000700"}]},
        "roles": {(SERVER): []},
        "members": {(SERVER): {"roles": []}},
        "threads": {(BRIDGE): [thread]},
        "messages": {
            (BRIDGE): [starter, reply],
            (THREAD): [message(THREAD, "1235000000000000600", &riker, "Two minutes.")],
            (DM): [message(DM, "1235000000000000800", &picard, "Poker?")],
        },
    })
}

async fn download(root: &Path, name: &str, world: &Value) -> String {
    let tape = root.join(name);
    let link = "https://cdn.discordapp.com/attachments/1/5/core.txt?ex=7fffffff";
    let files = BTreeMap::from([(link.to_string(), b"Hot!".to_vec())]);
    write_world(world, &files, &tape).unwrap();
    let db = RawDb::open(&db_path_for(&root.join("discord/ingest")))
        .await
        .unwrap();
    let now = datalib_time::parse_with_assumed_utc("2024-05-02T00:00:00Z").unwrap();
    let opts = ingest::FetchOptions {
        sync: DiscordSync {
            all_servers: true,
            list_every_hours: Some(0),
            refresh_every_minutes: Some(0),
            refresh_window_days: Some(3650),
            ..Default::default()
        },
        ..ingest::FetchOptions::new(db.clone(), now)
    };
    let s = playback::scope(&tape, ingest::fetch(opts)).await.unwrap();
    assert_eq!(s.conversations, 3, "{s:?}");
    let head = commit_run(db.pool(), name)
        .await
        .unwrap()
        .expect("a commit");
    db.close().await;
    head
}

struct Rendered {
    docs: BTreeMap<String, RenderedMarkdown>,
    /// Each conversation's declared inputs, as `table id`.
    inputs: BTreeMap<String, BTreeSet<String>>,
    render: Option<HashSet<String>>,
}

fn render_now(root: &Path, range: RawRange<'_>) -> Rendered {
    let s = store::open(&root.join("discord/ingest"), "discord", range)
        .unwrap()
        .expect("a store");
    assert!(s.unparsed.is_empty());
    let mut docs = BTreeMap::new();
    let mut inputs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    render::render_store(
        &s,
        "discord",
        PageSize::DEFAULT,
        &Progress::noop(),
        &mut |md| {
            docs.insert(md.markdown_uuid.clone(), md);
            Ok(())
        },
        &mut |page| {
            for unit in page.render_units {
                let declared = unit
                    .inputs
                    .iter()
                    .map(|i| format!("{} {}", i.table, i.id))
                    .collect();
                inputs.insert(unit.key, declared);
            }
            Ok(())
        },
    )
    .unwrap();
    let render = s.render.clone();
    s.close();
    Rendered {
        docs,
        inputs,
        render,
    }
}

fn text(md: &RenderedMarkdown) -> String {
    md.sections.iter().map(|s| s.md.as_str()).collect()
}

fn doc<'a>(r: &'a Rendered, channel: &str) -> &'a RenderedMarkdown {
    &r.docs[&ids::conversation("discord", channel).uuid]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_conversation_is_a_document_and_declares_what_it_read() {
    let d = tempfile::tempdir().unwrap();
    download(d.path(), "first", &world("Will Riker", "bridge")).await;
    let r = render_now(d.path(), RawRange::cold());
    assert_eq!((r.docs.len(), &r.render), (3, &None));

    let bridge = doc(&r, BRIDGE);
    let md = text(bridge);
    for wanted in [
        "The core is failing.",
        "/data/discord/ingest/blob/",
        "↪ [Will Riker](datalib:handle/discord/1702",
        "Eject it, [@Will Riker](datalib:handle/discord/1702",
    ] {
        assert!(md.contains(wanted), "no {wanted:?} in:\n{md}");
    }
    assert!(!md.contains("cdn.discordapp.com"), "a signed link: {md}");
    let top = bridge.rows.iter().find(|r| r.is_document).unwrap();
    assert_eq!(top.conversation_name.as_deref(), Some("#bridge"));
    assert_eq!(top.org_name.as_deref(), Some("USS Enterprise"));
    assert_eq!(top.account.as_deref(), Some("picard@enterprise.starfleet"));
    assert_eq!(top.item_count, Some(2));
    assert!(bridge.problems.is_empty(), "{:?}", bridge.problems);

    // The thread sits in the channel, which sits in the server.
    let places: Vec<Option<String>> = doc(&r, THREAD)
        .data_nodes
        .iter()
        .map(|n| n.name.clone())
        .collect();
    assert_eq!(
        places,
        [
            Some("USS Enterprise".to_string()),
            Some("#bridge".into()),
            Some("Warp core".into())
        ]
    );

    // A row a document reads and does not declare never renders it again.
    let key = |channel: &str| ids::conversation("discord", channel).uuid;
    let wanted = |channel: &str, rows: &[&str]| {
        for row in rows {
            assert!(
                r.inputs[&key(channel)].contains(*row),
                "{channel}: no {row}"
            );
        }
    };
    wanted(
        BRIDGE,
        &["users 1702", "channels 7001", "guilds 7000", "account 1701"],
    );
    wanted(
        THREAD,
        &["channels 7001", &format!("channels {THREAD}"), "users 1702"],
    );
    wanted(DM, &["users 1702", "channels 9001"]);
    assert!(r.inputs[&key(BRIDGE)]
        .iter()
        .any(|i| i.starts_with("discord_attachments ")));
}

/// A person's new name moves no message, so the forward scan names
/// nothing; it reaches the pages that declared reading them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_rename_reaches_a_page_through_what_it_declared() {
    let d = tempfile::tempdir().unwrap();
    let first = download(d.path(), "first", &world("Will Riker", "bridge")).await;
    download(
        d.path(),
        "second",
        &world("William T. Riker", "main-bridge"),
    )
    .await;
    let stale = HashSet::new();
    let range = RawRange {
        cursor: Some(&first),
        pin: None,
        stale: Some(&stale),
    };
    let r = render_now(d.path(), range);
    assert_eq!(r.render, Some(HashSet::new()), "no message changed");
    assert!(r.docs.is_empty());

    let stale = HashSet::from([ids::conversation("discord", BRIDGE).uuid]);
    let range = RawRange {
        cursor: Some(&first),
        pin: None,
        stale: Some(&stale),
    };
    let r = render_now(d.path(), range);
    assert_eq!(r.docs.len(), 1);
    let md = text(doc(&r, BRIDGE));
    assert!(md.contains("William T. Riker"), "{md}");
    let top = doc(&r, BRIDGE).rows.iter().find(|r| r.is_document).unwrap();
    assert_eq!(top.conversation_name.as_deref(), Some("#main-bridge"));
}

/// A new message names its own conversation and no other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_message_renders_its_conversation_alone() {
    let d = tempfile::tempdir().unwrap();
    let mut w = world("Will Riker", "bridge");
    let first = download(d.path(), "first", &w).await;
    w["messages"][DM].as_array_mut().unwrap().push(message(
        DM,
        "1235000000000000900",
        &user(RIKER, "Will Riker"),
        "Deal me in.",
    ));
    w["dms"][0]["last_message_id"] = json!("1235000000000000900");
    download(d.path(), "second", &w).await;
    let stale = HashSet::new();
    let range = RawRange {
        cursor: Some(&first),
        pin: None,
        stale: Some(&stale),
    };
    let r = render_now(d.path(), range);
    assert_eq!(r.render, Some(HashSet::from([DM.to_string()])));
    assert!(text(doc(&r, DM)).contains("Deal me in."));
}
