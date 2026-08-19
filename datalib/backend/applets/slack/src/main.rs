//! `datalib-view-slack` — an applet: one binary that both describes
//! the frontend it contributes and serves the data behind it.
//!
//! Two modes, selected by flag:
//!
//! ```text
//! datalib-view-slack --frontend-manifest --applet-id slack_work \
//!                    --module-dir <store> --params '{"tree":"slack_work/rendered_md"}'
//! datalib-view-slack -p 41xxx --params '{"tree":"slack_work/rendered_md"}'
//! ```
//!
//! The first writes its component module into the store (named after
//! the sha256 of its bytes) and prints a manifest; the second serves
//! `/channels` over the rendered tree named in `params`.
//!
//! ## Why the manifest needs `--applet-id`
//!
//! Gallery entries are full card-source snippets, not names. Two
//! instances of this binary over two different Slack downloads must
//! emit `slack_work.channels("slack_work")` and
//! `slack_personal.channels("slack_personal")` — snippets that differ
//! only by information the binary cannot know about itself. So the id
//! is passed in, and both instances still register the *same* module
//! hash, which is what lets the browser evaluate the component once
//! and bind it twice.
//!
//! ## Why it reads sidecars rather than Slack
//!
//! The applet consumes `<tree>/**/*.grid_rows.json` — the
//! cross-provider contract every render step already emits (see
//! `datalib/backend/etl/src/grid_index.rs`). That keeps it independent
//! of the Slack provider crates and, incidentally, means the same code
//! would work over any source's rendered tree.

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
    frontend_manifest: bool,
    applet_id: Option<String>,
    module_dir: Option<PathBuf>,
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
        frontend_manifest: false,
        applet_id: None,
        module_dir: None,
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
            "--frontend-manifest" => a.frontend_manifest = true,
            "--applet-id" => a.applet_id = Some(next(&mut i)?),
            "--module-dir" => a.module_dir = Some(PathBuf::from(next(&mut i)?)),
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
    if args.frontend_manifest {
        return dump_manifest(&args);
    }
    match args.port {
        Some(p) => serve(p, &args.params),
        None => anyhow::bail!("expected --frontend-manifest or -p <port>"),
    }
}

// ---------------------------------------------------------------------------
// Mode 1: the frontend manifest
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Manifest {
    components: Vec<Component>,
    gallery: Vec<Gallery>,
}

#[derive(Serialize)]
struct Component {
    name: String,
    module: String,
}

#[derive(Serialize)]
struct Gallery {
    source: String,
    title: String,
    description: String,
}

fn dump_manifest(args: &Args) -> Result<()> {
    let id = args.applet_id.as_deref().context(
        "--frontend-manifest needs --applet-id: the gallery snippet has to name this instance",
    )?;
    let dir = args
        .module_dir
        .as_deref()
        .context("--frontend-manifest needs --module-dir")?;

    // The module's own bytes name it. Every instance of this binary
    // computes the same digest and writes the same file, so the write
    // is idempotent across instances and the browser sees one URL.
    let hash = sha256_hex(COMPONENT_JS.as_bytes());
    std::fs::create_dir_all(dir).with_context(|| format!("create module dir {}", dir.display()))?;
    let path = dir.join(&hash);
    if !path.exists() {
        // Write-then-rename so a reader never sees a half-written
        // module under a name that promises complete content.
        let tmp = dir.join(format!(".{hash}.tmp"));
        std::fs::write(&tmp, COMPONENT_JS).with_context(|| format!("write {}", tmp.display()))?;
        std::fs::rename(&tmp, &path).with_context(|| format!("rename into {}", path.display()))?;
    }

    let label = args
        .params
        .workspace
        .clone()
        .unwrap_or_else(|| id.to_string());
    let manifest = Manifest {
        components: vec![Component {
            name: COMPONENT_NAME.to_string(),
            module: hash,
        }],
        gallery: vec![Gallery {
            // The snippet, not the name. `id` selects which code (two
            // instances may be on different builds); the argument
            // tells that code which backend to call.
            source: format!("{id}.{COMPONENT_NAME}({})", json_string(id)),
            title: format!("Slack — {label}"),
            description: format!("Browse the channels mirrored into {label}."),
        }],
    };
    let out = serde_json::to_string_pretty(&manifest)?;
    println!("{out}");
    Ok(())
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).expect("string → JSON")
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
    /// Paths that could not be read. Non-empty means the channel list
    /// is partial, which the card says out loud rather than presenting
    /// a truncated list as complete.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

#[derive(Serialize)]
struct Channel {
    name: String,
    messages: usize,
    /// A document to open when the row is clicked — the newest one in
    /// the channel, which is what a reader almost always wants.
    markdown_uuid: Option<String>,
}

/// `when_ts` as a comparable instant.
///
/// The field is offset-bearing RFC 3339, so string comparison is
/// wrong: `…T10:00:00+05:00` sorts after `…T08:00:00+00:00` while
/// actually being two hours earlier. Parsing to a fixed-offset
/// datetime and comparing UTC instants is the only ordering that
/// matches what a reader means by "newest". Unparseable stamps sort
/// before everything, so a malformed row never displaces a good one.
fn when_key(when_ts: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(when_ts)
        .ok()
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

/// Walk the rendered tree and group its rows by channel.
///
/// Rows come from `.grid_rows.json` sidecars, read as untyped JSON so
/// this applet does not link the schema crate: the three fields it
/// needs (`channel`, `markdown_uuid`, `when_ts`) are part of the
/// cross-provider contract and change far more slowly than the struct.
///
/// Returns the channels it could read plus the paths it could not, so
/// the caller can tell the user the list is partial. A silently
/// truncated channel list reads as authoritative, which is the worse
/// failure: an unreadable subdirectory would look like an empty one.
fn scan_channels(tree: &Path) -> (Vec<Channel>, Vec<String>) {
    type Newest = Option<(chrono::DateTime<chrono::Utc>, String)>;
    let mut by_channel: BTreeMap<String, (usize, Newest)> = BTreeMap::new();
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
            let rows = match v.get("rows").and_then(|r| r.as_array()) {
                Some(r) => r,
                None => continue,
            };
            for row in rows {
                let Some(channel) = row.get("channel").and_then(|c| c.as_str()) else {
                    continue;
                };
                let when = row.get("when_ts").and_then(|w| w.as_str()).unwrap_or("");
                let md = row
                    .get("markdown_uuid")
                    .and_then(|m| m.as_str())
                    .map(str::to_string);
                let slot = by_channel.entry(channel.to_string()).or_insert((0, None));
                slot.0 += 1;
                if let (Some(md), Some(key)) = (md, when_key(when)) {
                    // Keep the newest document per channel. Ties break
                    // by markdown_uuid rather than by walk order: the
                    // walk drains a LIFO stack over unordered
                    // `read_dir`, so anything positional would differ
                    // between runs over identical data.
                    let better = match &slot.1 {
                        None => true,
                        Some((prev, prev_md)) => (key, &md) > (*prev, prev_md),
                    };
                    if better {
                        slot.1 = Some((key, md));
                    }
                }
            }
        }
    }
    let channels = by_channel
        .into_iter()
        .map(|(name, (messages, newest))| Channel {
            name,
            messages,
            markdown_uuid: newest.map(|(_, md)| md),
        })
        .collect();
    (channels, warnings)
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
    let path = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("/");
    let path = path.split('?').next().unwrap_or("/");

    let (status, body) = match path {
        "/channels" => {
            let (channels, warnings) = scan_channels(tree);
            for w in &warnings {
                eprintln!("datalib-view-slack: unreadable: {w}");
            }
            let resp = ChannelsResponse {
                workspace: workspace.to_string(),
                channels,
                warnings,
            };
            (200, serde_json::to_string(&resp)?)
        }
        // A readiness probe the gateway may use once it wants
        // something stronger than "the port accepts".
        "/health" => (200, r#"{"ok":true}"#.to_string()),
        _ => (
            404,
            serde_json::json!({ "error": format!("no route {path}") }).to_string(),
        ),
    };

    let reason = if status == 200 { "OK" } else { "Not Found" };
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

    /// The gallery snippet has to name the instance, since that is the
    /// only thing distinguishing two instances of this binary.
    #[test]
    fn gallery_source_addresses_its_own_instance() {
        assert_eq!(
            format!(
                "{}.{COMPONENT_NAME}({})",
                "slack_work",
                json_string("slack_work")
            ),
            r#"slack_work.channels("slack_work")"#
        );
    }

    /// Both instances must report the same module hash — that is what
    /// makes the browser evaluate the component once.
    #[test]
    fn module_hash_is_a_function_of_the_bytes_only() {
        let a = sha256_hex(COMPONENT_JS.as_bytes());
        let b = sha256_hex(COMPONENT_JS.as_bytes());
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert!(a
            .bytes()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn scanning_a_missing_tree_is_empty_and_silent() {
        // A tree no step has written yet is the normal first-run
        // state, so it must not produce a warning either.
        let (channels, warnings) = scan_channels(Path::new("/definitely/not/here"));
        assert!(channels.is_empty());
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    /// `when_ts` carries an offset, so string order is not time order.
    #[test]
    fn newest_is_by_instant_not_by_string() {
        let dir = tempfile::tempdir().unwrap();
        // 10:00+05:00 is 05:00Z — earlier than 08:00Z, though it sorts
        // later as a string.
        std::fs::write(
            dir.path().join("a.grid_rows.json"),
            r#"{"rows":[
                {"channel":"c","when_ts":"2026-01-01T08:00:00+00:00","markdown_uuid":"utc"},
                {"channel":"c","when_ts":"2026-01-01T10:00:00+05:00","markdown_uuid":"offset"}
            ]}"#,
        )
        .unwrap();
        let (channels, _) = scan_channels(dir.path());
        assert_eq!(channels[0].markdown_uuid.as_deref(), Some("utc"));
    }

    /// Ties must not depend on directory-walk order, which is not
    /// stable across runs.
    #[test]
    fn ties_break_deterministically() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..5 {
            let dir = tempfile::tempdir().unwrap();
            for (i, name) in ["x", "y", "z"].iter().enumerate() {
                std::fs::write(
                    dir.path().join(format!("{i}.grid_rows.json")),
                    format!(
                        r#"{{"rows":[{{"channel":"c","when_ts":"2026-01-01T00:00:00Z","markdown_uuid":"{name}"}}]}}"#
                    ),
                )
                .unwrap();
            }
            let (channels, _) = scan_channels(dir.path());
            seen.insert(channels[0].markdown_uuid.clone().unwrap());
        }
        assert_eq!(seen.len(), 1, "tie-break varied across runs: {seen:?}");
    }

    /// An unreadable file makes the listing partial, and the caller is
    /// told so rather than being handed a short list that looks whole.
    #[test]
    fn unreadable_input_is_reported_not_swallowed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.grid_rows.json"), "{not json").unwrap();
        let (_, warnings) = scan_channels(dir.path());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("bad.grid_rows.json"), "{warnings:?}");
    }

    #[test]
    fn groups_rows_by_channel_and_keeps_the_newest_doc() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("nested");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(
            sub.join("a.grid_rows.json"),
            r#"{"header":{},"rows":[
                {"channel":"general","when_ts":"2026-01-01T00:00:00Z","markdown_uuid":"old"},
                {"channel":"general","when_ts":"2026-06-01T00:00:00Z","markdown_uuid":"new"},
                {"channel":"random","when_ts":"2026-02-01T00:00:00Z","markdown_uuid":"r1"}
            ]}"#,
        )
        .unwrap();
        // A row with no channel belongs to no channel and is skipped.
        std::fs::write(
            dir.path().join("b.grid_rows.json"),
            r#"{"rows":[{"when_ts":"2026-03-01T00:00:00Z","markdown_uuid":"x"}]}"#,
        )
        .unwrap();

        let (channels, warnings) = scan_channels(dir.path());
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(channels.len(), 2);
        let general = channels.iter().find(|c| c.name == "general").unwrap();
        assert_eq!(general.messages, 2);
        assert_eq!(general.markdown_uuid.as_deref(), Some("new"));
        let random = channels.iter().find(|c| c.name == "random").unwrap();
        assert_eq!(random.messages, 1);
    }

    /// Malformed sidecars are skipped rather than failing the scan: a
    /// half-written file during a sync must not blank the card.
    #[test]
    fn skips_unparseable_sidecars() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("bad.grid_rows.json"), "{not json").unwrap();
        std::fs::write(
            dir.path().join("good.grid_rows.json"),
            r#"{"rows":[{"channel":"ok","when_ts":"2026-01-01T00:00:00Z","markdown_uuid":"m"}]}"#,
        )
        .unwrap();
        let (channels, warnings) = scan_channels(dir.path());
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].name, "ok");
        // The unparseable sibling is still reported.
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }
}
