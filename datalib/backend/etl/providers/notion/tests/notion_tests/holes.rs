//! A body's holes end up one of three ways: filled where the hole was,
//! nothing a reader would miss, or a `problems` row saying what is lost.
//! None of them may vanish quietly.

use std::path::{Path, PathBuf};

use datalib_etl_notion::ingest::official::BASE;
use datalib_etl_notion::ingest::RawDb;
use serde_json::{json, Value};
use tempfile::{tempdir, TempDir};

use crate::support::*;

const HOLE: &str = "1701d000-0000-4000-8000-0000000000a1";
const INNER_HOLE: &str = "1701d000-0000-4000-8000-0000000000a2";

fn marker(id: &str, alt: Option<&str>) -> String {
    let alt = alt.map(|a| format!(" alt=\"{a}\"")).unwrap_or_default();
    format!(
        "<unknown url=\"https://www.notion.so/x#{}\"{alt}/>",
        id.replace('-', "")
    )
}

struct Bridge {
    _dir: TempDir,
    tape: PathBuf,
    store: PathBuf,
}

fn bridge_with_body(markdown: &str, truncated: bool) -> Bridge {
    let dir = tempdir().unwrap();
    let (tape, store) = (dir.path().join("tape"), dir.path().join("s.doltlite_db"));
    serve_workspace(&tape, &[BRIDGE]);
    serve_object(&tape, BRIDGE, EDITED);
    serve_comments(&tape, BRIDGE);
    serve_body(&tape, BRIDGE, markdown, truncated);
    Bridge {
        _dir: dir,
        tape,
        store,
    }
}

fn serve_block(tape: &Path, id: &str, kind: &str, content: Value) {
    let mut block = json!({"object": "block", "id": id, "type": kind, "has_children": false});
    block[kind] = content;
    serve(tape, &format!("{BASE}/blocks/{id}"), block);
}

async fn stored(store: &Path) -> (String, Vec<String>) {
    let db = RawDb::open(store).await.unwrap();
    let (markdown, unresolved): (String, Option<String>) =
        sqlx::query_as("SELECT markdown, unresolved_block_ids FROM page_markdown WHERE id = ?")
            .bind(BRIDGE)
            .fetch_one(db.pool())
            .await
            .unwrap();
    db.close().await;
    let unresolved = unresolved
        .map(|u| serde_json::from_str(&u).unwrap())
        .unwrap_or_default();
    (markdown, unresolved)
}

fn warning_on_the_bridge() -> Vec<(String, String)> {
    vec![row(&format!("page_markdown:{BRIDGE}"), "warning")]
}

/// A followed subtree landed at the end of the page, its marker stayed
/// behind and its id stayed on the unresolved list, so a filled hole
/// looked like an open one and a reader saw its text out of order.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_filled_hole_is_in_its_place_and_is_no_longer_a_hole() {
    let b = bridge_with_body(&format!("Before\n{}\nAfter\n", marker(HOLE, None)), true);
    serve_body(&b.tape, HOLE, "Middle\n", false);

    run(&b.tape, &b.store, &[BRIDGE]).await.unwrap();

    let (markdown, unresolved) = stored(&b.store).await;
    assert_eq!(markdown, "Before\nMiddle\nAfter\n");
    assert_eq!(unresolved, Vec::<String>::new());
    assert_eq!(problems(&b.store).await, vec![]);
}

/// A subtree can be a block with nothing in it, which is not a loss.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hole_that_turns_out_empty_is_gone() {
    let b = bridge_with_body(&format!("Before\n{}\nAfter\n", marker(HOLE, None)), true);
    serve_body(&b.tape, HOLE, "", false);

    run(&b.tape, &b.store, &[BRIDGE]).await.unwrap();

    let (markdown, unresolved) = stored(&b.store).await;
    assert_eq!(markdown, "Before\n\nAfter\n");
    assert_eq!(unresolved, Vec::<String>::new());
    assert_eq!(problems(&b.store).await, vec![]);
}

/// A followed subtree that was itself truncated had its own holes
/// dropped: no follow-up, no row, and the text simply missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hole_inside_a_hole_is_followed() {
    let b = bridge_with_body(&format!("Before\n{}\nAfter\n", marker(HOLE, None)), true);
    serve_body(
        &b.tape,
        HOLE,
        &format!("Outer\n{}\n", marker(INNER_HOLE, None)),
        true,
    );
    serve_body(&b.tape, INNER_HOLE, "Inner\n", false);

    run(&b.tape, &b.store, &[BRIDGE]).await.unwrap();

    let (markdown, unresolved) = stored(&b.store).await;
    assert_eq!(markdown, "Before\nOuter\nInner\nAfter\n");
    assert_eq!(unresolved, Vec::<String>::new());
    assert_eq!(problems(&b.store).await, vec![]);
}

/// A subtree answering with its own marker must not be asked for forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hole_answering_with_itself_ends_as_a_row() {
    let b = bridge_with_body(&format!("Before\n{}\n", marker(HOLE, None)), true);
    serve_body(&b.tape, HOLE, &format!("{}\n", marker(HOLE, None)), true);

    run(&b.tape, &b.store, &[BRIDGE]).await.unwrap();

    assert_eq!(problems(&b.store).await, warning_on_the_bridge());
    assert_eq!(stored(&b.store).await.1, vec![HOLE.to_string()]);
}

/// A 404 on a subtree was taken to mean the block went a moment ago, but
/// Notion answers it for a block the credential cannot see too, and the
/// page was then held with text missing and nothing saying so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hole_notion_will_not_show_is_a_warning_and_the_body_is_kept() {
    let b = bridge_with_body(&format!("Before\n{}\n", marker(HOLE, None)), true);
    serve_status(&b.tape, &format!("{BASE}/pages/{HOLE}/markdown"), 404);

    run(&b.tape, &b.store, &[BRIDGE]).await.unwrap();

    assert_eq!(problems(&b.store).await, warning_on_the_bridge());
    let (markdown, unresolved) = stored(&b.store).await;
    assert!(markdown.starts_with("Before\n"), "{markdown}");
    assert_eq!(unresolved, vec![HOLE.to_string()]);
}

/// Bookmarks, embeds and link previews were markers with their URLs
/// nowhere in the mirror, though asking for the block returns them. This
/// holds with `truncated: false`, which Notion answers on such a body.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_block_markdown_cannot_carry_keeps_its_link() {
    let mut text = String::from("Reading\n");
    for (id, alt) in [
        ("1701d000-0000-4000-8000-0000000000b1", "bookmark"),
        ("1701d000-0000-4000-8000-0000000000b2", "tweet"),
        (
            "1701d000-0000-4000-8000-0000000000b3",
            "external_object_instance",
        ),
    ] {
        text.push_str(&marker(id, Some(alt)));
        text.push('\n');
    }
    let b = bridge_with_body(&text, false);
    serve_block(
        &b.tape,
        "1701d000-0000-4000-8000-0000000000b1",
        "bookmark",
        json!({"url": "https://example.com/treaty", "caption": [{"plain_text": "Khitomer [draft]"}]}),
    );
    serve_block(
        &b.tape,
        "1701d000-0000-4000-8000-0000000000b2",
        "embed",
        json!({"url": "https://example.com/post?a=(1)", "caption": []}),
    );
    serve_block(
        &b.tape,
        "1701d000-0000-4000-8000-0000000000b3",
        "link_preview",
        json!({"url": "https://example.com/pr/1"}),
    );

    run(&b.tape, &b.store, &[BRIDGE]).await.unwrap();

    let (markdown, unresolved) = stored(&b.store).await;
    assert_eq!(
        markdown,
        "Reading\n\
         [Khitomer \\[draft\\]](<https://example.com/treaty>)\n\
         [https://example.com/post?a=(1)](<https://example.com/post?a=(1)>)\n\
         [https://example.com/pr/1](<https://example.com/pr/1>)\n"
    );
    assert_eq!(unresolved, Vec::<String>::new());
    assert_eq!(problems(&b.store).await, vec![]);
}

/// A URL came from upstream, and what we write gets drawn as a link, so
/// only http and https become one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_link_that_is_not_http_stays_a_row() {
    let b = bridge_with_body(&format!("{}\n", marker(HOLE, Some("bookmark"))), false);
    serve_block(
        &b.tape,
        HOLE,
        "bookmark",
        json!({"url": "javascript:alert(1)", "caption": []}),
    );

    run(&b.tape, &b.store, &[BRIDGE]).await.unwrap();

    assert_eq!(problems(&b.store).await, warning_on_the_bridge());
    let (markdown, unresolved) = stored(&b.store).await;
    assert!(!markdown.contains("javascript:"), "{markdown}");
    assert_eq!(unresolved, vec![HOLE.to_string()]);
}

/// An alias or an AI block is content the API will not give us. It was
/// an `info` line in the log, which reaches nobody.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_block_the_api_will_not_show_is_a_warning_naming_it() {
    let b = bridge_with_body(&format!("Before\n{}\n", marker(HOLE, Some("alias"))), true);
    serve_block(&b.tape, HOLE, "unsupported", json!({"block_type": "alias"}));

    run(&b.tape, &b.store, &[BRIDGE]).await.unwrap();

    assert_eq!(problems(&b.store).await, warning_on_the_bridge());
    let db = RawDb::open(&b.store).await.unwrap();
    let said: String = sqlx::query_scalar("SELECT sample FROM problems")
        .fetch_one(db.pool())
        .await
        .unwrap();
    db.close().await;
    assert!(said.contains("alias"), "{said}");
    assert_eq!(stored(&b.store).await.1, vec![HOLE.to_string()]);
}

/// A button is an action, a breadcrumb is navigation: nothing is lost,
/// so no row and no request. The tape has no answer for these blocks, so
/// asking would fail the body.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_button_loses_nothing_and_costs_nothing() {
    let b = bridge_with_body(
        &format!(
            "{}\n{}\n",
            marker(HOLE, Some("button")),
            marker(INNER_HOLE, Some("breadcrumb"))
        ),
        true,
    );

    run(&b.tape, &b.store, &[BRIDGE]).await.unwrap();

    assert_eq!(problems(&b.store).await, vec![]);
    assert_eq!(stored(&b.store).await.1.len(), 2);
}

/// A body stored by an earlier build is held until its page is edited, so
/// without this rung its filled holes stay misplaced and its losses stay
/// unreported. One with nothing to gain is not asked for again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_body_an_earlier_build_stored_is_read_again_only_if_it_gains() {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    let pages = [BRIDGE, SICKBAY, HOLODECK];
    serve_workspace(&tape, &pages);
    for id in pages {
        serve_object(&tape, id, EDITED);
        serve_comments(&tape, id);
        serve_body(&tape, id, "As Notion has it now.\n", false);
    }
    run(&tape, &store, &pages).await.unwrap();

    let db = RawDb::open(&store).await.unwrap();
    for (id, markdown) in [
        (
            BRIDGE,
            format!("{}\nFilled, at the end.\n", marker(HOLE, None)),
        ),
        (SICKBAY, format!("{}\n", marker(HOLE, Some("button")))),
    ] {
        sqlx::query("UPDATE page_markdown SET markdown = ? WHERE id = ?")
            .bind(markdown)
            .bind(id)
            .execute(db.pool())
            .await
            .unwrap();
    }
    sqlx::query("UPDATE _datalib_meta SET value = '2' WHERE key = 'schema_version'")
        .execute(db.pool())
        .await
        .unwrap();
    datalib_etl::doltlite_raw::commit_run(db.pool(), "an earlier build")
        .await
        .unwrap();
    db.close().await;

    let db = RawDb::open(&store).await.expect("the rung carries it");
    let held: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT id, held_version FROM page_markdown_bookkeeping ORDER BY id")
            .fetch_all(db.pool())
            .await
            .unwrap();
    db.close().await;
    let at_edited = |id: &str| (id.to_string(), Some(EDITED.to_string()));
    assert_eq!(
        held,
        [
            (BRIDGE.to_string(), None),
            at_edited(SICKBAY),
            at_edited(HOLODECK)
        ]
    );

    run(&tape, &store, &pages).await.unwrap();
    assert_eq!(stored(&store).await.0, "As Notion has it now.\n");
}
