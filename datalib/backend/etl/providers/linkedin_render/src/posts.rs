//! Render the user's own LinkedIn posts and the comments they left,
//! grouped into one chat-style thread per post.

use std::collections::BTreeMap;
use std::collections::HashMap;

use anyhow::Result;
use datalib_etl::blob_cas::BlobBundle;
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::render::{
    render_all as cc_render_all, RenderProfile, ENTITY_KIND_CONVERSATION,
};
use datalib_etl_chat_common::types::{
    ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem, NormalizedDoc,
};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::{changed_rows, Bucket, Input, Inputs};
use serde_json::Value;

use datalib_etl_linkedin::ingest::schema_raw::ns_id as uuid5;
use datalib_etl_linkedin::ingest::{db_path_for, RawDb};

use crate::processor::{FeedOutcome, Source};

use crate::render::{parse_date_ms, RENDER_VERSION};
use datalib_schema::providers::Provider;

/// Author label for the export owner. Every share and comment in these
/// two feeds is something the user themselves wrote.
const ME: &str = "Me";

fn profile() -> RenderProfile {
    RenderProfile {
        stamp_precision: datalib_etl_chat_common::RecordStampPrecision::Seconds,
        provider: Provider::Linkedin,
        source_label: "LinkedIn".to_string(),
        chat_kind: "LinkedIn Post".to_string(),
        message_kind: "LinkedIn Post Message".to_string(),
        reaction_kind: "LinkedIn Post Reaction".to_string(),
        chat_entity_kind: ENTITY_KIND_CONVERSATION,
        render_version: RENDER_VERSION,
    }
}

pub fn render_posts(
    source: &Source<'_>,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<FeedOutcome> {
    let Source {
        raw_dir,
        out_dir,
        name: source_id,
        account,
        account_inputs,
        range,
    } = *source;
    let db_path = db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(FeedOutcome::default());
    }

    let Some((shares, comments, changed, new_head)) = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let db = RawDb::open_reader(&db_path).await?;
            // Read at a commit: this store belongs to the download step, and
            // nothing committed means nothing to render from.
            let Some(pin) = range.pin(db.pool()).await? else {
                db.close().await;
                return Ok(None);
            };
            datalib_etl::pin::install_views(db.pool(), &pin).await?;
            // A feed the user didn't export has no table; treat a load
            // error as "absent" rather than failing the render.
            let shares = datalib_etl::doltlite_raw::load_payloads_with_id(
                db.pool(),
                datalib_etl::pin::Reads::At(&pin),
                "shares",
            )
            .await
            .unwrap_or_default();
            let comments = datalib_etl::doltlite_raw::load_payloads_with_id(
                db.pool(),
                datalib_etl::pin::Reads::At(&pin),
                "comments",
            )
            .await
            .unwrap_or_default();
            let changed = changed_rows(db.pool(), range, &pin, &["shares", "comments"]).await?;
            // Closed, not dropped: the next open of this store is a
            // second connection until this one is actually gone.
            db.close().await;
            Ok::<_, anyhow::Error>(Some((shares, comments, changed, pin.commit().to_string())))
        })
    })?
    else {
        return Ok(FeedOutcome::default());
    };

    let mut chats = build_post_chats(&shares, &comments, account, account_inputs);

    // What to render: the threads the driver found stale, plus the ones a
    // new or changed row maps to through the rows just loaded. A removed
    // row's thread reaches here through the driver, having declared the
    // row.
    let forward = changed.map(|changed| {
        chats
            .iter()
            .filter(|c| {
                c.inputs
                    .iter()
                    .any(|i| changed.get(&i.table).is_some_and(|ids| ids.contains(&i.id)))
            })
            .map(|c| c.chat_uuid.clone())
            .collect::<std::collections::HashSet<String>>()
    });
    let render = range.narrow(forward.as_ref());
    let mut outcome = FeedOutcome {
        new_head: Some(new_head),
        buckets: render
            .iter()
            .flatten()
            .map(|key| Bucket {
                key: key.clone(),
                inputs: Vec::new(),
            })
            .collect(),
    };
    if let Some(render) = &render {
        chats.retain(|c| render.contains(&c.chat_uuid));
    }

    let blobs: HashMap<String, BlobBundle> = HashMap::new();
    let s = cc_render_all(
        &profile(),
        &chats,
        out_dir,
        source_id,
        &blobs,
        progress,
        on_doc_complete,
    )?;
    outcome.buckets.extend(s.buckets);
    Ok(outcome)
}

/// One share + its comments, sharing a post key.
struct Thread<'a> {
    /// Representative full post URL for the linkout (first row seen).
    url: String,
    /// The rows this thread was built from.
    inputs: Inputs,
    share: Option<&'a Value>,
    comments: Vec<&'a Value>,
}

/// Shares and comments as `(row id, payload)`: the ids are what each
/// thread declares it read, beside the account rows every document
/// carries.
fn build_post_chats(
    shares: &[(String, Value)],
    comments: &[(String, Value)],
    account: Option<&str>,
    account_inputs: &[Input],
) -> Vec<NormalizedChat> {
    // BTreeMap keeps thread order stable across runs.
    let mut by_post: BTreeMap<String, Thread> = BTreeMap::new();

    for (i, (row_id, s)) in shares.iter().enumerate() {
        let link = field(s, "ShareLink");
        let key = thread_key(link, &format!("share:{i}"));
        let t = by_post.entry(key).or_insert_with(|| Thread {
            url: link.to_string(),
            inputs: Inputs::default(),
            share: None,
            comments: Vec::new(),
        });
        if t.url.is_empty() {
            t.url = link.to_string();
        }
        t.inputs.read("shares", row_id);
        // Keep the first share if a key somehow repeats (shouldn't).
        t.share.get_or_insert(s);
    }
    for (i, (row_id, c)) in comments.iter().enumerate() {
        let link = field(c, "Link");
        let key = thread_key(link, &format!("comment:{i}"));
        let t = by_post.entry(key).or_insert_with(|| Thread {
            url: link.to_string(),
            inputs: Inputs::default(),
            share: None,
            comments: Vec::new(),
        });
        if t.url.is_empty() {
            t.url = link.to_string();
        }
        t.inputs.read("comments", row_id);
        t.comments.push(c);
    }

    let mut chats = Vec::with_capacity(by_post.len());
    for (key, thread) in by_post {
        for input in account_inputs {
            thread.inputs.read(&input.table, &input.id);
        }
        let mut items: Vec<NormalizedChatItem> = Vec::new();

        // Opening message: the post itself, or a note that the original
        // isn't in the export when we only have comments on it.
        if let Some(s) = thread.share {
            let date = field(s, "Date");
            let mut body = nonempty(field(s, "ShareCommentary"))
                .unwrap_or("")
                .to_string();
            // Append the shared link / media so a link-only repost still
            // has a non-empty body.
            for k in ["SharedUrl", "MediaUrl"] {
                if let Some(u) = nonempty(field(s, k)) {
                    if !body.is_empty() {
                        body.push_str("\n\n");
                    }
                    body.push_str(u);
                }
            }
            items.push(me_item(&key, "post", date, body, &thread.url));
        } else {
            // Earliest *real* comment date, or `None` when none of the
            // comments carry one — the placeholder then has no
            // timestamp to report rather than a fabricated epoch.
            let earliest = thread
                .comments
                .iter()
                .filter_map(|c| parse_date_ms(field(c, "Date")))
                .min();
            items.push(post_placeholder(&key, earliest, &thread.url));
        }

        // Comments, oldest-first.
        let mut crows = thread.comments.clone();
        crows.sort_by_key(|c| parse_date_ms(field(c, "Date")));
        for c in crows {
            let date = field(c, "Date");
            let body = nonempty(field(c, "Message")).unwrap_or("").to_string();
            items.push(me_item(&key, "comment", date, body, &thread.url));
        }

        chats.push(NormalizedChat {
            inputs: thread.inputs.declared(),
            path_prefix: None,
            id: format!("posts:{key}"),
            chat_uuid: uuid5(&format!("chat:posts:{key}")),
            display: thread_title(thread.share, &thread.comments),
            title: None,
            author: None,
            account: account.map(str::to_string),
            project: None,
            external_id: nonempty(&key).map(str::to_string),
            // Whole-post linkout on the thread header / chat-level row.
            source_url: nonempty(&thread.url).map(str::to_string),
            upstream_scope: None,
            org_uuid: None,
            org_name: None,
            buckets: vec![NormalizedDoc {
                orphan_reactions: Vec::new(),
                period_key: "all".to_string(),
                markdown_uuid: uuid5(&format!("doc:posts:{key}:all")),
                items,
            }],
        });
    }
    chats
}

fn me_item(key: &str, role: &str, date: &str, body: String, url: &str) -> NormalizedChatItem {
    let mut text = body;
    if let Some(u) = nonempty(url) {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&format!("[🔗 View on LinkedIn]({u})"));
    }
    NormalizedChatItem {
        message_uuid: uuid5(&format!("msg:posts:{key}:{role}:{date}:{text}")),
        author_id: "me".to_string(),
        author_display: ME.to_string(),
        date_ms: parse_date_ms(date),
        text: nonempty(&text).map(str::to_string),
        kind: ItemKind::Text,
        attachments: linkout(url),
        reactions: Vec::new(),
        system_note: None,
        source_url: None,
        kind_label: None,
        source_ref: None,
        is_aside: false,
    }
}

fn post_placeholder(key: &str, date_ms: Option<i64>, url: &str) -> NormalizedChatItem {
    let note = match nonempty(url) {
        Some(u) => format!("Original post not included in the LinkedIn export — {u}"),
        None => "Original post not included in the LinkedIn export.".to_string(),
    };
    NormalizedChatItem {
        message_uuid: uuid5(&format!("msg:posts:{key}:origin")),
        author_id: "linkedin".to_string(),
        author_display: "LinkedIn".to_string(),
        date_ms,
        text: None,
        kind: ItemKind::System,
        attachments: linkout(url),
        reactions: Vec::new(),
        system_note: Some(note),
        source_url: None,
        kind_label: None,
        source_ref: None,
        is_aside: false,
    }
}

/// A path-less attachment whose only job is to carry the post URL into
/// the grid row's `source_url`. The chat renderer draws attachments only
/// for attachment-kind items, so this stays invisible in the transcript.
fn linkout(url: &str) -> Vec<NormalizedAttachment> {
    match nonempty(url) {
        Some(u) => vec![NormalizedAttachment {
            rel_path: None,
            file_name: None,
            mime_type: None,
            byte_len: None,
            source_url: Some(u.to_string()),
            ref_id: None,
        }],
        None => Vec::new(),
    }
}

fn thread_key(link: &str, fallback: &str) -> String {
    post_urn(link)
        .or_else(|| nonempty(link).map(str::to_string))
        .unwrap_or_else(|| fallback.to_string())
}

/// Canonical post identity (`urn:li:<type>:<id>`) parsed from a LinkedIn
/// post URL. Handles both percent-encoded
/// (`…/urn%3Ali%3Ashare%3A123`) and already-decoded (`urn:li:share:123`)
/// forms; group posts keep their `<group>-<id>` numeric tail.
fn post_urn(link: &str) -> Option<String> {
    let decoded = link.replace("%3A", ":").replace("%3a", ":");
    let start = decoded.find("urn:li:")?;
    let tail: String = decoded[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-'))
        .collect();
    let parts: Vec<&str> = tail.splitn(4, ':').collect();
    match parts.as_slice() {
        ["urn", "li", ty, id] if !ty.is_empty() && !id.is_empty() => Some(tail.clone()),
        _ => None,
    }
}

fn thread_title(share: Option<&Value>, comments: &[&Value]) -> String {
    let snippet = share
        .and_then(|s| nonempty(field(s, "ShareCommentary")))
        .or_else(|| comments.first().and_then(|c| nonempty(field(c, "Message"))));
    match snippet {
        Some(text) => {
            let line = text.lines().next().unwrap_or(text).trim();
            let prefix = if share.is_some() { "Post" } else { "Comment" };
            format!("{prefix}: {}", truncate(line, 80))
        }
        None if share.is_some() => "LinkedIn post".to_string(),
        None => "LinkedIn comment".to_string(),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn field<'a>(p: &'a Value, key: &str) -> &'a str {
    p.get(key).and_then(Value::as_str).unwrap_or("")
}

fn nonempty(s: &str) -> Option<&str> {
    let t = s.trim();
    (!t.is_empty()).then_some(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn with_ids(rows: &[Value]) -> Vec<(String, Value)> {
        rows.iter()
            .enumerate()
            .map(|(i, v)| (format!("row-{i}"), v.clone()))
            .collect()
    }

    #[test]
    fn parses_post_urns_in_both_encodings() {
        assert_eq!(
            post_urn(
                "https://www.linkedin.com/feed/update/urn%3Ali%3AugcPost%3A7458194261025673216"
            ),
            Some("urn:li:ugcPost:7458194261025673216".to_string())
        );
        assert_eq!(
            post_urn("https://www.linkedin.com/feed/update/urn:li:share:7448081445065035776"),
            Some("urn:li:share:7448081445065035776".to_string())
        );
        // Group posts keep their <group>-<id> numeric tail.
        assert_eq!(
            post_urn("https://www.linkedin.com/feed/update/urn%3Ali%3AgroupPost%3A8702844-6496601618866335744"),
            Some("urn:li:groupPost:8702844-6496601618866335744".to_string())
        );
        // No URN → none (callers fall back to the raw link).
        assert_eq!(post_urn("https://example.com/p/1"), None);
        assert_eq!(post_urn(""), None);
    }

    fn share(link: &str, date: &str, commentary: &str) -> Value {
        json!({
            "Date": date, "ShareLink": link, "ShareCommentary": commentary,
            "SharedUrl": "", "MediaUrl": "", "Visibility": "PUBLIC",
        })
    }
    fn comment(link: &str, date: &str, msg: &str) -> Value {
        json!({ "Date": date, "Link": link, "Message": msg })
    }

    #[test]
    fn groups_share_and_comment_on_same_post() {
        let ugc = "https://www.linkedin.com/feed/update/urn%3Ali%3AugcPost%3A7458194261025673216";
        let shares = vec![share(ugc, "2026-05-07 16:41:18", "My post body")];
        let comments = vec![comment(ugc, "2026-05-08 09:00:00", "Following up")];

        let chats = build_post_chats(&with_ids(&shares), &with_ids(&comments), None, &[]);
        assert_eq!(chats.len(), 1, "share + comment on same URN merge");
        let items = &chats[0].buckets[0].items;
        assert_eq!(items.len(), 2, "post + one comment");
        // Post opens the thread, oldest-first.
        assert!(items[0].text.as_deref().unwrap().contains("My post body"));
        assert!(items[1].text.as_deref().unwrap().contains("Following up"));
        // Every item carries the linkout in source_url.
        for it in items {
            assert_eq!(
                it.attachments[0].source_url.as_deref(),
                Some(ugc),
                "linkout on every item"
            );
            assert!(
                it.text.as_deref().unwrap().contains("View on LinkedIn"),
                "inline linkout in body"
            );
        }
        assert_eq!(chats[0].display, "Post: My post body");
        // Whole-post linkout on the thread header / chat-level row.
        assert_eq!(chats[0].source_url.as_deref(), Some(ugc));
    }

    #[test]
    fn comment_only_thread_notes_missing_original() {
        let act = "https://www.linkedin.com/feed/update/urn%3Ali%3Aactivity%3A7401794121226567681";
        let chats = build_post_chats(
            &[],
            &with_ids(&[comment(act, "2026-04-30 15:32:07", "Great point!")]),
            None,
            &[],
        );
        assert_eq!(chats.len(), 1);
        let items = &chats[0].buckets[0].items;
        assert_eq!(items.len(), 2, "placeholder + one comment");
        assert!(matches!(items[0].kind, ItemKind::System));
        assert!(items[0]
            .system_note
            .as_deref()
            .unwrap()
            .contains("not included in the LinkedIn export"));
        assert_eq!(items[0].attachments[0].source_url.as_deref(), Some(act));
        assert!(items[1].text.as_deref().unwrap().contains("Great point!"));
        assert_eq!(chats[0].display, "Comment: Great point!");
        assert_eq!(chats[0].source_url.as_deref(), Some(act));
    }

    #[test]
    fn distinct_posts_stay_separate() {
        let a = "https://www.linkedin.com/feed/update/urn%3Ali%3Ashare%3A111";
        let b = "https://www.linkedin.com/feed/update/urn%3Ali%3Ashare%3A222";
        let chats = build_post_chats(
            &with_ids(&[
                share(a, "2026-01-01 00:00:00", "A"),
                share(b, "2026-01-02 00:00:00", "B"),
            ]),
            &[],
            None,
            &[],
        );
        assert_eq!(chats.len(), 2, "two distinct posts → two threads");
    }
}
