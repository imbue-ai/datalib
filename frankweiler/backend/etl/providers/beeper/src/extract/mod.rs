//! Beeper downloader entry point.
//!
//! Captures the user's Beeper account into a single doltlite database
//! at `<data_root>/raw/<name>.doltlite_db` — one row per room, user,
//! and Matrix event. Beeper is a Matrix homeserver
//! (`matrix.beeper.com`) with bridges that translate iMessage,
//! WhatsApp, Signal, Telegram, Discord, … into Matrix rooms. We talk
//! to it via the standard Matrix Client-Server API; per-bridge
//! semantics are deferred to the Translate stage.
//!
//! Resume cursor: derived at startup from the DB. The
//! `sync_cursors` table records each room's `prev_batch` token from
//! the previous `/messages?dir=b` walk so a re-run picks up where the
//! last left off.

pub mod api;
pub mod db;

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tracing::{info, instrument};

use api::{matrix_get, matrix_get_with_timeout, MatrixError, LATCHKEY_SYNC_TIMEOUT};
pub use db::{db_path_for, RawDb};

use crate::translate::{beeper_room_uuid, beeper_user_uuid};

pub const DEFAULT_REFRESH_WINDOW_DAYS: i64 = 14;

/// Networks Beeper exposes via its bridge bots. Used both to label
/// extracted rooms and as the allow-list for `--networks` filtering.
/// Maintained as a string lookup against the localpart prefix of a
/// bridge bot's mxid (e.g. `@whatsappbot:beeper.local`).
const KNOWN_NETWORKS: &[(&str, &str)] = &[
    ("imessage", "imessage"),
    ("whatsapp", "whatsapp"),
    ("signal", "signal"),
    ("telegram", "telegram"),
    ("discord", "discord"),
    ("linkedin", "linkedin"),
    ("twitter", "twitter"),
    ("instagram", "instagram"),
    ("gmessages", "gmessages"),
    ("googlechat", "googlechat"),
    ("slack", "slack"),
];

/// Public knob set passed from the CLI or the sync orchestrator.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Path to the doltlite database file. A legacy directory path is
    /// rewritten to `<dir>.doltlite_db` by [`db_path_for`].
    pub db_path: PathBuf,
    /// When non-empty, restrict the fetch to rooms whose inferred
    /// `bridge_network` is in this list. Empty = all networks.
    pub networks: Vec<String>,
    /// When non-empty, restrict the fetch to exactly these matrix
    /// room IDs. Takes precedence over `networks`.
    pub rooms: Vec<String>,
    /// On each run, re-walk the trailing N days to pick up edits and
    /// reactions that landed on previously-stored events. Milestone A
    /// does not paginate `/messages` yet, so this is unused.
    #[allow(dead_code)]
    pub refresh_window_days: i64,
    /// Download media (`mxc://` attachments). Off = JSON metadata only.
    /// Milestone A: not implemented.
    #[allow(dead_code)]
    pub media: bool,
    pub progress: frankweiler_etl::progress::Progress,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            db_path: PathBuf::new(),
            networks: Vec::new(),
            rooms: Vec::new(),
            refresh_window_days: DEFAULT_REFRESH_WINDOW_DAYS,
            media: true,
            progress: frankweiler_etl::progress::Progress::noop(),
        }
    }
}

#[derive(Debug, Default)]
pub struct FetchSummary {
    pub rooms: usize,
    pub users: usize,
    pub events: usize,
    pub requests: u64,
}

#[instrument(skip_all, fields(db = %opts.db_path.display()))]
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db_path = db_path_for(&opts.db_path);
    let _ = frankweiler_etl::latchkey::ensure_curl_shim();
    let db = RawDb::open(&db_path)
        .await
        .with_context(|| format!("open raw db {}", db_path.display()))?;

    let run_config = json!({
        "networks": opts.networks,
        "rooms": opts.rooms,
        "refresh_window_days": opts.refresh_window_days,
        "media": opts.media,
    });
    let run_id = db.start_run(&run_config).await?;

    let mut summary = FetchSummary::default();
    let result = drive(&db, &opts, &mut summary).await;

    let summary_json = json!({
        "rooms": summary.rooms,
        "users": summary.users,
        "events": summary.events,
        "requests": summary.requests,
        "error": result.as_ref().err().map(|e| e.to_string()),
    });
    let status = if result.is_ok() { "ok" } else { "error" };
    let _ = db.finish_run(run_id, status, &summary_json).await;
    result?;

    info!(
        event = "beeper_fetch_complete",
        rooms = summary.rooms,
        users = summary.users,
        events = summary.events,
        requests = summary.requests,
    );
    Ok(summary)
}

async fn drive(db: &RawDb, opts: &FetchOptions, summary: &mut FetchSummary) -> Result<()> {
    // ── whoami ─────────────────────────────────────────────────────
    // Cheap identity probe up front so a bad token fails fast with a
    // clear 401 (rather than getting buried in a 50MB /sync response).
    let whoami = matrix_get("/_matrix/client/v3/account/whoami", &BTreeMap::new())
        .await
        .map_err(|e: MatrixError| anyhow::anyhow!("{}", e))
        .context("whoami")?;
    summary.requests += 1;
    let self_mxid = whoami
        .get("user_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("whoami: missing user_id"))?
        .to_string();
    db.upsert_user(&self_mxid, None, &whoami).await?;
    summary.users += 1;

    // ── /sync ──────────────────────────────────────────────────────
    // One call for every joined room's state + first timeline page.
    // Resume across runs via the global `next_batch` cursor stored in
    // sync_scope_state.
    //
    // The filter is a JSON document passed as a single query param.
    // We ask for:
    //   - timeline.limit = 50          (recent events per room)
    //   - room.state.lazy_load_members = false  (need full member
    //     state for bridge-bot inference + participant rows)
    // We deliberately DON'T set `account_data`/`presence`/`to_device`
    // filters; defaults are fine and we'd rather see the data.
    //
    // First run: `since=` omitted, `full_state=true`, `timeout=0`.
    // Subsequent runs: `since=<prev next_batch>`, `full_state=false`.
    let prior_next_batch = db.read_next_batch().await?;
    let mut params = BTreeMap::new();
    params.insert(
        "filter".to_string(),
        r#"{"room":{"timeline":{"limit":50},"state":{"lazy_load_members":false}}}"#
            .to_string(),
    );
    params.insert("timeout".to_string(), "0".to_string());
    if let Some(since) = prior_next_batch.as_deref() {
        params.insert("since".to_string(), since.to_string());
        params.insert("full_state".to_string(), "false".to_string());
        info!(event = "beeper_sync_resume", since = %since);
    } else {
        params.insert("full_state".to_string(), "true".to_string());
        info!(event = "beeper_sync_initial");
    }
    let opts_progress = opts.progress.clone();
    opts_progress.set_message("syncing");
    let sync = matrix_get_with_timeout(
        "/_matrix/client/v3/sync",
        &params,
        LATCHKEY_SYNC_TIMEOUT,
    )
    .await
    .map_err(|e| anyhow::anyhow!("{}", e))
    .context("sync")?;
    summary.requests += 1;

    // Optional explicit-rooms filter (paste-able matrix.to URLs are
    // normalised to the trailing `!…:server` token).
    let explicit: Vec<String> = opts.rooms.iter().map(|s| normalize_room(s)).collect();

    // Walk rooms.join.<room_id> → upsert room + state members + timeline.
    let empty = serde_json::Map::new();
    let joined = sync
        .pointer("/rooms/join")
        .and_then(|v| v.as_object())
        .unwrap_or(&empty);
    info!(event = "beeper_sync_rooms", count = joined.len());
    opts_progress.set_length(Some(joined.len() as u64));

    for (matrix_room_id, room_value) in joined {
        if !explicit.is_empty() && !explicit.iter().any(|r| r == matrix_room_id) {
            opts_progress.inc(1);
            continue;
        }

        // Build a synthetic state-events array (the shape the rest of
        // our extract code expects) by concatenating
        // `state.events` (full-state snapshot or delta) with any
        // state events from the timeline (membership changes etc.).
        let mut all_state: Vec<Value> = Vec::new();
        if let Some(arr) = room_value.pointer("/state/events").and_then(|v| v.as_array()) {
            all_state.extend(arr.iter().cloned());
        }
        // Timeline can carry state events too (state_key present); we
        // include them when sniffing room info so a rename caught
        // mid-window applies.
        if let Some(arr) = room_value
            .pointer("/timeline/events")
            .and_then(|v| v.as_array())
        {
            for ev in arr {
                if ev.get("state_key").is_some() {
                    all_state.push(ev.clone());
                }
            }
        }
        let state_payload = Value::Array(all_state.clone());

        let info = extract_room_info(matrix_room_id, &state_payload);
        if !opts.networks.is_empty()
            && !opts
                .networks
                .iter()
                .any(|n| n.eq_ignore_ascii_case(&info.bridge_network))
        {
            opts_progress.inc(1);
            continue;
        }

        db.upsert_room(&info, &state_payload).await?;
        summary.rooms += 1;

        // Member rows from state.
        for ev in &all_state {
            if ev.get("type").and_then(|v| v.as_str()) != Some("m.room.member") {
                continue;
            }
            let Some(mxid) = ev.get("state_key").and_then(|v| v.as_str()) else {
                continue;
            };
            db.upsert_user(mxid, Some(&info.bridge_network), ev).await?;
            summary.users += 1;
        }

        // Timeline events: store everything the sync returned. Walking
        // back further is Milestone B's job, via the `prev_batch`
        // token we record below.
        let timeline_events = room_value
            .pointer("/timeline/events")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let mut last_event_id: Option<String> = None;
        for ev in &timeline_events {
            db.upsert_event(matrix_room_id, ev).await?;
            summary.events += 1;
            if let Some(eid) = ev.get("event_id").and_then(|v| v.as_str()) {
                last_event_id = Some(eid.to_string());
            }
        }

        // Per-room cursor for Milestone B backfill. `prev_batch` is
        // the token pointing one event before the earliest timeline
        // event we got; Matrix says nothing about whether it's
        // present, so we tolerate its absence.
        let prev_batch = room_value
            .pointer("/timeline/prev_batch")
            .and_then(|v| v.as_str());
        db.upsert_sync_cursor(matrix_room_id, prev_batch, last_event_id.as_deref())
            .await?;

        opts_progress.inc(1);
        opts_progress.set_message(&format!(
            "rooms={} users={} events={}",
            summary.rooms, summary.users, summary.events
        ));
    }

    // Persist the global cursor LAST — so a crash mid-room leaves us
    // with a stable resume point at the previous run's next_batch.
    if let Some(nb) = sync.get("next_batch").and_then(|v| v.as_str()) {
        db.write_next_batch(nb).await?;
    }

    Ok(())
}

/// Distilled per-room columns surfaced from the `/state` response.
/// Carries enough to populate the `rooms` table; the full state payload
/// goes into `rooms.payload` for translate-time lookup of anything else.
#[derive(Debug, Clone)]
pub struct RoomInfo {
    pub matrix_room_id: String,
    pub bridge_network: String,
    pub bridge_protocol: Option<String>,
    pub display_name: Option<String>,
    pub topic: Option<String>,
    pub is_dm: bool,
    pub is_space: bool,
}

fn extract_room_info(matrix_room_id: &str, state: &Value) -> RoomInfo {
    let events = state.as_array().map(Vec::as_slice).unwrap_or(&[]);
    let mut display_name: Option<String> = None;
    let mut topic: Option<String> = None;
    let mut is_space = false;
    let mut bridge_network: Option<String> = None;
    let mut bridge_protocol: Option<String> = None;
    let mut member_count: usize = 0;
    for ev in events {
        match ev.get("type").and_then(|v| v.as_str()) {
            Some("m.room.name") => {
                display_name = ev
                    .pointer("/content/name")
                    .and_then(|v| v.as_str())
                    .map(String::from);
            }
            Some("m.room.topic") => {
                topic = ev
                    .pointer("/content/topic")
                    .and_then(|v| v.as_str())
                    .map(String::from);
            }
            Some("m.room.create") => {
                if ev.pointer("/content/type").and_then(|v| v.as_str()) == Some("m.space") {
                    is_space = true;
                }
            }
            Some("m.bridge") | Some("uk.half-shot.bridge") => {
                // Beeper bridges populate the `m.bridge` state event
                // (MSC2346 style). The protocol id sits at
                // content.protocol.id; on Beeper this is the network
                // tag we want.
                if let Some(proto) =
                    ev.pointer("/content/protocol/id").and_then(|v| v.as_str())
                {
                    bridge_protocol = Some(proto.to_string());
                    bridge_network = Some(proto.to_string());
                }
            }
            Some("m.room.member") => {
                if ev.pointer("/content/membership").and_then(|v| v.as_str())
                    == Some("join")
                {
                    member_count += 1;
                }
                // Fallback network detection: bridge bots have a
                // distinctive localpart prefix (e.g.
                // `@whatsappbot:beeper.local`,
                // `@signal_+15551234567:beeper.local`).
                if bridge_network.is_none() {
                    if let Some(mxid) = ev.get("state_key").and_then(|v| v.as_str()) {
                        if let Some(net) = network_from_mxid(mxid) {
                            bridge_network = Some(net);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    RoomInfo {
        matrix_room_id: matrix_room_id.to_string(),
        bridge_network: bridge_network.unwrap_or_else(|| "matrix".to_string()),
        bridge_protocol,
        display_name,
        topic,
        // DMs are 2-member non-space rooms. Beeper also stamps
        // `is_direct` on the `m.room.member` for the other party but
        // that requires fishing through account_data; the 2-member
        // heuristic matches what the desktop app uses in practice.
        is_dm: !is_space && member_count == 2,
        is_space,
    }
}

/// Localpart-prefix sniff for mapping a bridge-bot mxid back to a
/// network tag. Matches the well-known prefixes Beeper assigns in
/// `beeper.local`-namespaced users.
fn network_from_mxid(mxid: &str) -> Option<String> {
    let local = mxid.strip_prefix('@')?;
    let local = local.split(':').next()?;
    for (prefix, network) in KNOWN_NETWORKS {
        if local.starts_with(prefix) {
            return Some((*network).to_string());
        }
    }
    None
}

/// Normalize a paste-able room reference. Accepts a bare
/// `!abc:beeper.com`, a `matrix.to/#/!abc:beeper.com` URL, or a
/// `https://matrix.to/#/!abc%3Abeeper.com` percent-encoded form.
fn normalize_room(s: &str) -> String {
    let trimmed = s.trim();
    let last = trimmed
        .rsplit('/')
        .next()
        .unwrap_or(trimmed)
        .trim_start_matches('#');
    // Decode the single %3A that matrix.to URLs put around the colon.
    last.replace("%3A", ":").replace("%3a", ":")
}

// ─────────────────────────────────────────────────────────────────────
// Reuse helpers for tests
// ─────────────────────────────────────────────────────────────────────

#[doc(hidden)]
pub fn __room_uuid(matrix_room_id: &str) -> String {
    beeper_room_uuid(matrix_room_id)
}

#[doc(hidden)]
pub fn __user_uuid(matrix_user_id: &str) -> String {
    beeper_user_uuid(matrix_user_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn network_from_imessage_bot_mxid() {
        assert_eq!(
            network_from_mxid("@imessagebot:beeper.local"),
            Some("imessage".to_string())
        );
        assert_eq!(
            network_from_mxid("@signal_+15551234567:beeper.local"),
            Some("signal".to_string())
        );
        assert_eq!(network_from_mxid("@thad:beeper.com"), None);
    }

    #[test]
    fn normalize_matrix_to_url() {
        assert_eq!(
            normalize_room("https://matrix.to/#/!abc:beeper.com"),
            "!abc:beeper.com"
        );
        assert_eq!(
            normalize_room("https://matrix.to/#/!abc%3Abeeper.com"),
            "!abc:beeper.com"
        );
        assert_eq!(normalize_room("  !abc:beeper.com  "), "!abc:beeper.com");
    }

    #[test]
    fn extract_room_info_imessage_dm() {
        let state = json!([
            {"type": "m.room.create", "content": {}},
            {"type": "m.room.name", "content": {"name": "Alice"}},
            {
                "type": "m.bridge",
                "content": {"protocol": {"id": "imessage"}}
            },
            {
                "type": "m.room.member",
                "state_key": "@imessagebot:beeper.local",
                "content": {"membership": "join"}
            },
            {
                "type": "m.room.member",
                "state_key": "@thad:beeper.com",
                "content": {"membership": "join"}
            }
        ]);
        let info = extract_room_info("!abc:beeper.com", &state);
        assert_eq!(info.bridge_network, "imessage");
        assert_eq!(info.display_name.as_deref(), Some("Alice"));
        assert!(info.is_dm);
        assert!(!info.is_space);
    }
}
