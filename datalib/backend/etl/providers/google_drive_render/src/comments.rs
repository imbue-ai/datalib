//! A file's comment threads: each comment and reply as a message under
//! the header every chat uses, and the place in the document each thread
//! belongs. Drive does not say where a Docs comment is anchored (its
//! `anchor` is an id only the editor can resolve), but it does say what
//! text was selected, so a thread follows the first block quoting it.

use datalib_etl_render::html::{escape_md_block, escape_text};
use datalib_etl_render::message::MessageHeader;
use datalib_etl_render::section::{msg_class, Section};
use std::collections::BTreeMap;

use datalib_contact_schema::{ContactHandle, ContactKind, NormalizedContact};
use datalib_handle::Handle;
use datalib_id::Identity;
use datalib_schema::providers::Provider;
use serde_json::Value;

use crate::doc::Block;
use crate::ids;

#[derive(Debug, Clone)]
pub struct Thread {
    /// The raw store's key, `{file}#{comment}`.
    pub key: String,
    pub comment_id: String,
    /// The text the comment was made on, as a reader would see it.
    pub quote: Option<String>,
    pub resolved: bool,
    /// The comment first, then its replies.
    pub messages: Vec<Message>,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub id: Identity,
    pub author: String,
    pub handle: Option<Handle>,
    pub created: Option<String>,
    pub modified: Option<String>,
    /// What it says; empty for a deleted one and for a bare resolve.
    pub text: String,
    /// What it did besides speak: resolved or reopened the thread, or
    /// that it was deleted.
    pub note: Option<&'static str>,
}

/// Every thread on a file, oldest first. `me` is the account's own
/// handle where this file names it; a comment the account wrote is
/// otherwise known, like anyone's, by the name it shows.
pub fn threads(source_id: &str, rows: &[(String, Value)], me: Option<&Handle>) -> Vec<Thread> {
    let mut out: Vec<Thread> = rows
        .iter()
        .map(|(key, c)| {
            let comment_id = key.split_once('#').map_or(key.as_str(), |(_, c)| c);
            let mut messages = vec![message(
                c,
                me,
                ids::comment(source_id, key, str_at(c, "createdTime")),
            )];
            for r in c
                .get("replies")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let reply_key = format!("{key}#{}", str_at(r, "id").unwrap_or(""));
                messages.push(message(
                    r,
                    me,
                    ids::reply(source_id, &reply_key, str_at(r, "createdTime")),
                ));
            }
            Thread {
                key: key.clone(),
                comment_id: comment_id.to_string(),
                quote: c
                    .pointer("/quotedFileContent/value")
                    .and_then(Value::as_str)
                    .map(|q| datalib_html_entities::decode(q).into_owned())
                    .filter(|q| !q.trim().is_empty()),
                resolved: c.get("resolved").and_then(Value::as_bool) == Some(true),
                messages,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        let at = |t: &Thread| t.messages[0].created.clone();
        at(a).cmp(&at(b)).then_with(|| a.key.cmp(&b.key))
    });
    out
}

fn message(v: &Value, me: Option<&Handle>, id: Identity) -> Message {
    let name = v
        .pointer("/author/displayName")
        .and_then(Value::as_str)
        .unwrap_or("");
    let is_me = v.pointer("/author/me").and_then(Value::as_bool) == Some(true);
    let handle = match (is_me, me) {
        (true, Some(me)) => Some(me.clone()),
        _ => Handle::google_name(name),
    };
    let deleted = v.get("deleted").and_then(Value::as_bool) == Some(true);
    let note = if deleted {
        Some("deleted")
    } else {
        match str_at(v, "action") {
            Some("resolve") => Some("resolved the thread"),
            Some("reopen") => Some("reopened the thread"),
            _ => None,
        }
    };
    let created = str_at(v, "createdTime").map(str::to_string);
    let modified = str_at(v, "modifiedTime")
        .filter(|m| Some(*m) != created.as_deref())
        .map(str::to_string);
    Message {
        id,
        author: if name.is_empty() { "Unknown" } else { name }.to_string(),
        handle,
        created,
        modified,
        text: str_at(v, "content").unwrap_or("").to_string(),
        note,
    }
}

fn str_at<'v>(v: &'v Value, key: &str) -> Option<&'v str> {
    v.get(key).and_then(Value::as_str)
}

/// The fewest characters a quote's opening must share with a block for
/// a thread whose text was edited since to follow that block.
const EDITED_QUOTE_MIN_CHARS: usize = 24;

/// Which block each thread follows: the first whose text holds the
/// first line of the thread's quote, else, for text edited since the
/// comment, the one holding the longest opening of it. `None` for a
/// thread with no quote, or one whose text is no longer there.
pub fn place(threads: &[Thread], blocks: &[Block]) -> Vec<Option<usize>> {
    let texts: Vec<String> = blocks.iter().map(|b| squash(&b.text)).collect();
    threads
        .iter()
        .map(|t| {
            let quote = t.quote.as_deref()?;
            let first = quote.lines().map(squash).find(|l| !l.is_empty())?;
            if let Some(i) = texts.iter().position(|text| text.contains(&first)) {
                return Some(i);
            }
            let (i, shared) = texts
                .iter()
                .map(|text| shared_opening(&first, text))
                .enumerate()
                .max_by_key(|&(i, n)| (n, std::cmp::Reverse(i)))?;
            (shared >= EDITED_QUOTE_MIN_CHARS).then_some(i)
        })
        .collect()
}

/// How many characters of `quote`'s opening `text` holds somewhere.
fn shared_opening(quote: &str, text: &str) -> usize {
    let ends: Vec<usize> = quote
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .collect();
    let (mut lo, mut hi) = (0, ends.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if text.contains(&quote[..ends[mid - 1]]) {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    lo
}

fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// One thread, collapsed, its quote first: an opener and a closer around
/// one keyed section per message, so each message is its own anchor and
/// grid row.
pub fn sections(t: &Thread) -> Vec<Section> {
    let n = t.messages.len();
    let mut summary = format!("💬 {n} comment{}", if n == 1 { "" } else { "s" });
    if t.resolved {
        summary.push_str(" · resolved");
    }
    if let Some(q) = &t.quote {
        let q = squash(q);
        let cut: String = q.chars().take(60).collect();
        let ellipsis = if cut.len() < q.len() { "…" } else { "" };
        summary.push_str(&format!(" on “{cut}{ellipsis}”"));
    }
    // Kept as text search reads, as the thread's summary and quote are.
    let mut open = Section::fold_open("comment-thread", &escape_text(&summary)).kept_whole();
    if let Some(q) = &t.quote {
        open = open.then(&format!(
            "> {}\n\n",
            escape_md_block(q.trim()).replace('\n', "\n> ")
        ));
    }
    let mut out = vec![open];
    for m in &t.messages {
        let mut s = String::new();
        s.push_str(
            &MessageHeader {
                author: &m.author,
                handle: m.handle.as_ref(),
                date_ms: m.created.as_deref().and_then(datalib_time::record_stamp_ms),
                offset_secs: m
                    .created
                    .as_deref()
                    .and_then(datalib_time::written_offset_secs),
                source_url: None,
                ..Default::default()
            }
            .render(),
        );
        s.push_str("\n\n");
        if !m.text.is_empty() {
            s.push_str(&escape_md_block(&m.text));
            s.push_str("\n\n");
        }
        if let Some(note) = m.note {
            s.push_str(&format!("*{note}*\n\n"));
        }
        out.push(Section::boxed(
            &m.id.uuid,
            msg_class(Provider::GoogleDrive, &[]),
            &s,
        ));
    }
    out.push(Section::fold_close().kept_whole());
    out
}

/// Each comment author with a handle, as this file's comments describe
/// them: the names they showed, as chat-common's `baseline_contacts`
/// describes a chat's authors. The grid counts how much each wrote.
pub fn authors(source_id: &str, threads: &[Thread], me: Option<&Handle>) -> Vec<NormalizedContact> {
    let mut by_handle: BTreeMap<&str, NormalizedContact> = BTreeMap::new();
    for m in threads.iter().flat_map(|t| &t.messages) {
        let Some(handle) = &m.handle else { continue };
        let c = by_handle.entry(handle.as_str()).or_insert_with(|| {
            let mut c = NormalizedContact::new(source_id, handle.as_str(), ContactKind::Person);
            c.handles = vec![ContactHandle::of(handle.clone())];
            c.account = Some(handle) == me;
            c
        });
        if !c.names.contains(&m.author) {
            c.names.push(m.author.clone());
        }
    }
    by_handle.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::BlockKind;
    use datalib_etl_render::section::join;
    use serde_json::json;

    fn block(text: &str) -> Block {
        Block {
            md: text.to_string(),
            text: text.to_string(),
            kind: BlockKind::Plain,
        }
    }

    fn thread_rows() -> Vec<(String, Value)> {
        vec![(
            "1Log#AAA".to_string(),
            json!({"id": "AAA", "createdTime": "2364-03-02T09:00:00.000Z",
            "modifiedTime": "2364-03-02T09:00:00.000Z",
            "author": {"displayName": "Geordi La Forge", "me": false},
            "content": "Recalibrate first.", "resolved": true,
            "quotedFileContent": {"mimeType": "text/html", "value": "warp &amp; impulse"},
            "replies": [
                {"id": "R1", "createdTime": "2364-03-02T10:00:00.000Z",
                 "author": {"displayName": "Jean-Luc Picard", "me": true},
                 "content": "Make it so."},
                {"id": "R2", "createdTime": "2364-03-02T11:00:00.000Z",
                 "author": {"displayName": "Jean-Luc Picard", "me": true},
                 "content": "", "action": "resolve"},
                {"id": "R3", "createdTime": "2364-03-02T12:00:00.000Z",
                 "author": {"displayName": "Worf", "me": false}, "deleted": true},
            ]}),
        )]
    }

    #[test]
    fn a_thread_is_its_comment_then_its_replies_each_attributed() {
        let me = Handle::email("picard@enterprise.test").unwrap();
        let t = &threads("src", &thread_rows(), Some(&me))[0];
        assert_eq!(t.quote.as_deref(), Some("warp & impulse"));
        let who: Vec<(&str, Option<&str>, Option<&str>)> = t
            .messages
            .iter()
            .map(|m| {
                (
                    m.author.as_str(),
                    m.handle.as_ref().map(Handle::as_str),
                    m.note,
                )
            })
            .collect();
        assert_eq!(
            who,
            vec![
                ("Geordi La Forge", Some("google:name/Geordi La Forge"), None),
                (
                    "Jean-Luc Picard",
                    Some("email:picard@enterprise.test"),
                    None
                ),
                (
                    "Jean-Luc Picard",
                    Some("email:picard@enterprise.test"),
                    Some("resolved the thread")
                ),
                ("Worf", Some("google:name/Worf"), Some("deleted")),
            ]
        );
        let md = join(&sections(t));
        assert!(
            md.starts_with(
                "<details class=\"comment-thread\">\n<summary>💬 4 comments · resolved on \
                 “warp &amp; impulse”</summary>\n\n> warp &amp; impulse\n\n<div id=\"m-"
            ),
            "{md}"
        );
        assert!(
            md.contains(
                "## [Geordi La Forge](datalib:handle/google/name/Geordi%20La%20Forge \
                 \"Geordi La Forge (google:name/Geordi La Forge)\") <time"
            ),
            "{md}"
        );
        assert!(md.contains("\n\nRecalibrate first.\n\n</div>"), "{md}");
        assert!(md.ends_with("</div>\n\n</details>\n\n"), "{md}");
    }

    #[test]
    fn a_thread_follows_the_block_holding_its_quote() {
        let blocks = vec![
            block("Captain's log."),
            block("Set warp  & impulse to standby.\nThen engage."),
        ];
        let mut rows = thread_rows();
        rows.push((
            "1Log#BBB".to_string(),
            json!({"id": "BBB", "createdTime": "2364-03-03T09:00:00.000Z",
                   "author": {"displayName": "Data"}, "content": "On the whole page."}),
        ));
        rows.push((
            "1Log#CCC".to_string(),
            json!({"id": "CCC", "createdTime": "2364-03-04T09:00:00.000Z",
                   "author": {"displayName": "Data"}, "content": "Gone.",
                   "quotedFileContent": {"value": "text since deleted"}}),
        ));
        let t = threads("src", &rows, None);
        assert_eq!(place(&t, &blocks), vec![Some(1), None, None]);
    }

    /// Text edited after the comment still finds its paragraph when the
    /// quote's opening is there; a short shared opening is not enough.
    #[test]
    fn a_thread_on_edited_text_follows_its_longest_shared_opening() {
        let blocks = vec![
            block("Set a course for the Neutral Zone."),
            block("Set a course for Starbase 74, warp factor six, and hold."),
        ];
        let rows = |quote: &str| {
            vec![(
                "1Log#AAA".to_string(),
                json!({"author": {"displayName": "Data"}, "content": "Noted.",
                       "quotedFileContent": {"value": quote}}),
            )]
        };
        let edited = threads(
            "src",
            &rows("Set a course for Starbase 74, warp factor five"),
            None,
        );
        assert_eq!(place(&edited, &blocks), vec![Some(1)]);
        let unlike = threads("src", &rows("Set a course for Risa"), None);
        assert_eq!(place(&unlike, &blocks), vec![None]);
    }

    #[test]
    fn a_comment_in_markup_reads_as_typed() {
        let rows = vec![(
            "1Log#AAA".to_string(),
            json!({"id": "AAA", "author": {"displayName": "<b>Q</b>"},
                   "content": "<script>x</script> & co\n# not a heading",
                   "quotedFileContent": {"value": "&lt;i&gt;omnipotent&lt;/i&gt;"}}),
        )];
        let md = join(&sections(&threads("src", &rows, None)[0]));
        assert!(!md.contains("<script>") && !md.contains("<i>"), "{md}");
        // The link's title is an attribute, which markdown-it escapes.
        assert!(
            md.contains("## [&lt;b&gt;Q&lt;/b&gt;](datalib:handle/google/"),
            "{md}"
        );
        assert!(
            md.contains("&lt;script&gt;x&lt;/script&gt; &amp; co\n\\# not a heading"),
            "{md}"
        );
    }

    #[test]
    fn each_author_with_a_handle_is_a_source_contact() {
        let me = Handle::email("picard@enterprise.test").unwrap();
        let contacts = authors("src", &threads("src", &thread_rows(), Some(&me)), Some(&me));
        let mine: Vec<&str> = contacts
            .iter()
            .filter(|c| c.account)
            .map(|c| c.key.as_str())
            .collect();
        assert_eq!(mine, ["email:picard@enterprise.test"], "the account's own");
        let got: Vec<(String, Vec<String>)> = contacts
            .iter()
            .map(|c| (c.key.clone(), c.names.clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                (
                    "email:picard@enterprise.test".to_string(),
                    vec!["Jean-Luc Picard".to_string()]
                ),
                (
                    "google:name/Geordi La Forge".to_string(),
                    vec!["Geordi La Forge".to_string()]
                ),
                ("google:name/Worf".to_string(), vec!["Worf".to_string()]),
            ]
        );
    }
}
