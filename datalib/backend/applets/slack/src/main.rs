//! `datalib-view-slack` — an applet: one binary that both describes
//! the frontend it contributes and serves the data behind it.
//!
//! Two modes, selected by flag:
//!
//! ```text
//! datalib-view-slack --write-frontend-dir <root>/system/frontend/slack_work \
//!                    --params '{"tree":"slack_work/rendered_md"}'
//! datalib-view-slack -p 41xxx --params '{"tree":"slack_work/rendered_md"}'
//! ```
//!
//! The first writes two files into the directory it is handed and
//! exits; the second serves `/channels` over the rendered tree named in
//! `params`.
//!
//! ## What the write mode produces
//!
//! ```text
//! <dir>/<sha256 of the component>.js
//! <dir>/channels.json    { title, description, component_hash, component_args }
//! ```
//!
//! Nothing about those files is applet-specific — the same two files
//! written by hand into `system/frontend/user/` would define a
//! component the same way. The directory *is* the interface.
//!
//! ## Why the id comes from the directory
//!
//! The gallery entry has to call `comp.<namespace>.channels` with this
//! instance's own id as its argument, so the component knows which
//! backend to talk to. The binary cannot know that id — two instances
//! differ only in configuration — so it reads it off the last segment
//! of the directory it was told to write. Both instances still emit
//! byte-identical component code, which is what lets the browser
//! evaluate it once and bind it twice.
//!
//! ## Printing
//!
//! The workspace bans `println!`/`eprintln!` because they bypass
//! indicatif's bar suspension and corrupt a pipeline's progress
//! display. An applet runs no bars: it is a standalone server, and in
//! manifest mode its **stdout is the protocol** — the gateway parses
//! exactly what this prints. stderr is its log, captured by the gateway
//! and surfaced in a `502` when it fails to start. So the ban is lifted
//! here for the same reason it is lifted in `datalib-http`.
//!
//! ## Why it reads sidecars rather than Slack
//!
//! The applet consumes `<tree>/**/*.grid_rows.json` — the
//! cross-provider contract every render step already emits (see
//! `datalib/backend/etl/src/grid_index.rs`). That keeps it independent
//! of the Slack provider crates and, incidentally, means the same code
//! would work over any source's rendered tree.
#![allow(clippy::disallowed_macros)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// The component module, baked in at compile time so the binary is the
/// only artifact that has to ship.
const COMPONENT_JS: &str = include_str!("component.js");

/// Member name inside the applet's namespace — `slack_work.channels`.
/// Unique only within this manifest, which is the whole point: no
/// other applet can collide with it.
const COMPONENT_NAME: &str = "channels";

/// Set by the gateway on every applet child. Kept as a named constant
/// because the manifest mode receives the same value as `--applet-id`,
/// and the two must agree.
const ENV_APPLET_ID: &str = "DATALIB_APPLET_ID";

fn main() {
    if let Err(e) = run() {
        eprintln!("datalib-view-slack: {e:#}");
        std::process::exit(1);
    }
}

struct Args {
    /// Where to write this instance's namespace. Its last segment is
    /// the namespace name, which the gallery entry has to embed.
    write_frontend_dir: Option<PathBuf>,
    port: Option<u16>,
    params: Params,
}

#[derive(Default)]
struct Params {
    /// Rendered-markdown tree this instance reads, relative to the
    /// data root (the process's working directory, set by the
    /// gateway).
    tree: Option<String>,
    /// Optional display name for the workspace.
    workspace: Option<String>,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        write_frontend_dir: None,
        port: None,
        params: Params::default(),
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        let next = |i: &mut usize| -> Result<String> {
            *i += 1;
            argv.get(*i)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{arg} needs a value"))
        };
        match arg {
            "--write-frontend-dir" => a.write_frontend_dir = Some(PathBuf::from(next(&mut i)?)),
            "-p" | "--port" => a.port = Some(next(&mut i)?.parse().context("port")?),
            "--params" => {
                let json = next(&mut i)?;
                let v: serde_json::Value =
                    serde_json::from_str(&json).context("--params is not valid JSON")?;
                a.params.tree = v.get("tree").and_then(|x| x.as_str()).map(str::to_string);
                a.params.workspace = v
                    .get("workspace")
                    .and_then(|x| x.as_str())
                    .map(str::to_string);
            }
            other => anyhow::bail!("unknown flag {other:?}"),
        }
        i += 1;
    }
    Ok(a)
}

fn run() -> Result<()> {
    let args = parse_args()?;
    if let Some(dir) = &args.write_frontend_dir {
        return write_frontend(dir, &args.params);
    }
    match args.port {
        Some(p) => serve(p, &args.params),
        None => anyhow::bail!("expected --write-frontend-dir <dir> or -p <port>"),
    }
}

// ---------------------------------------------------------------------------
// Mode 1: the frontend manifest
// ---------------------------------------------------------------------------

/// A `<name>.json` in a namespace directory. The same document a
/// person would write by hand into `system/frontend/user/`.
#[derive(Serialize)]
struct ComponentMeta {
    title: String,
    description: String,
    component_hash: String,
    component_args: Vec<String>,
}

/// Write this instance's namespace: the component, and the metadata
/// naming it.
fn write_frontend(dir: &Path, params: &Params) -> Result<()> {
    // The namespace is the directory's own name. That is the only
    // channel by which this binary learns which instance it is.
    let namespace = dir
        .file_name()
        .and_then(|s| s.to_str())
        .context("--write-frontend-dir needs a path whose last segment is the namespace")?
        .to_string();

    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;

    // The component's own bytes name it. Every instance of this binary
    // computes the same digest, so two namespaces holding the same
    // component resolve to one `/modules/<hash>` URL and the browser
    // evaluates it once.
    let hash = sha256_hex(COMPONENT_JS.as_bytes());
    let js = dir.join(format!("{hash}.js"));
    if !js.exists() {
        // Write-then-rename so a reader never sees a half-written file
        // under a name that promises complete content.
        let tmp = dir.join(format!(".{hash}.tmp"));
        std::fs::write(&tmp, COMPONENT_JS).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &js).with_context(|| format!("rename into {}", js.display()))?;
    }

    let label = params
        .workspace
        .clone()
        .unwrap_or_else(|| namespace.clone());
    let meta = ComponentMeta {
        title: format!("Slack — {label}"),
        description: format!("Browse the channels mirrored into {label}."),
        component_hash: hash,
        // The gallery builds `comp.<namespace>.channels("<namespace>")`
        // from this. The argument is what tells the component which
        // backend prefix to call, and it is per-instance — which is the
        // reason the namespace had to be discoverable at all.
        component_args: vec![namespace],
    };
    let meta_path = dir.join(format!("{COMPONENT_NAME}.json"));
    std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)
        .with_context(|| format!("write {}", meta_path.display()))?;
    Ok(())
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
// Mode 2: the server
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
struct ThreadsResponse {
    channel: String,
    threads: Vec<Thread>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct Thread {
    /// The document to open — `documentView("<markdown_uuid>")`.
    markdown_uuid: String,
    /// First line of the thread, for the row label.
    title: String,
    when_ts: String,
    messages: usize,
}

#[derive(Default)]
struct ThreadData {
    title: String,
    when: Option<chrono::DateTime<chrono::Utc>>,
    when_raw: String,
    messages: usize,
}

/// `when_ts` as a comparable instant.
///
/// The field is offset-bearing RFC 3339, so string comparison is
/// wrong: `…T10:00:00+05:00` sorts after `…T08:00:00+00:00` while
/// actually being two hours earlier. Unparseable stamps sort before
/// everything, so a malformed row never displaces a good one.
fn when_key(when_ts: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(when_ts)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

/// Walk the rendered tree, grouping documents by channel.
///
/// Rows come from `.grid_rows.json` sidecars, read as untyped JSON so
/// this applet does not link the schema crate: the fields it needs
/// (`channel`, `markdown_uuid`, `when_ts`, `message_index`, `text`) are
/// part of the cross-provider contract and change far more slowly than
/// the struct.
///
/// The grouping is two levels because that is how the data is shaped:
/// Slack renders one document per thread, and every message in a thread
/// carries that thread's `markdown_uuid`. Collapsing straight to "a
/// document per channel" — which an earlier version did — picks one
/// arbitrary thread and makes the card look like the channel holds a
/// single message.
///
/// Returns the channels it could read plus the paths it could not, so
/// the caller can say the listing is partial. A silently truncated list
/// reads as authoritative, which is the worse failure.
type Channels = BTreeMap<String, BTreeMap<String, ThreadData>>;

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
                let when_raw = row.get("when_ts").and_then(|w| w.as_str()).unwrap_or("");
                let thread = by_channel
                    .entry(channel.to_string())
                    .or_default()
                    .entry(md.to_string())
                    .or_default();

                // `message_index` is absent on the row that *is* the
                // document and present on each message inside it, so it
                // is the discriminator — sturdier than matching the
                // `kind` display string.
                if row.get("message_index").map(|i| !i.is_null()) == Some(true) {
                    thread.messages += 1;
                }

                // The document row carries the thread's own title and
                // timestamp; take those in preference to a message's.
                let is_doc = row.get("message_index").map(|i| i.is_null()) != Some(false);
                let text_field = row.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if (is_doc || thread.title.is_empty()) && !text_field.is_empty() {
                    thread.title = first_line(text_field);
                }
                let key = when_key(when_raw);
                // Keep the thread's earliest stamp: that is when the
                // conversation started, which is how a reader orders
                // threads.
                let take = match (&thread.when, &key) {
                    (None, _) => true,
                    (Some(_), None) => false,
                    (Some(prev), Some(k)) => k < prev,
                };
                if take {
                    thread.when = key;
                    thread.when_raw = when_raw.to_string();
                }
            }
        }
    }
    (by_channel, warnings)
}

/// One line for a row label, trimmed so a long paste does not fill the
/// card.
fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    let mut out: String = line.chars().take(120).collect();
    if line.chars().count() > 120 {
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

fn threads_response(tree: &Path, channel: &str) -> ThreadsResponse {
    let (by_channel, warnings) = scan(tree);
    let mut threads: Vec<Thread> = by_channel
        .get(channel)
        .map(|m| {
            m.iter()
                .map(|(md, t)| Thread {
                    markdown_uuid: md.clone(),
                    title: if t.title.is_empty() {
                        "(no text)".to_string()
                    } else {
                        t.title.clone()
                    },
                    when_ts: t.when_raw.clone(),
                    messages: t.messages,
                })
                .collect()
        })
        .unwrap_or_default();
    // Newest thread first, and ties broken on the uuid so the order is
    // the same on every request over unchanged data (the walk itself is
    // unordered).
    threads.sort_by(|a, b| {
        let ka = when_key(&a.when_ts);
        let kb = when_key(&b.when_ts);
        kb.cmp(&ka)
            .then_with(|| a.markdown_uuid.cmp(&b.markdown_uuid))
    });
    ThreadsResponse {
        channel: channel.to_string(),
        threads,
        warnings,
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

fn serve(port: u16, params: &Params) -> Result<()> {
    let tree = params
        .tree
        .clone()
        .context("params.tree is required: which rendered_md tree this instance reads")?;
    // `DATALIB_APPLET_ID` is what the gateway calls this instance, so
    // it is the right fallback label when the config sets no
    // `workspace`.
    let workspace = params
        .workspace
        .clone()
        .or_else(|| std::env::var(ENV_APPLET_ID).ok())
        .unwrap_or_else(|| "slack".to_string());
    let tree_path = PathBuf::from(&tree);

    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
        .with_context(|| format!("bind 127.0.0.1:{port}"))?;
    eprintln!("datalib-view-slack: listening on 127.0.0.1:{port}, tree {tree}");

    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if let Err(e) = handle(stream, &tree_path, &workspace) {
            eprintln!("datalib-view-slack: request failed: {e:#}");
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
        "/channels" => {
            let resp = channels_response(tree, workspace);
            for w in &resp.warnings {
                eprintln!("datalib-view-slack: unreadable: {w}");
            }
            (200, serde_json::to_string(&resp)?)
        }
        // One level down: the threads in one channel. Slack renders a
        // document per thread, so this is the list a reader picks from.
        "/threads" => match query_param(query, "channel") {
            Some(channel) => {
                let resp = threads_response(tree, &channel);
                for w in &resp.warnings {
                    eprintln!("datalib-view-slack: unreadable: {w}");
                }
                (200, serde_json::to_string(&resp)?)
            }
            None => (
                400,
                serde_json::json!({ "error": "/threads needs ?channel=<name>" }).to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The namespace is read off the directory, and it lands in
    /// `component_args` — which is how the gallery's constructed call
    /// tells the component which backend prefix to use.
    #[test]
    fn the_written_metadata_names_its_own_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("slack_work");
        write_frontend(&dir, &Params::default()).unwrap();

        let meta: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("channels.json")).unwrap())
                .unwrap();
        assert_eq!(meta["component_args"], serde_json::json!(["slack_work"]));
        // …and the component it names is actually there, under a
        // filename that is its own digest.
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
            write_frontend(&dir, &Params::default()).unwrap();
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
        write_frontend(&dir, &Params::default()).unwrap();
        let first: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        write_frontend(&dir, &Params::default()).unwrap();
        let second: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(first.len(), 2);
        assert_eq!(first.len(), second.len());
    }

    /// The component's address is a pure function of its bytes.
    #[test]
    fn module_hash_is_a_function_of_the_bytes_only() {
        let a = sha256_hex(COMPONENT_JS.as_bytes());
        assert_eq!(a, sha256_hex(COMPONENT_JS.as_bytes()));
        assert_eq!(a.len(), 64);
        assert!(a
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn scanning_a_missing_tree_is_empty_and_silent() {
        // A tree no step has written yet is the normal first-run state,
        // so it must not produce a warning either.
        let (channels, warnings) = scan(Path::new("/definitely/not/here"));
        assert!(channels.is_empty());
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// One sidecar per thread, with the thread row and its messages all
    /// carrying the same markdown_uuid — the real Slack shape.
    fn write_thread(dir: &Path, md: &str, channel: &str, when: &str, texts: &[&str]) {
        let mut rows = vec![serde_json::json!({
            "uuid": md, "kind": "Slack Thread", "channel": channel,
            "when_ts": when, "markdown_uuid": md, "message_index": null,
            "text": texts.first().copied().unwrap_or(""),
        })];
        for (i, t) in texts.iter().enumerate() {
            rows.push(serde_json::json!({
                "uuid": format!("{md}-m{i}"), "kind": "Slack Message", "channel": channel,
                "when_ts": when, "markdown_uuid": md, "message_index": i, "text": t,
            }));
        }
        std::fs::write(
            dir.join(format!("{md}.grid_rows.json")),
            serde_json::json!({ "header": { "markdown_uuid": md }, "rows": rows }).to_string(),
        )
        .unwrap();
    }

    /// The regression this model exists for: a channel with several
    /// threads must report all of them, not collapse to one document.
    #[test]
    fn a_channel_reports_every_thread_and_every_message() {
        let dir = tempfile::tempdir().unwrap();
        write_thread(
            dir.path(),
            "t1",
            "#cat-qi",
            "2026-07-01T00:00:00Z",
            &["hello", "there"],
        );
        write_thread(
            dir.path(),
            "t2",
            "#cat-qi",
            "2026-07-02T00:00:00Z",
            &["joined"],
        );
        write_thread(
            dir.path(),
            "t3",
            "#chat-qi",
            "2026-07-03T00:00:00Z",
            &["elsewhere"],
        );

        let resp = channels_response(dir.path(), "ws");
        assert_eq!(resp.channels.len(), 2);
        let cat = resp.channels.iter().find(|c| c.name == "#cat-qi").unwrap();
        assert_eq!(cat.threads, 2, "both threads must be counted");
        assert_eq!(cat.messages, 3);

        let threads = threads_response(dir.path(), "#cat-qi");
        assert_eq!(threads.threads.len(), 2);
        // Newest first.
        assert_eq!(threads.threads[0].markdown_uuid, "t2");
        assert_eq!(threads.threads[1].markdown_uuid, "t1");
        assert_eq!(threads.threads[1].title, "hello");
        assert_eq!(threads.threads[1].messages, 2);
    }

    #[test]
    fn threads_for_an_unknown_channel_is_empty_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write_thread(dir.path(), "t1", "#a", "2026-07-01T00:00:00Z", &["x"]);
        assert!(threads_response(dir.path(), "#nope").threads.is_empty());
    }

    /// Thread order must not depend on the directory walk, which is a
    /// LIFO stack over unordered `read_dir`.
    #[test]
    fn thread_order_is_stable_across_runs() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..5 {
            let dir = tempfile::tempdir().unwrap();
            for md in ["a", "b", "c"] {
                write_thread(dir.path(), md, "#c", "2026-07-01T00:00:00Z", &["x"]);
            }
            let order: Vec<String> = threads_response(dir.path(), "#c")
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
        assert_eq!(
            query_param("channel=%23cat-qi", "channel").unwrap(),
            "#cat-qi"
        );
        assert_eq!(
            query_param("x=1&channel=%23a%20b", "channel").unwrap(),
            "#a b"
        );
        assert!(query_param("nope=1", "channel").is_none());
    }

    /// An unreadable file makes the listing partial, and the caller is
    /// told so rather than handed a short list that looks whole.
    #[test]
    fn unreadable_input_is_reported_not_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.grid_rows.json"), "{not json").unwrap();
        write_thread(dir.path(), "ok", "#c", "2026-07-01T00:00:00Z", &["x"]);
        let resp = channels_response(dir.path(), "ws");
        assert_eq!(resp.channels.len(), 1);
        assert_eq!(resp.warnings.len(), 1, "{:?}", resp.warnings);
    }

    #[test]
    fn long_titles_are_trimmed_to_one_line() {
        assert_eq!(first_line("first\nsecond"), "first");
        assert_eq!(first_line("   \n  real  \n"), "real");
        let long = "x".repeat(200);
        let out = first_line(&long);
        assert!(out.chars().count() <= 121 && out.ends_with('…'), "{out}");
    }
}
