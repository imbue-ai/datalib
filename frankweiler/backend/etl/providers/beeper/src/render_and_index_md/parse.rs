//! Translate-stage parser.
//!
//! Opens the doltlite raw store the extract stage built and pulls
//! out the data we need to render. The high-level grouping
//! (`(room, period)` → rendered document) is done in SQL via
//! GROUP BY + strftime so SQLite shoulders the bucketing. The
//! finer details — attaching reactions to their target's period
//! when target and reaction landed in different periods — happen
//! in Rust, because that's a graph traversal SQL can't express
//! cleanly.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::Row;

use super::Period;

/// Distilled room row, ready for rendering.
#[derive(Debug, Clone)]
pub struct Room {
    pub room_uuid: String,
    pub source: String,
    pub network: String,
    pub native_room_id: String,
    pub external_room_id: Option<String>,
    pub external_workspace_id: Option<String>,
    pub account_id: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub is_dm: bool,
}

/// One rendered-event entry. Both top-level messages and reactions
/// flow through this same shape; renderers branch on
/// [`Event::is_reaction`].
#[derive(Debug, Clone)]
pub struct Event {
    pub event_uuid: String,
    pub native_event_id: String,
    pub external_event_id: Option<String>,
    pub event_type: String,
    pub timestamp_ms: i64,
    pub sender_uuid: Option<String>,
    pub sender_label: Option<String>,
    pub text_content: Option<String>,
    pub reply_to_native_event_id: Option<String>,
    pub edit_of_native_event_id: Option<String>,
    pub reaction_emoji: Option<String>,
    pub reaction_target_native_event_id: Option<String>,
    /// Blobs attached to this event (resolved at parse-time so
    /// renderers don't need their own SQL).
    pub blobs: Vec<Blob>,
}

impl Event {
    pub fn is_reaction(&self) -> bool {
        self.event_type == "REACTION"
    }

    pub fn is_hidden(&self) -> bool {
        self.event_type == "HIDDEN"
    }
}

#[derive(Debug, Clone)]
pub struct Blob {
    pub blob_id: String,
    pub slot: String,
    pub content_type: Option<String>,
    pub byte_len: Option<i64>,
    pub source_url: Option<String>,
    /// blake3 hex of the CAS object holding the bytes, when fetched.
    pub blake3: Option<String>,
    /// Whether the bytes are actually populated (vs metadata-only).
    pub has_bytes: bool,
}

/// One rendered document's worth of events: all messages whose
/// own period bucket matches the doc's `(room, period_key)`, plus
/// any reactions whose targets fall here regardless of when the
/// reaction itself landed.
#[derive(Debug, Clone)]
pub struct DocBucket {
    pub room_uuid: String,
    pub period_key: String,
    /// Wall-clock bounds across the messages included in the doc.
    /// Excludes reactions (so adding a late reaction doesn't move
    /// the bounds).
    pub first_ms: i64,
    pub last_ms: i64,
    /// Messages in chronological order.
    pub messages: Vec<Event>,
    /// Reactions whose target falls in this bucket. Keyed by
    /// target `native_event_id` so renderers can index quickly.
    pub reactions_by_target: BTreeMap<String, Vec<Event>>,
}

#[derive(Debug, Default)]
pub struct ParsedBeeper {
    pub rooms: HashMap<String, Room>,
    /// `Vec<DocBucket>` ordered by `(room_uuid, period_key)`.
    pub docs: Vec<DocBucket>,
}

// ─────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────

/// Compatibility entry — sync/main.rs calls this with no period
/// knob, in which case we default to `Month`.
pub fn parse_raw_dir(input: &Path) -> Result<ParsedBeeper> {
    parse(input, Period::Month)
}

/// Open the doltlite raw store at `<input>/entities.doltlite_db` (or
/// the path itself if it's already that file) and produce one
/// [`DocBucket`] per `(room, period)` pair with events ready for
/// rendering.
pub fn parse(input: &Path, period: Period) -> Result<ParsedBeeper> {
    let db_path = frankweiler_etl::doltlite_raw::db_path_for(input);
    if !db_path.is_file() {
        // Empty mirror is a valid configuration (extract step
        // skipped or produced no rows). Surface it as a fresh
        // ParsedBeeper rather than a hard error so a translate-only
        // run with no data doesn't blow up the whole sync.
        return Ok(ParsedBeeper::default());
    }
    // Bridge from sync-Rust into the async sqlx API by borrowing
    // the *existing* tokio runtime. Spinning up a new
    // `Runtime::new()` here panics because the sync orchestrator
    // is already inside `#[tokio::main]`.
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current()
            .block_on(async move { parse_async(&db_path, period).await })
    })
}

async fn parse_async(db_path: &Path, period: Period) -> Result<ParsedBeeper> {
    let opts =
        SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?.read_only(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .with_context(|| format!("open raw doltlite for translate at {}", db_path.display()))?;

    // ── rooms ──────────────────────────────────────────────────────
    let mut rooms: HashMap<String, Room> = HashMap::new();
    let room_rows = sqlx::query(
        "SELECT id, source, network, native_room_id, external_room_id,
                external_workspace_id, account_id, title, description, is_dm
         FROM rooms",
    )
    .fetch_all(&pool)
    .await
    .context("read rooms")?;
    for r in &room_rows {
        let room_uuid: String = r.try_get("id")?;
        rooms.insert(
            room_uuid.clone(),
            Room {
                room_uuid,
                source: r.try_get("source")?,
                network: r.try_get("network")?,
                native_room_id: r.try_get("native_room_id")?,
                external_room_id: r.try_get("external_room_id")?,
                external_workspace_id: r.try_get("external_workspace_id")?,
                account_id: r.try_get("account_id")?,
                title: r.try_get("title")?,
                description: r.try_get("description")?,
                is_dm: r.try_get::<i64, _>("is_dm")? != 0,
            },
        );
    }

    // ── per-user labels (used to populate sender_label) ────────────
    // Prefer full_name (from beeper participants) → display_name →
    // native_user_id. Stored separately rather than joined into
    // the event SELECT so a single user appearing in many events
    // only round-trips once.
    let user_rows = sqlx::query("SELECT id, native_user_id, display_name, full_name FROM users")
        .fetch_all(&pool)
        .await
        .context("read users")?;
    let mut user_label: HashMap<String, String> = HashMap::new();
    for r in &user_rows {
        let id: String = r.try_get("id")?;
        let mxid: String = r.try_get("native_user_id")?;
        let full: Option<String> = r.try_get("full_name")?;
        let disp: Option<String> = r.try_get("display_name")?;
        let label = full.or(disp).unwrap_or(mxid);
        user_label.insert(id, label);
    }

    // ── blobs by owning event uuid ─────────────────────────────────
    // Edge table is `beeper_media_attachments` (universal CasEdgeRow
    // shape: id PK, event_uuid owning FK, ref_id, nullable blake3).
    // The schema_raw.rs docstring explains the shape. `content_type`
    // / `byte_len` no longer live alongside the ref — they're a
    // property of the bytes themselves, so we look them up in
    // `cas_objects` via the sibling CAS pool, mirroring how every
    // other ported provider grabs that metadata at render time.
    let blob_rows = sqlx::query(
        "SELECT event_uuid, ref_id, blake3
         FROM beeper_media_attachments",
    )
    .fetch_all(&pool)
    .await
    .context("read beeper_media_attachments")?;
    let cas_path = frankweiler_etl::blob_cas::cas_path_for(db_path);
    let cas_meta: HashMap<String, (Option<String>, Option<i64>)> = if cas_path.is_file() {
        let cas_opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", cas_path.display()))?
            .read_only(true);
        let cas_pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(cas_opts)
            .await
            .with_context(|| format!("open CAS for translate at {}", cas_path.display()))?;
        let rows = sqlx::query("SELECT blake3, content_type, byte_len FROM cas_objects")
            .fetch_all(&cas_pool)
            .await
            .context("read cas_objects")?;
        cas_pool.close().await;
        let mut out: HashMap<String, (Option<String>, Option<i64>)> = HashMap::new();
        for r in &rows {
            let h: String = r.try_get("blake3")?;
            let ct: Option<String> = r.try_get("content_type")?;
            let bl: Option<i64> = r.try_get("byte_len")?;
            out.insert(h, (ct, bl));
        }
        out
    } else {
        HashMap::new()
    };
    let mut blobs_by_owner: HashMap<String, Vec<Blob>> = HashMap::new();
    for r in &blob_rows {
        let blake3: Option<String> = r.try_get("blake3")?;
        let has_bytes = blake3.is_some();
        let ref_id: String = r.try_get("ref_id")?;
        // `ref_id` is encoded as `"{slot_index}|{display_name}"` by
        // extract (see `index_db::ingest_attachment`). The display
        // half is what render uses as the markdown link's alt text.
        let slot = ref_id
            .split_once('|')
            .map(|(_, name)| name.to_string())
            .unwrap_or_else(|| ref_id.clone());
        let (content_type, byte_len) = blake3
            .as_deref()
            .and_then(|h| cas_meta.get(h))
            .cloned()
            .unwrap_or_default();
        let blob = Blob {
            blob_id: ref_id,
            slot,
            content_type,
            byte_len,
            source_url: None,
            blake3,
            has_bytes,
        };
        let owner: String = r.try_get("event_uuid")?;
        blobs_by_owner.entry(owner).or_default().push(blob);
    }

    // ── doc enumeration via GROUP BY (THE big SQL step) ────────────
    // Build the period expression once. `All` collapses every
    // event into a single bucket via a constant column rather
    // than strftime.
    let period_expr: String = match period {
        Period::All => format!("'{}'", Period::key_for_all()),
        _ => format!(
            "strftime('{fmt}', timestamp_ms/1000, 'unixepoch')",
            fmt = period.strftime_fmt()
        ),
    };

    // GROUP BY here is the document-grouping the user asked for —
    // SQLite does the (room, period) partitioning natively;
    // reactions are excluded from doc enumeration because they
    // get attached to their target's bucket later, not their own.
    // HIDDEN events ARE included so that a room consisting
    // entirely of system / membership events still gets a
    // rendered file (and so the bucket's first_ms/last_ms reflect
    // the real activity envelope).
    let bucket_sql = format!(
        "SELECT room_uuid,
                {period_expr} AS period_key,
                MIN(timestamp_ms) AS first_ms,
                MAX(timestamp_ms) AS last_ms,
                COUNT(*) AS event_count
         FROM events
         WHERE event_type != 'REACTION'
         GROUP BY room_uuid, period_key
         ORDER BY room_uuid, period_key"
    );
    let bucket_rows = sqlx::query(&bucket_sql)
        .fetch_all(&pool)
        .await
        .context("group events by (room, period)")?;

    let mut docs: Vec<DocBucket> = Vec::with_capacity(bucket_rows.len());

    // ── pull every non-HIDDEN event once and bucket it ─────────────
    // Doing this as a single scan (rather than N per-doc SELECTs)
    // avoids hammering sqlite when a user has hundreds of
    // conversations. The period column is computed in SQL so the
    // bucketing keys line up with the GROUP BY result above.
    // HIDDEN events are returned too — they get a one-liner in
    // the markdown output so a translator-aware reader can see
    // them, but they're tagged distinctly enough that downstream
    // filters can drop them cheaply.
    let events_sql = format!(
        "SELECT id, room_uuid, native_event_id, external_event_id, event_type,
                timestamp_ms, sender_uuid, text_content,
                reply_to_native_event_id, edit_of_native_event_id,
                reaction_emoji, reaction_target_native_event_id,
                {period_expr} AS period_key
         FROM events
         ORDER BY room_uuid, timestamp_ms"
    );
    let event_rows = sqlx::query(&events_sql)
        .fetch_all(&pool)
        .await
        .context("read events for bucketing")?;

    // Index 1: native_event_id → period_key, for resolving where
    // a reaction's target falls (the reaction itself may have
    // landed in a different period).
    let mut target_period: HashMap<(String, String), String> = HashMap::new();
    for r in &event_rows {
        let room_uuid: String = r.try_get("room_uuid")?;
        let native_id: String = r.try_get("native_event_id")?;
        let period_key: String = r.try_get("period_key")?;
        let ev_type: String = r.try_get("event_type")?;
        if ev_type != "REACTION" {
            target_period.insert((room_uuid, native_id), period_key);
        }
    }

    // Pre-build empty DocBuckets, keyed by (room, period) so we
    // can find the right one quickly during the second scan.
    let mut bucket_idx: HashMap<(String, String), usize> = HashMap::new();
    for r in &bucket_rows {
        let room_uuid: String = r.try_get("room_uuid")?;
        let period_key: String = r.try_get("period_key")?;
        bucket_idx.insert((room_uuid.clone(), period_key.clone()), docs.len());
        docs.push(DocBucket {
            room_uuid,
            period_key,
            first_ms: r.try_get("first_ms")?,
            last_ms: r.try_get("last_ms")?,
            messages: Vec::new(),
            reactions_by_target: BTreeMap::new(),
        });
    }

    // Second scan: place each event in its right bucket.
    for r in &event_rows {
        let room_uuid: String = r.try_get("room_uuid")?;
        let event_uuid: String = r.try_get("id")?;
        let native_event_id: String = r.try_get("native_event_id")?;
        let event_type: String = r.try_get("event_type")?;
        let timestamp_ms: i64 = r.try_get("timestamp_ms")?;
        let sender_uuid: Option<String> = r.try_get("sender_uuid")?;
        let text_content: Option<String> = r.try_get("text_content")?;
        let external_event_id: Option<String> = r.try_get("external_event_id")?;
        let reply_to_native_event_id: Option<String> = r.try_get("reply_to_native_event_id")?;
        let edit_of_native_event_id: Option<String> = r.try_get("edit_of_native_event_id")?;
        let reaction_emoji: Option<String> = r.try_get("reaction_emoji")?;
        let reaction_target_native_event_id: Option<String> =
            r.try_get("reaction_target_native_event_id")?;
        let own_period: String = r.try_get("period_key")?;

        let sender_label = sender_uuid
            .as_ref()
            .and_then(|u| user_label.get(u).cloned());
        let blobs = blobs_by_owner.get(&event_uuid).cloned().unwrap_or_default();

        let ev = Event {
            event_uuid: event_uuid.clone(),
            native_event_id: native_event_id.clone(),
            external_event_id,
            event_type: event_type.clone(),
            timestamp_ms,
            sender_uuid,
            sender_label,
            text_content,
            reply_to_native_event_id,
            edit_of_native_event_id,
            reaction_emoji,
            reaction_target_native_event_id: reaction_target_native_event_id.clone(),
            blobs,
        };

        if event_type == "REACTION" {
            // Place the reaction in the bucket of its target. If
            // we don't have the target (orphaned reaction), fall
            // back to the reaction's own period, so it still
            // shows up somewhere.
            let target = reaction_target_native_event_id
                .as_ref()
                .and_then(|t| target_period.get(&(room_uuid.clone(), t.clone())).cloned());
            let dest_period = target.unwrap_or(own_period);
            // The (room, dest_period) bucket might not exist if
            // the target's period bucket was created without
            // including reactions in the COUNT (it wasn't — the
            // GROUP BY filtered REACTION out). Lazily create it.
            let key = (room_uuid.clone(), dest_period.clone());
            let idx = match bucket_idx.get(&key) {
                Some(&i) => i,
                None => {
                    let i = docs.len();
                    bucket_idx.insert(key, i);
                    docs.push(DocBucket {
                        room_uuid: room_uuid.clone(),
                        period_key: dest_period,
                        first_ms: timestamp_ms,
                        last_ms: timestamp_ms,
                        messages: Vec::new(),
                        reactions_by_target: BTreeMap::new(),
                    });
                    i
                }
            };
            let target_key = ev
                .reaction_target_native_event_id
                .clone()
                .unwrap_or_else(|| ev.native_event_id.clone());
            docs[idx]
                .reactions_by_target
                .entry(target_key)
                .or_default()
                .push(ev);
        } else {
            let key = (room_uuid, own_period);
            if let Some(&idx) = bucket_idx.get(&key) {
                docs[idx].messages.push(ev);
            }
        }
    }

    Ok(ParsedBeeper { rooms, docs })
}
