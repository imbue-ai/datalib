//! Render-stage parser.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl_render::inputs::{Inputs, RawRange};
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

/// What the diff scan said: which rooms to load, and the commit read.
#[derive(Debug, Default)]
pub struct ScanResult {
    /// `Some(set)` → load only these rooms: the ones the driver found
    /// stale through their declared inputs, plus the ones the diff
    /// named. `None` → every room.
    pub render: Option<HashSet<String>>,
    /// Bucket keys the driver found stale whose room row is gone —
    /// declared with nothing so their documents go.
    pub gone: Vec<String>,
    /// The commit this parse read. `None` when nothing was committed.
    pub new_head: Option<String>,
}

#[derive(Debug, Default)]
pub struct ParsedBeeper {
    pub rooms: HashMap<String, Room>,
    /// `Vec<DocBucket>` ordered by `(room_uuid, period_key)`.
    pub docs: Vec<DocBucket>,
    /// Every raw row each loaded room reads, by room uuid: its row, its
    /// events and attachment edges from the load, its senders' `users`
    /// rows as their labels are looked up.
    pub inputs: HashMap<String, Inputs>,
    pub scan: ScanResult,
}

// Entry point

pub fn parse_raw_dir(input: &Path, source_id: &str) -> Result<ParsedBeeper> {
    parse(input, source_id, Period::Month, RawRange::cold())
}

/// Open the doltlite raw store at `<input>/entities.doltlite_db` (or
/// the path itself if it's already that file) and produce one
/// [`DocBucket`] per `(room, period)` pair with events ready for
/// rendering.
pub fn parse(
    input: &Path,
    source_id: &str,
    period: Period,
    range: RawRange<'_>,
) -> Result<ParsedBeeper> {
    let db_path = datalib_etl::doltlite_raw::db_path_for(input);
    if !db_path.is_file() {
        // Empty mirror is a valid configuration (download step
        // skipped or produced no rows). Surface it as a fresh
        // ParsedBeeper rather than a hard error so a render with no
        // data doesn't blow up the whole sync.
        return Ok(ParsedBeeper::default());
    }
    // Bridge from sync-Rust into the async sqlx API by borrowing
    // the *existing* tokio runtime. Spinning up a new
    // `Runtime::new()` here panics because the sync orchestrator
    // is already inside `#[tokio::main]`.
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current()
            .block_on(async move { parse_async(&db_path, source_id, period, range).await })
    })
}

async fn parse_async(
    db_path: &Path,
    source_id: &str,
    period: Period,
    range: RawRange<'_>,
) -> Result<ParsedBeeper> {
    // Pinned at open — at the driver's commit, else HEAD — with the views
    // installed before anything reads. No commit means nothing has been
    // committed here to render: emptiness, not a reason to read the
    // working set.
    let Some(reader) = datalib_etl::doltlite_raw::open_reader(db_path, range.pin)
        .await
        .with_context(|| format!("open raw doltlite for render at {}", db_path.display()))?
    else {
        return Ok(ParsedBeeper::default());
    };
    let pool = reader.pool().clone();
    let pin = reader.pin().clone();

    // ── rooms ──────────────────────────────────────────────────────
    let mut rooms: HashMap<String, Room> = HashMap::new();
    let room_rows = sqlx::query(
        "SELECT id, source, network, native_room_id, external_room_id,
                external_workspace_id, account_id, title, description, is_dm
         FROM pinned_rooms rooms",
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

    // ── which rooms to load ────────────────────────────────────────
    // A new or changed event, room or attachment names its room; a
    // changed row a room already declared reaches it through the
    // driver, which names buckets by the chat's entity id — mapped
    // back to the room's raw key here.
    let room_by_bucket: HashMap<String, &str> = rooms
        .keys()
        .map(|room| (super::ids::room(source_id, room).uuid, room.as_str()))
        .collect();
    let forward = datalib_etl::doltlite_raw::scan_buckets(
        &pool,
        range.cursor,
        &pin,
        &datalib_etl::doltlite_raw::DiffScanSpec {
            global_fanout_tables: &[],
            bucket_query: "
                SELECT DISTINCT room_uuid FROM (
                    SELECT coalesce(to_id, from_id) AS room_uuid
                      FROM dolt_diff_rooms
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT coalesce(to_room_uuid, from_room_uuid)
                      FROM dolt_diff_events
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT e.room_uuid
                      FROM dolt_diff_beeper_media_attachments d
                      JOIN pinned_events e ON e.id = coalesce(d.to_event_uuid, d.from_event_uuid)
                     WHERE d.from_ref = ?1 AND d.to_ref = ?2 AND d.diff_type != 'unchanged'
                )
                WHERE room_uuid IS NOT NULL
            ",
        },
    )
    .await?;
    let narrowed = range.narrow_by(forward.render.as_ref(), |key| {
        room_by_bucket.get(key).map(|r| r.to_string())
    });
    let scan = ScanResult {
        render: narrowed.render,
        gone: narrowed.gone,
        new_head: forward.new_head,
    };
    // `AND room_uuid IN (…)` on every per-room read below, bound from
    // the set; empty on a cold start.
    let room_filter = match &scan.render {
        None => String::new(),
        Some(set) => {
            let placeholders = std::iter::repeat_n("?", set.len().max(1))
                .collect::<Vec<_>>()
                .join(",");
            format!(" AND room_uuid IN ({placeholders})")
        }
    };
    // Bound in the order the placeholders were written; a set with no
    // room still binds one value so the SQL stays valid and matches none.
    let room_binds: Vec<String> = match &scan.render {
        None => Vec::new(),
        Some(set) if set.is_empty() => vec![String::new()],
        Some(set) => set.iter().cloned().collect(),
    };

    // ── per-user labels (used to populate sender_label) ────────────
    // Prefer full_name (from beeper participants) → display_name →
    // native_user_id. Stored separately rather than joined into
    // the event SELECT so a single user appearing in many events
    // only round-trips once.
    let user_rows =
        sqlx::query("SELECT id, native_user_id, display_name, full_name FROM pinned_users users")
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
        "SELECT id, event_uuid, ref_id, blake3
         FROM pinned_beeper_media_attachments beeper_media_attachments",
    )
    .fetch_all(&pool)
    .await
    .context("read beeper_media_attachments")?;
    let cas_path = datalib_etl::blob_cas::cas_path_for(db_path);
    let cas_meta: HashMap<String, (Option<String>, Option<i64>)> = if cas_path.is_file() {
        let cas_pool = datalib_etl::blob_cas::open_cas_reader(&cas_path)
            .await
            .with_context(|| format!("open CAS for render at {}", cas_path.display()))?;
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
    let mut blobs_by_owner: HashMap<String, Vec<(String, Blob)>> = HashMap::new();
    for r in &blob_rows {
        let edge_id: String = r.try_get("id")?;
        let blake3: Option<String> = r.try_get("blake3")?;
        let has_bytes = blake3.is_some();
        let ref_id: String = r.try_get("ref_id")?;
        // `ref_id` is encoded as `"{slot_index}|{display_name}"` by
        // download (see `index_db::ingest_attachment`). The display
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
        blobs_by_owner
            .entry(owner)
            .or_default()
            .push((edge_id, blob));
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
         FROM pinned_events events
         WHERE event_type != 'REACTION'{room_filter}
         GROUP BY room_uuid, period_key
         ORDER BY room_uuid, period_key"
    );
    // Audited: the interpolations are `period_expr`, built above from the
    // `Period` enum (`strftime_fmt()` / `key_for_all()`), and a `?,?,?`
    // run sized from the room set, every room bound.
    let mut bucket_query = sqlx::query(sqlx::AssertSqlSafe(bucket_sql));
    for room in &room_binds {
        bucket_query = bucket_query.bind(room);
    }
    let bucket_rows = bucket_query
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
         FROM pinned_events events
         WHERE 1 = 1{room_filter}
         ORDER BY room_uuid, timestamp_ms"
    );
    // Audited: the same two interpolations as `bucket_sql` above —
    // `period_expr` from the `Period` enum and the bound room run.
    let mut events_query = sqlx::query(sqlx::AssertSqlSafe(events_sql));
    for room in &room_binds {
        events_query = events_query.bind(room);
    }
    let event_rows = events_query
        .fetch_all(&pool)
        .await
        .context("read events for bucketing")?;

    // Every row a room reads, recorded as it is read. Rooms this run
    // looks at that have no events still declare their own row, so a
    // room whose events all went keeps nothing.
    let mut inputs: HashMap<String, Inputs> = HashMap::new();
    for room_uuid in scan.render.iter().flatten() {
        inputs
            .entry(room_uuid.clone())
            .or_default()
            .read("rooms", room_uuid);
    }

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

        let read = inputs.entry(room_uuid.clone()).or_default();
        read.read("rooms", &room_uuid);
        read.read("events", &event_uuid);
        let users = read.lookup("users", &user_label);
        let sender_label = sender_uuid.as_ref().and_then(|u| users.get(u).cloned());
        let blobs: Vec<Blob> = blobs_by_owner
            .get(&event_uuid)
            .map(|edges| {
                edges
                    .iter()
                    .map(|(edge_id, blob)| {
                        read.read("beeper_media_attachments", edge_id);
                        blob.clone()
                    })
                    .collect()
            })
            .unwrap_or_default();

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

    Ok(ParsedBeeper {
        rooms,
        docs,
        inputs,
        scan,
    })
}
