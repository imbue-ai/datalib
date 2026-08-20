//! The Slack applet: browse a mirrored workspace the way the Slack app
//! is laid out.
//!
//! Three levels, because that is the shape both Slack and the rendered
//! data have:
//!
//! ```text
//! channels                    every channel, with counts
//!   └ one channel             each thread, showing its opening message
//!       └ one thread          the whole conversation
//! ```
//!
//! The middle level is the one worth getting right. Slack's channel
//! view shows a *thread's opening message*, not a thread's title, with
//! replies collapsed behind a "N replies" affordance. The data is
//! shaped for that already: the renderer emits one document per thread,
//! and every message in it carries that thread's `markdown_uuid` plus a
//! `message_index`, so the opening message is simply index 0.
//!
//! The third level is the rendered document itself, opened as a card
//! through `documentView`. Re-rendering messages here would mean
//! reimplementing markdown, media and edge handling that the document
//! view already does properly.
//!
//! ## Why it reads sidecars rather than Slack
//!
//! It consumes `<tree>/**/*.grid_rows.json` — the cross-provider
//! contract every render step already emits (see
//! `datalib/backend/etl/src/grid_index.rs`) — as untyped JSON. That
//! keeps it independent of the Slack provider crates and, incidentally,
//! means the same code would work over any source's rendered tree.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// The component module, baked in at compile time so the binary is the
/// only artifact that has to ship.
const COMPONENT_JS: &str = include_str!("component.js");

/// Member name inside the applet's namespace — `comp.<ns>.channels`.
/// Unique only within this applet's own namespace, which is the point:
/// no other applet can collide with it.
const COMPONENT_NAME: &str = "channels";

// ---------------------------------------------------------------------------
// Write mode
// ---------------------------------------------------------------------------

/// A `<name>.json` in a namespace directory. The same document a person
/// would write by hand into `system/frontend/user/`.
#[derive(Serialize)]
struct ComponentMeta {
    title: String,
    description: String,
    component_hash: String,
    component_args: Vec<String>,
}

/// Write this instance's namespace: the component, and the metadata
/// naming it.
pub fn write_frontend(dir: &Path, params: &serde_json::Value) -> Result<()> {
    // The namespace is the directory's own name.
    let namespace = dir
        .file_name()
        .and_then(|s| s.to_str())
        .context("--write-frontend-dir needs a path whose last segment is the namespace")?
        .to_string();

    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;

    // The component's own bytes name it. Every instance computes the
    // same digest, so two namespaces holding this component resolve to
    // one `/modules/<hash>` URL and the browser evaluates it once.
    let hash = sha256_hex(COMPONENT_JS.as_bytes());
    let js = dir.join(format!("{hash}.js"));
    if !js.exists() {
        // Write-then-rename so a reader never sees a half-written file
        // under a name that promises complete content.
        let tmp = dir.join(format!(".{hash}.tmp"));
        std::fs::write(&tmp, COMPONENT_JS).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &js).with_context(|| format!("rename into {}", js.display()))?;
    }

    let label = str_param(params, "workspace").unwrap_or_else(|| namespace.clone());
    let meta = ComponentMeta {
        title: format!("Slack — {label}"),
        description: format!("Browse the channels mirrored into {label}."),
        component_hash: hash,
        // The gallery builds `comp.<namespace>.channels("<namespace>")`
        // from this. The argument tells the component which backend
        // prefix to call, and it is per-instance — which is why the
        // namespace had to be discoverable at all.
        component_args: vec![namespace],
    };
    let meta_path = dir.join(format!("{COMPONENT_NAME}.json"));
    std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)
        .with_context(|| format!("write {}", meta_path.display()))?;
    Ok(())
}

fn str_param(params: &serde_json::Value, key: &str) -> Option<String> {
    params.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let d = h.finalize();
    let mut s = String::with_capacity(64);
    for b in d.iter() {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ---------------------------------------------------------------------------
// The data
// ---------------------------------------------------------------------------

#[derive(Serialize, Default)]
struct ChannelsResponse {
    workspace: String,
    channels: Vec<Channel>,
    /// Paths that could not be read. Non-empty means the listing is
    /// partial, which the card says out loud rather than presenting a
    /// truncated list as complete.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct Channel {
    name: String,
    /// Rendered documents in this channel. Slack renders one document
    /// per thread, so this is the thread count.
    threads: usize,
    messages: usize,
}

#[derive(Serialize, Default)]
struct ChannelResponse {
    channel: String,
    threads: Vec<Thread>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

/// One thread as the channel view shows it: its opening message, and
/// how many replies are hiding behind it.
#[derive(Serialize)]
struct Thread {
    /// The document holding the whole conversation —
    /// `documentView("<markdown_uuid>")`.
    markdown_uuid: String,
    /// Who wrote the opening message, when, and what it said.
    author: String,
    when_ts: String,
    text: String,
    /// Messages after the opening one. Zero means there is nothing more
    /// to open, which is what the card keys the "N replies" link on.
    replies: usize,
}

#[derive(Default)]
struct ThreadData {
    /// The lowest `message_index` seen, and that message's fields. The
    /// opening message is index 0, but taking the minimum means a
    /// partially-rendered thread still shows something sensible.
    opening_index: Option<i64>,
    author: String,
    text: String,
    when_raw: String,
    when: Option<chrono::DateTime<chrono::Utc>>,
    messages: usize,
}

/// `when_ts` as a comparable instant.
///
/// The field is offset-bearing RFC 3339, so string comparison is wrong:
/// `…T10:00:00+05:00` sorts after `…T08:00:00+00:00` while actually
/// being two hours earlier. Unparseable stamps sort before everything,
/// so a malformed row never displaces a good one.
fn when_key(when_ts: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(when_ts)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

type Channels = BTreeMap<String, BTreeMap<String, ThreadData>>;

/// Walk the rendered tree, grouping documents by channel and then by
/// thread.
///
/// Two levels because that is how the data is shaped: Slack renders one
/// document per thread, and every message in a thread carries that
/// thread's `markdown_uuid`. Collapsing straight to "a document per
/// channel" — which an earlier version did — picks one arbitrary thread
/// and makes a busy channel look like it holds a single message.
///
/// Returns the channels it could read plus the paths it could not, so
/// the caller can say the listing is partial. A silently truncated list
/// reads as authoritative, which is the worse failure.
fn scan(tree: &Path) -> (Channels, Vec<String>) {
    let mut by_channel: Channels = BTreeMap::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut stack = vec![tree.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && dir == tree => {
                // A tree no step has written yet is an empty listing,
                // not a failure — the card renders a first-run message.
                continue;
            }
            Err(e) => {
                // Anything else (EACCES, EIO, a vanished subdirectory
                // mid-walk) means the listing below is incomplete.
                warnings.push(format!("{}: {e}", dir.display()));
                continue;
            }
        };
        for ent in rd.flatten() {
            let path = ent.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if !path.to_string_lossy().ends_with(".grid_rows.json") {
                continue;
            }
            let text = match std::fs::read_to_string(&path) {
                Ok(t) => t,
                Err(e) => {
                    warnings.push(format!("{}: {e}", path.display()));
                    continue;
                }
            };
            let v: serde_json::Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                // A sidecar half-written by a running sync is expected
                // and transient, so it is a warning rather than a
                // failure — but the user still learns the list is
                // incomplete.
                Err(e) => {
                    warnings.push(format!("{}: {e}", path.display()));
                    continue;
                }
            };
            let Some(rows) = v.get("rows").and_then(|r| r.as_array()) else {
                continue;
            };
            for row in rows {
                let Some(channel) = row.get("channel").and_then(|c| c.as_str()) else {
                    continue;
                };
                let Some(md) = row.get("markdown_uuid").and_then(|m| m.as_str()) else {
                    continue;
                };
                let thread = by_channel
                    .entry(channel.to_string())
                    .or_default()
                    .entry(md.to_string())
                    .or_default();

                // `message_index` is absent on the row that *is* the
                // document and present on each message inside it, so it
                // is the discriminator — sturdier than matching the
                // `kind` display string.
                let index = row.get("message_index").and_then(|i| i.as_i64());
                let when_raw = row.get("when_ts").and_then(|w| w.as_str()).unwrap_or("");
                if let Some(index) = index {
                    thread.messages += 1;
                    // The opening message is the lowest index present.
                    if thread.opening_index.is_none_or(|prev| index < prev) {
                        thread.opening_index = Some(index);
                        thread.author = row
                            .get("author")
                            .and_then(|a| a.as_str())
                            .unwrap_or("")
                            .to_string();
                        thread.text = row
                            .get("text")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string();
                        thread.when_raw = when_raw.to_string();
                        thread.when = when_key(when_raw);
                    }
                } else if thread.when.is_none() {
                    // The document row, when no message has landed yet.
                    thread.when_raw = when_raw.to_string();
                    thread.when = when_key(when_raw);
                }
            }
        }
    }
    (by_channel, warnings)
}

/// One line for a row label, trimmed so a long paste does not fill the
/// card.
fn preview(text: &str) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut out: String = line.chars().take(200).collect();
    if line.chars().count() > 200 {
        out.push('…');
    }
    out
}

fn channels_response(tree: &Path, workspace: &str) -> ChannelsResponse {
    let (by_channel, warnings) = scan(tree);
    let channels = by_channel
        .into_iter()
        .map(|(name, threads)| Channel {
            name,
            threads: threads.len(),
            messages: threads.values().map(|t| t.messages).sum(),
        })
        .collect();
    ChannelsResponse {
        workspace: workspace.to_string(),
        channels,
        warnings,
    }
}

fn channel_response(tree: &Path, channel: &str) -> ChannelResponse {
    let (by_channel, warnings) = scan(tree);
    let mut threads: Vec<Thread> = by_channel
        .get(channel)
        .map(|m| {
            m.iter()
                .map(|(md, t)| Thread {
                    markdown_uuid: md.clone(),
                    author: t.author.clone(),
                    when_ts: t.when_raw.clone(),
                    text: preview(&t.text),
                    // Everything after the opening message.
                    replies: t.messages.saturating_sub(1),
                })
                .collect()
        })
        .unwrap_or_default();
    // Oldest first, the way a channel reads. Ties break on the uuid so
    // the order is identical on every request over unchanged data (the
    // directory walk itself is unordered).
    threads.sort_by(|a, b| {
        when_key(&a.when_ts)
            .cmp(&when_key(&b.when_ts))
            .then_with(|| a.markdown_uuid.cmp(&b.markdown_uuid))
    });
    ChannelResponse {
        channel: channel.to_string(),
        threads,
        warnings,
    }
}

// ---------------------------------------------------------------------------
// Serve mode
// ---------------------------------------------------------------------------

pub fn serve(port: u16, params: &serde_json::Value) -> Result<()> {
    let tree = str_param(params, "tree")
        .context("params.tree is required: which rendered_md tree this instance reads")?;
    let workspace = str_param(params, "workspace")
        // `DATALIB_APPLET_ID` is what the gateway calls this instance,
        // so it is the right fallback label.
        .or_else(|| std::env::var("DATALIB_APPLET_ID").ok())
        .unwrap_or_else(|| "slack".to_string());
    let tree_path = std::path::PathBuf::from(&tree);

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .with_context(|| format!("bind 127.0.0.1:{port}"))?;
    eprintln!("datalib-applet slack: listening on 127.0.0.1:{port}, tree {tree}");

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if let Err(e) = handle(stream, &tree_path, &workspace) {
            eprintln!("datalib-applet slack: request failed: {e:#}");
        }
    }
    Ok(())
}

fn handle(mut stream: TcpStream, tree: &Path, workspace: &str) -> Result<()> {
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf)?;
    let head = String::from_utf8_lossy(&buf[..n]);
    let target = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/");
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };

    let (status, body) = match path {
        // Level 1.
        "/channels" => {
            let resp = channels_response(tree, workspace);
            warn(&resp.warnings);
            (200, serde_json::to_string(&resp)?)
        }
        // Level 2: one channel's threads, each with its opening
        // message. Level 3 is the rendered document itself, which the
        // card opens through `documentView` — no endpoint needed.
        "/channel" => match query_param(query, "name") {
            Some(channel) => {
                let resp = channel_response(tree, &channel);
                warn(&resp.warnings);
                (200, serde_json::to_string(&resp)?)
            }
            None => (
                400,
                serde_json::json!({ "error": "/channel needs ?name=<channel>" }).to_string(),
            ),
        },
        // A readiness probe the gateway may use once it wants something
        // stronger than "the port accepts".
        "/health" => (200, r#"{"ok":true}"#.to_string()),
        _ => (
            404,
            serde_json::json!({ "error": format!("no route {path}") }).to_string(),
        ),
    };

    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        _ => "Not Found",
    };
    let resp = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(resp.as_bytes())?;
    stream.flush()?;
    Ok(())
}

fn warn(warnings: &[String]) {
    for w in warnings {
        eprintln!("datalib-applet slack: unreadable: {w}");
    }
}

/// Percent-decode a query value. Channel names begin with `#`, which a
/// URL would otherwise read as a fragment delimiter, so the caller
/// encodes and this undoes it.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(v) => {
                    out.push(v);
                    i += 3;
                }
                Err(_) => {
                    out.push(b[i]);
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One sidecar per thread, with the thread row and its messages all
    /// carrying the same markdown_uuid — the real Slack shape.
    fn write_thread(dir: &Path, md: &str, channel: &str, when: &str, msgs: &[(&str, &str)]) {
        std::fs::create_dir_all(dir).unwrap();
        let mut rows = vec![serde_json::json!({
            "uuid": md, "kind": "Slack Thread", "channel": channel,
            "when_ts": when, "markdown_uuid": md, "message_index": null,
            "text": msgs.first().map(|m| m.1).unwrap_or(""),
        })];
        for (i, (author, text)) in msgs.iter().enumerate() {
            rows.push(serde_json::json!({
                "uuid": format!("{md}-m{i}"), "kind": "Slack Message", "channel": channel,
                "when_ts": when, "markdown_uuid": md, "message_index": i,
                "author": author, "text": text,
            }));
        }
        std::fs::write(
            dir.join(format!("{md}.grid_rows.json")),
            serde_json::json!({ "header": { "markdown_uuid": md }, "rows": rows }).to_string(),
        )
        .unwrap();
    }

    /// The namespace is read off the directory, and it lands in
    /// `component_args` — which is how the gallery's constructed call
    /// tells the component which backend prefix to use.
    #[test]
    fn the_written_metadata_names_its_own_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("slack_work");
        write_frontend(&dir, &serde_json::Value::Null).unwrap();

        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("channels.json")).unwrap())
                .unwrap();
        assert_eq!(meta["component_args"], serde_json::json!(["slack_work"]));
        let hash = meta["component_hash"].as_str().unwrap();
        let body = std::fs::read(dir.join(format!("{hash}.js"))).unwrap();
        assert_eq!(sha256_hex(&body), hash);
    }

    /// Two instances write byte-identical component code — that is what
    /// makes the browser evaluate it once — and differ only in the
    /// argument baked into their metadata.
    #[test]
    fn two_namespaces_share_the_component_and_differ_in_args() {
        let tmp = tempfile::tempdir().unwrap();
        let mut hashes = std::collections::HashSet::new();
        for ns in ["slack_work", "slack_personal"] {
            let dir = tmp.path().join(ns);
            write_frontend(&dir, &serde_json::Value::Null).unwrap();
            let meta: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(dir.join("channels.json")).unwrap())
                    .unwrap();
            assert_eq!(meta["component_args"], serde_json::json!([ns]));
            hashes.insert(meta["component_hash"].as_str().unwrap().to_string());
        }
        assert_eq!(hashes.len(), 1, "component code must be identical");
    }

    /// Re-running over an existing directory must be a no-op, since a
    /// refresh does exactly that.
    #[test]
    fn writing_twice_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("ns");
        write_frontend(&dir, &serde_json::Value::Null).unwrap();
        let before = std::fs::read_dir(&dir).unwrap().flatten().count();
        write_frontend(&dir, &serde_json::Value::Null).unwrap();
        let after = std::fs::read_dir(&dir).unwrap().flatten().count();
        assert_eq!(before, 2);
        assert_eq!(before, after);
    }

    #[test]
    fn scanning_a_missing_tree_is_empty_and_silent() {
        let (channels, warnings) = scan(Path::new("/definitely/not/here"));
        assert!(channels.is_empty());
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// Level 1 counts threads and messages; the regression guard is
    /// that a channel with several threads reports all of them rather
    /// than collapsing to one document.
    #[test]
    fn channels_count_every_thread_and_message() {
        let tmp = tempfile::tempdir().unwrap();
        write_thread(
            tmp.path(),
            "t1",
            "#cat-qi",
            "2026-07-01T00:00:00Z",
            &[("ann", "hello"), ("bob", "hi back")],
        );
        write_thread(
            tmp.path(),
            "t2",
            "#cat-qi",
            "2026-07-02T00:00:00Z",
            &[("cid", "joined")],
        );
        write_thread(
            tmp.path(),
            "t3",
            "#chat-qi",
            "2026-07-03T00:00:00Z",
            &[("dee", "elsewhere")],
        );

        let resp = channels_response(tmp.path(), "ws");
        assert_eq!(resp.channels.len(), 2);
        let cat = resp.channels.iter().find(|c| c.name == "#cat-qi").unwrap();
        assert_eq!(cat.threads, 2);
        assert_eq!(cat.messages, 3);
    }

    /// Level 2 is the one that mimics Slack: each thread shows its
    /// *opening message*, with the rest counted as replies.
    #[test]
    fn a_channel_shows_each_threads_opening_message() {
        let tmp = tempfile::tempdir().unwrap();
        write_thread(
            tmp.path(),
            "t1",
            "#c",
            "2026-07-01T00:00:00Z",
            &[
                ("ann", "the opening line"),
                ("bob", "a reply"),
                ("cid", "another"),
            ],
        );
        write_thread(
            tmp.path(),
            "t2",
            "#c",
            "2026-07-02T00:00:00Z",
            &[("dee", "solo")],
        );

        let resp = channel_response(tmp.path(), "#c");
        assert_eq!(resp.threads.len(), 2);
        // Oldest first, the way a channel reads.
        let first = &resp.threads[0];
        assert_eq!(first.markdown_uuid, "t1");
        assert_eq!(first.author, "ann");
        assert_eq!(first.text, "the opening line");
        assert_eq!(first.replies, 2, "everything after the opening message");
        // A one-message thread has nothing to open.
        assert_eq!(resp.threads[1].replies, 0);
    }

    /// Row order inside a sidecar is not guaranteed, so "opening
    /// message" has to mean lowest index rather than first seen.
    #[test]
    fn the_opening_message_is_the_lowest_index_not_the_first_row() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("t.grid_rows.json"),
            serde_json::json!({ "rows": [
                {"channel":"#c","markdown_uuid":"t","message_index":2,"author":"c","text":"third","when_ts":"2026-07-01T00:00:02Z"},
                {"channel":"#c","markdown_uuid":"t","message_index":0,"author":"a","text":"first","when_ts":"2026-07-01T00:00:00Z"},
                {"channel":"#c","markdown_uuid":"t","message_index":1,"author":"b","text":"second","when_ts":"2026-07-01T00:00:01Z"},
            ]})
            .to_string(),
        )
        .unwrap();
        let resp = channel_response(tmp.path(), "#c");
        assert_eq!(resp.threads[0].author, "a");
        assert_eq!(resp.threads[0].text, "first");
        assert_eq!(resp.threads[0].replies, 2);
    }

    #[test]
    fn an_unknown_channel_is_empty_not_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        write_thread(tmp.path(), "t", "#a", "2026-07-01T00:00:00Z", &[("x", "y")]);
        assert!(channel_response(tmp.path(), "#nope").threads.is_empty());
    }

    /// Thread order must not depend on the directory walk, which is a
    /// LIFO stack over unordered `read_dir`.
    #[test]
    fn thread_order_is_stable_across_runs() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..5 {
            let tmp = tempfile::tempdir().unwrap();
            for md in ["a", "b", "c"] {
                write_thread(tmp.path(), md, "#c", "2026-07-01T00:00:00Z", &[("x", "y")]);
            }
            let order: Vec<String> = channel_response(tmp.path(), "#c")
                .threads
                .into_iter()
                .map(|t| t.markdown_uuid)
                .collect();
            seen.insert(order);
        }
        assert_eq!(seen.len(), 1, "order varied across runs: {seen:?}");
    }

    /// A channel name starts with `#`, which a URL reads as a fragment
    /// delimiter, so the round trip has to survive encoding.
    #[test]
    fn channel_names_survive_the_query_string() {
        assert_eq!(query_param("name=%23cat-qi", "name").unwrap(), "#cat-qi");
        assert_eq!(query_param("x=1&name=%23a%20b", "name").unwrap(), "#a b");
        assert!(query_param("nope=1", "name").is_none());
    }

    /// An unreadable file makes the listing partial, and the caller is
    /// told so rather than handed a short list that looks whole.
    #[test]
    fn unreadable_input_is_reported_not_swallowed() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("bad.grid_rows.json"), "{not json").unwrap();
        write_thread(
            tmp.path(),
            "ok",
            "#c",
            "2026-07-01T00:00:00Z",
            &[("x", "y")],
        );
        let resp = channels_response(tmp.path(), "ws");
        assert_eq!(resp.channels.len(), 1);
        assert_eq!(resp.warnings.len(), 1, "{:?}", resp.warnings);
    }

    #[test]
    fn long_previews_are_trimmed_to_one_line() {
        assert_eq!(preview("first\nsecond"), "first");
        assert_eq!(preview("   \n  real  \n"), "real");
        let long = "x".repeat(400);
        let out = preview(&long);
        assert!(out.chars().count() <= 201 && out.ends_with('…'), "{out}");
    }
}
