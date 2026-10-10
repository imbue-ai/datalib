//! Parse the doltlite raw store into a small in-memory `ParsedSignal`
//! that the renderer can walk without re-querying.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use datalib_etl::blob_cas::{self, BlobBundle, CasEdgeRow};
use datalib_etl::periodize::Period;
use datalib_etl_render::inputs::{Inputs, RawRange};
use datalib_etl_signal::ingest::schema_raw::ChatItemAttachmentRow;
use datalib_signal_backup::backup;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

/// SQL projection from Signal's `chat_item_attachments` edge to its
/// CAS blake3. Consumed by [`BlobBundle::load_many`].
const ATTACHMENTS_PROJECTION_SQL: &str = "
    SELECT ref_id, blake3,
           NULL AS content_type, NULL AS upstream_name
      FROM chat_item_attachments
     WHERE ref_id IN ({placeholders}) AND blake3 IS NOT NULL";

/// Result of the dolt_diff scan: the chats we need to re-render, the
/// current HEAD hash to stamp into the next cursor, and how long the
/// diff query took. All three travel together so `render` can write
/// the cursor + log the elapsed_ms without a second round-trip.
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    /// `Some(set)` → render only chats whose id is in `set`: the ones
    /// the driver found stale through their declared inputs, plus the
    /// ones the diff named. `None` → cold start, render every chat.
    pub render: Option<HashSet<String>>,
    /// Bucket keys the driver found stale whose `chats` row is gone —
    /// declared with nothing so their documents go.
    pub gone: Vec<String>,
    /// The HEAD commit hash at scan time, ready to stamp into the
    /// render cursor on success. `None` if we couldn't read HEAD —
    /// next run is another cold start.
    pub new_head: Option<String>,
    /// Wall-clock time spent in the `dolt_diff_<table>` union query.
    /// `None` on a cold start that didn't run the query (no cursor,
    /// nothing to diff against).
    pub scan_elapsed: Option<Duration>,
}

#[derive(Clone, Default)]
pub struct ParsedSignal {
    pub recipients: HashMap<String, ParsedRecipient>,
    /// Recipients whose stored frame names an ACI this build could not
    /// read, by recipient id, with what it looked like. The recipient is
    /// kept without it; the processor reports each.
    pub aci_unreadable: Vec<(String, String)>,
    /// Chats indexed by `chat_id` for lookup from `DocBucket`. The
    /// chats themselves carry no items — each item ends up in the
    /// matching bucket in `docs`.
    pub chats: HashMap<String, ParsedChat>,
    /// One bucket per `(chat_id, period_key)` pair that needs
    /// re-rendering. A chat survives Phase 1 iff `dolt_diff_*`
    /// reported any row of it as added/modified/removed since the
    /// last render cursor — every period of that chat is then
    /// loaded in Phase 2. Buckets whose chat didn't change are
    /// entirely absent from `docs`. Ordered by chat_id then
    /// period_key so the rendered tree is deterministic.
    pub docs: Vec<DocBucket>,
    /// Count of chats `dolt_diff` said were unchanged.
    pub docs_skipped: usize,
    /// Scan diagnostics propagated up to render so it can write the
    /// cursor + log elapsed_ms.
    pub scan: ScanResult,
    /// Every raw row each loaded chat reads, by `chat_id` — its `chats`
    /// row, its items and attachment edges from the load, its
    /// recipients as render looks them up. One per chat, not per
    /// period: the bucket the driver sweeps is the chat.
    pub inputs: HashMap<String, Inputs>,
}

#[derive(Debug, Clone)]
pub struct ParsedRecipient {
    pub id: String,
    /// `+<e164>` where the backup has the number; otherwise the ACI or
    /// PNI as bare hex, which does not say which. A handle comes from
    /// the number, else from [`ParsedRecipient::aci`].
    pub identifier: Option<String>,
    pub display_name: Option<String>,
    /// The account's id, as a UUID, read from the recipient frame
    /// itself; `None` for a group, the account, or a contact the backup
    /// knows by PNI alone.
    pub aci: Option<String>,
}

impl ParsedRecipient {
    /// The name the backup shows, else the number, else the account id
    /// as Signal spells it: `identifier` holds an ACI or PNI as bare hex,
    /// which reads as nothing.
    pub fn display(&self) -> String {
        self.display_name
            .clone()
            .or_else(|| self.identifier.clone().filter(|i| i.starts_with('+')))
            .or_else(|| self.aci.clone())
            .or_else(|| self.identifier.clone())
            .unwrap_or_else(|| format!("recipient_{}", self.id))
    }
}

#[derive(Debug, Clone)]
pub struct ParsedChat {
    pub id: String,
    pub recipient_id: String,
}

/// One rendered-markdown bucket: a slice of a chat covering a single
/// period key (`2024-03`, `2024-03-15`, `2024`, or `all`).
#[derive(Debug, Clone, Default)]
pub struct DocBucket {
    pub chat_id: String,
    pub period_key: String,
    pub items: Vec<ParsedChatItem>,
    /// This bucket's attachment bytes, loaded in bulk by [`parse`]
    /// from `chat_item_attachments` + CAS in two SQL queries. Render
    /// walks it synchronously via [`BlobBundle::markdown_link`] and
    /// [`BlobBundle::materialize_to_dir`].
    pub blobs: BlobBundle,
}

#[derive(Debug, Clone)]
pub struct ParsedChatItem {
    /// `chat_items.id`, as read — the key the diff names and the
    /// attachment edges hang off.
    pub item_pk: String,
    pub author_id: String,
    pub date_sent: i64,
    pub text: Option<String>,
    /// True when ChatItem.directionalDetails was `outgoing`. Drives
    /// "me" attribution in the rendered markdown.
    pub outgoing: bool,
    /// An incoming message the account has not read
    /// (`IncomingMessageDetails.read` false). Never set on an outgoing one.
    pub unread: bool,
    /// Attachments on this item, ordered by their position in the
    /// `StandardMessage.attachments` repeated field (matches the
    /// `slot` we stored at download time).
    pub attachments: Vec<ParsedAttachment>,
    /// The ACI of each person the text mentions, in the order of the
    /// `U+FFFC` placeholders that stand for them in `text`; `None` for
    /// one whose ACI this build could not read, so the rest keep their
    /// places.
    pub mentions: Vec<Option<String>>,
}

#[derive(Debug, Clone)]
pub struct ParsedAttachment {
    /// `local_media_name(plaintext_hash, local_key)` — the same key
    /// download used when calling `db.store_blob`.
    pub ref_id: String,
    pub content_type: Option<String>,
    pub file_name: Option<String>,
    pub is_image: bool,
}

pub fn parse_raw_dir(input: &Path) -> Result<ParsedSignal> {
    parse(input, Period::Month, "signal", RawRange::cold())
}

pub fn parse(
    input: &Path,
    period: Period,
    source_id: &str,
    range: RawRange<'_>,
) -> Result<ParsedSignal> {
    let db_path = datalib_etl::doltlite_raw::db_path_for(input);
    if !db_path.is_file() {
        return Ok(ParsedSignal::default());
    }
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current()
            .block_on(async move { parse_async(&db_path, period, source_id, range).await })
    })
}

async fn parse_async(
    db_path: &Path,
    period: Period,
    source_id: &str,
    range: RawRange<'_>,
) -> Result<ParsedSignal> {
    // Opened at the driver's commit, else HEAD. No commit means nothing
    // has been committed here to render: emptiness, not a reason to read
    // the working set.
    let Some(reader) = datalib_etl::doltlite_raw::open_reader(db_path, range.pin)
        .await
        .with_context(|| format!("open raw doltlite for render at {}", db_path.display()))?
    else {
        return Ok(ParsedSignal::default());
    };
    let pool = reader.pool().clone();
    let pin = reader.pin().clone();

    // Sibling CAS file holds attachment bytes.
    let cas_pool = blob_cas::open_cas_for_render(db_path)
        .await
        .with_context(|| format!("open the blob store beside {}", db_path.display()))?;

    let (recipients, aci_unreadable) = load_recipients(&pool).await?;
    let chats = load_chats(&pool).await?;

    // ── Phase 1: which chats changed since the cursor? ────────────
    let scan = scan_diff(&pool, range, &pin, source_id, &chats).await?;

    // Decide the load set.
    let (to_load_chats, docs_skipped) = match &scan.render {
        None => {
            let ids = load_all_chat_ids(&pool).await?;
            (ids, 0usize)
        }
        Some(changed) => {
            let total = chats.len();
            let to_load: HashSet<String> = changed
                .iter()
                .filter(|cid| chats.contains_key(*cid))
                .cloned()
                .collect();
            let skipped = total.saturating_sub(to_load.len());
            (to_load, skipped)
        }
    };

    // ── Phase 2: targeted load for to_load_chats ──────────────────
    let mut docs = if to_load_chats.is_empty() {
        Vec::new()
    } else {
        load_buckets(&pool, period, &to_load_chats).await?
    };

    // Per-bucket BlobBundle: each bucket gets its own bag of
    // attachment bytes, all of them loaded together. Render walks them
    // synchronously.
    let refs = docs.iter().enumerate().map(|(i, bucket)| {
        let refs = bucket
            .items
            .iter()
            .flat_map(|item| item.attachments.iter().map(|att| att.ref_id.as_str()));
        (i, refs)
    });
    let loaded =
        BlobBundle::load_many(&pool, cas_pool.as_ref(), ATTACHMENTS_PROJECTION_SQL, refs).await;
    if let Some(cas) = cas_pool {
        cas.close().await;
    }
    let mut blobs = loaded?;
    for (i, bucket) in docs.iter_mut().enumerate() {
        if let Some(b) = blobs.remove(&i) {
            bucket.blobs = b;
        }
    }

    let mut inputs: HashMap<String, Inputs> = HashMap::new();
    for bucket in &docs {
        let read = inputs.entry(bucket.chat_id.clone()).or_default();
        read.read("chats", &bucket.chat_id);
        for item in &bucket.items {
            read.read("chat_items", &item.item_pk);
            for att in &item.attachments {
                read.read(
                    "chat_item_attachments",
                    &ChatItemAttachmentRow::pk_recipe(&item.item_pk, &att.ref_id),
                );
            }
        }
    }

    Ok(ParsedSignal {
        recipients,
        aci_unreadable,
        chats,
        docs,
        docs_skipped,
        scan,
        inputs,
    })
}

/// Every recipient, and those whose ACI would not read.
type Recipients = (HashMap<String, ParsedRecipient>, Vec<(String, String)>);

async fn load_recipients(pool: &sqlx::SqlitePool) -> Result<Recipients> {
    let mut recipients: HashMap<String, ParsedRecipient> = HashMap::new();
    let mut unreadable: Vec<(String, String)> = Vec::new();
    let rrows = sqlx::query(
        "SELECT id, identifier, display_name, json(payload) AS payload FROM recipients",
    )
    .fetch_all(pool)
    .await
    .context("read recipients")?;
    for r in &rrows {
        let id: String = r.try_get("id")?;
        let identifier: Option<String> = r.try_get("identifier")?;
        let display_name: Option<String> = r.try_get("display_name")?;
        let payload: String = r.try_get("payload")?;
        let aci = aci_of(&payload).unwrap_or_else(|sample| {
            unreadable.push((id.clone(), sample));
            None
        });
        recipients.insert(
            id.clone(),
            ParsedRecipient {
                id,
                identifier,
                display_name,
                aci,
            },
        );
    }
    unreadable.sort();
    Ok((recipients, unreadable))
}

/// The ACI in a stored recipient frame, as a dashed lowercase UUID;
/// `Ok(None)` for a frame that has none (a group, the account, a contact
/// known by PNI alone). The frame is the proto as JSON (`WirePayload`,
/// read back through `json(payload)`), and only the one path is read,
/// so a field the proto gains or loses elsewhere costs nothing.
/// `Err` holds what an unreadable frame or ACI looked like.
fn aci_of(payload: &str) -> Result<Option<String>, String> {
    let frame: serde_json::Value =
        serde_json::from_str(payload).map_err(|e| format!("not JSON: {e}"))?;
    let aci = match frame.pointer("/destination/Contact/aci") {
        None | Some(serde_json::Value::Null) => return Ok(None),
        Some(aci) => aci,
    };
    let bytes: Option<Vec<u8>> = aci.as_array().and_then(|a| {
        a.iter()
            .map(|b| b.as_u64().and_then(|b| u8::try_from(b).ok()))
            .collect()
    });
    match bytes.as_deref().map(uuid_of) {
        Some(Some(uuid)) => Ok(Some(uuid)),
        _ => Err(aci.to_string()),
    }
}

fn uuid_of(bytes: &[u8]) -> Option<String> {
    if bytes.len() != 16 {
        return None;
    }
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    Some(format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    ))
}

async fn load_chats(pool: &sqlx::SqlitePool) -> Result<HashMap<String, ParsedChat>> {
    let crows = sqlx::query("SELECT id, recipient_id FROM chats ORDER BY id")
        .fetch_all(pool)
        .await
        .context("read chats")?;
    let mut chats: HashMap<String, ParsedChat> = HashMap::new();
    for r in &crows {
        let id: String = r.try_get("id")?;
        let recipient_id: String = r.try_get("recipient_id")?;
        chats.insert(
            id.clone(),
            ParsedChat {
                id: id.clone(),
                recipient_id,
            },
        );
    }
    Ok(chats)
}

async fn load_all_chat_ids(pool: &sqlx::SqlitePool) -> Result<HashSet<String>> {
    let rows = sqlx::query("SELECT DISTINCT chat_id FROM chat_items")
        .fetch_all(pool)
        .await
        .context("load all chat_ids")?;
    let mut out: HashSet<String> = HashSet::with_capacity(rows.len());
    for r in &rows {
        out.insert(r.try_get::<String, _>("chat_id")?);
    }
    Ok(out)
}

async fn scan_diff(
    pool: &SqlitePool,
    range: RawRange<'_>,
    pin: &datalib_etl::pin::Pin,
    source_id: &str,
    chats: &HashMap<String, ParsedChat>,
) -> Result<ScanResult> {
    let scan = datalib_etl::doltlite_raw::scan_buckets(
        pool,
        range.cursor,
        pin,
        &datalib_etl::doltlite_raw::DiffScanSpec {
            // A recipient change reaches a chat through the inputs it
            // declared; nothing fans out.
            global_fanout_tables: &[],
            bucket_query: "
                SELECT DISTINCT chat_id FROM (
                    SELECT coalesce(to_id, from_id) AS chat_id
                      FROM dolt_diff_chats
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT coalesce(to_chat_id, from_chat_id)
                      FROM dolt_diff_chat_items
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    -- Attachment changes propagate to their owning chat by
                    -- joining the diff vtab back to the live `chat_items`
                    -- table (which is at HEAD, the same ref the
                    -- surrounding diff queries are projecting to).
                    SELECT chat_items.chat_id
                      FROM dolt_diff_chat_item_attachments ca
                      JOIN chat_items
                        ON chat_items.id = coalesce(ca.to_chat_item_id, ca.from_chat_item_id)
                     WHERE ca.from_ref = ?1 AND ca.to_ref = ?2
                       AND ca.diff_type != 'unchanged'
                )
                WHERE chat_id IS NOT NULL
            ",
        },
    )
    .await?;
    // The driver names buckets by the chat uuid; the load wants chat ids.
    let by_uuid: HashMap<String, &str> = chats
        .keys()
        .map(|id| (super::ids::chat(source_id, id).uuid, id.as_str()))
        .collect();
    let narrowed = range.narrow_by(scan.render.as_ref(), |key| {
        by_uuid.get(key).map(|id| id.to_string())
    });
    Ok(ScanResult {
        render: narrowed.render,
        gone: narrowed.gone,
        new_head: scan.new_head,
        scan_elapsed: scan.scan_elapsed,
    })
}

/// Period::All bucket key — kept in one place so the SQL and Rust
/// paths agree.
const PERIOD_ALL_BUCKET_KEY: &str = "all";

/// Build the SQL fragment that derives the bucket key from
/// `date_sent` for a given [`Period`]. For non-`All` periods this
/// is a `strftime` over `date_sent / 1000` (sqlite expects
/// unix-seconds). For `Period::All` it's a literal string so every
/// chat_item lands in one bucket.
fn period_key_sql(period: Period) -> String {
    if matches!(period, Period::All) {
        format!("'{PERIOD_ALL_BUCKET_KEY}'")
    } else {
        format!(
            "strftime('{fmt}', date_sent / 1000, 'unixepoch')",
            fmt = period.strftime_fmt(),
        )
    }
}

async fn load_buckets(
    pool: &sqlx::SqlitePool,
    period: Period,
    chat_ids: &HashSet<String>,
) -> Result<Vec<DocBucket>> {
    if chat_ids.is_empty() {
        return Ok(Vec::new());
    }
    let period_key_expr = period_key_sql(period);
    let sql = format!(
        "SELECT id,
                chat_id,
                author_id,
                date_sent,
                {period_key_expr} AS period_key,
                json(payload) AS payload
           FROM chat_items
          WHERE chat_id IN (SELECT value FROM json_each(?))
          ORDER BY chat_id, period_key, date_sent"
    );
    // Audited: `period_key_expr` comes from `period_key_sql(period)` over the
    // `Period` enum; the chat ids are bound as one JSON array.
    let irows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(serde_json::to_string(chat_ids)?)
        .fetch_all(pool)
        .await
        .context("read chat_items")?;

    let mut bucket_idx: HashMap<(String, String), usize> = HashMap::new();
    let mut docs: Vec<DocBucket> = Vec::new();
    for r in &irows {
        let chat_id: String = r.try_get("chat_id")?;
        let author_id: String = r.try_get("author_id")?;
        let date_sent: i64 = r.try_get("date_sent")?;
        let period_key: String = r.try_get("period_key")?;
        let payload: String = r.try_get("payload")?;
        let key = (chat_id.clone(), period_key.clone());
        let idx = *bucket_idx.entry(key).or_insert_with(|| {
            docs.push(DocBucket {
                chat_id: chat_id.clone(),
                period_key: period_key.clone(),
                items: Vec::new(),
                blobs: BlobBundle::default(),
            });
            docs.len() - 1
        });
        let item_pk: String = r.try_get("id")?;
        let Decoded {
            text,
            outgoing,
            unread,
            attachments,
            mentions,
        } = decode_chat_item(&payload);
        docs[idx].items.push(ParsedChatItem {
            item_pk,
            author_id,
            date_sent,
            text,
            outgoing,
            unread,
            attachments,
            mentions,
        });
    }
    Ok(docs)
}

/// What render takes from one stored chat item.
#[derive(Debug, Default)]
struct Decoded {
    text: Option<String>,
    outgoing: bool,
    unread: bool,
    attachments: Vec<ParsedAttachment>,
    mentions: Vec<Option<String>>,
}

/// Parse a `chat_items.payload` JSON string (a `Frame::ChatItem`
/// serialized via serde). Returns empty defaults for non-StandardMessage
/// chat items so the renderer can skip them cleanly without panicking.
fn decode_chat_item(payload: &str) -> Decoded {
    let ci: backup::ChatItem = match serde_json::from_str(payload) {
        Ok(c) => c,
        Err(_) => return Decoded::default(),
    };
    use backup::chat_item::DirectionalDetails;
    let outgoing = matches!(
        ci.directional_details,
        Some(DirectionalDetails::Outgoing(_))
    );
    let unread =
        matches!(&ci.directional_details, Some(DirectionalDetails::Incoming(d)) if !d.read);
    match ci.item {
        Some(backup::chat_item::Item::StandardMessage(sm)) => {
            let mentions = sm.text.as_ref().map(mentioned_acis).unwrap_or_default();
            let text = sm.text.and_then(|t| {
                if t.body.is_empty() {
                    None
                } else {
                    Some(t.body)
                }
            });
            let attachments = sm
                .attachments
                .iter()
                .filter_map(attachment_from_message)
                .collect();
            Decoded {
                text,
                outgoing,
                unread,
                attachments,
                mentions,
            }
        }
        _ => Decoded {
            outgoing,
            unread,
            ..Decoded::default()
        },
    }
}

/// The ACIs a message's text mentions, in the order they appear: each
/// is a `mentionAci` body range over the `U+FFFC` that stands for it.
fn mentioned_acis(text: &backup::Text) -> Vec<Option<String>> {
    use backup::body_range::AssociatedValue;
    let mut ranges: Vec<&backup::BodyRange> = text
        .body_ranges
        .iter()
        .filter(|r| matches!(r.associated_value, Some(AssociatedValue::MentionAci(_))))
        .collect();
    ranges.sort_by_key(|r| r.start);
    ranges
        .into_iter()
        .map(|r| match &r.associated_value {
            Some(AssociatedValue::MentionAci(bytes)) => uuid_of(bytes),
            _ => None,
        })
        .collect()
}

fn attachment_from_message(att: &backup::MessageAttachment) -> Option<ParsedAttachment> {
    let ptr = att.pointer.as_ref()?;
    let li = ptr.locator_info.as_ref()?;
    let local_key = li.local_key.as_deref()?;
    if local_key.len() != 64 {
        return None;
    }
    let plaintext_hash = match li.integrity_check.as_ref()? {
        backup::file_pointer::locator_info::IntegrityCheck::PlaintextHash(h) if !h.is_empty() => {
            h.clone()
        }
        _ => return None,
    };
    let mut lk = [0u8; 64];
    lk.copy_from_slice(local_key);
    let ref_id = datalib_signal_backup::local_media_name(&plaintext_hash, &lk);
    let content_type = ptr.content_type.clone();
    let is_image = content_type
        .as_deref()
        .map(|ct| ct.starts_with("image/"))
        .unwrap_or(false);
    Some(ParsedAttachment {
        ref_id,
        content_type,
        file_name: ptr.file_name.clone(),
        is_image,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use backup::chat_item::{DirectionalDetails, IncomingMessageDetails, OutgoingMessageDetails};

    fn stored(directional: DirectionalDetails) -> String {
        serde_json::to_string(&backup::ChatItem {
            directional_details: Some(directional),
            item: Some(backup::chat_item::Item::StandardMessage(
                backup::StandardMessage {
                    text: Some(backup::Text {
                        body: "Hailing frequencies open, sir.".into(),
                        body_ranges: vec![],
                    }),
                    ..Default::default()
                },
            )),
            ..Default::default()
        })
        .unwrap()
    }

    /// Unread is an incoming message the account has not read, read off
    /// the JSON the ingest stores; an outgoing one never is.
    #[test]
    fn only_an_unread_incoming_message_is_unread() {
        let incoming = |read| {
            DirectionalDetails::Incoming(IncomingMessageDetails {
                read,
                ..Default::default()
            })
        };
        assert!(decode_chat_item(&stored(incoming(false))).unread);
        assert!(!decode_chat_item(&stored(incoming(true))).unread);
        let outgoing = decode_chat_item(&stored(DirectionalDetails::Outgoing(
            OutgoingMessageDetails::default(),
        )));
        assert!(outgoing.outgoing && !outgoing.unread);
    }

    /// The mentions come in the order of their placeholders whatever the
    /// order of the ranges, a style range is none, and an ACI that is not
    /// one keeps its place as `None`.
    #[test]
    fn a_messages_mentions_are_its_mention_ranges_in_text_order() {
        use backup::body_range::AssociatedValue;
        let range = |start, value| backup::BodyRange {
            start,
            length: 1,
            associated_value: Some(value),
        };
        let aci = |last: u8| {
            let mut b = vec![0u8; 16];
            b[15] = last;
            b
        };
        let payload = serde_json::to_string(&backup::ChatItem {
            item: Some(backup::chat_item::Item::StandardMessage(
                backup::StandardMessage {
                    text: Some(backup::Text {
                        body: "\u{FFFC}, \u{FFFC} and \u{FFFC}".into(),
                        body_ranges: vec![
                            range(6, AssociatedValue::MentionAci(vec![1, 2, 3])),
                            range(0, AssociatedValue::Style(1)),
                            range(10, AssociatedValue::MentionAci(aci(0xb))),
                            range(0, AssociatedValue::MentionAci(aci(0xa))),
                        ],
                    }),
                    ..Default::default()
                },
            )),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            decode_chat_item(&payload).mentions,
            [
                Some("00000000-0000-0000-0000-00000000000a".to_string()),
                None,
                Some("00000000-0000-0000-0000-00000000000b".to_string()),
            ]
        );
    }
}

#[cfg(test)]
mod load_tests {
    use super::*;
    use datalib_etl::bulk::SQLITE_MAX_VARIABLES;
    use datalib_etl::doltlite_raw::WirePayloadRow;
    use datalib_etl_signal::ingest::schema_raw::ChatItemRow;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    /// More chats to render than SQLite binds in one statement (#1156):
    /// every chat still comes back as its own bucket.
    #[tokio::test]
    async fn loads_more_chats_than_sqlite_binds_in_one_statement() {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(SqliteConnectOptions::from_str("sqlite::memory:").unwrap())
            .await
            .unwrap();
        sqlx::query(sqlx::AssertSqlSafe(ChatItemRow::ddl()))
            .execute(&pool)
            .await
            .unwrap();
        let n = SQLITE_MAX_VARIABLES + 1;
        sqlx::query(
            "WITH RECURSIVE s(value) AS (SELECT 1 UNION ALL SELECT value + 1 FROM s WHERE value < ?1)
             INSERT INTO chat_items (id, chat_id, author_id, date_sent, payload)
             SELECT 'i' || value, 'c' || value, 'worf', value, '{}' FROM s",
        )
        .bind(n as i64)
        .execute(&pool)
        .await
        .unwrap();
        let chats: HashSet<String> = (1..=n).map(|i| format!("c{i}")).collect();

        let docs = load_buckets(&pool, Period::All, &chats).await.unwrap();

        assert_eq!(docs.len(), n);
        assert!(docs.iter().all(|d| d.items.len() == 1));
    }
}

#[cfg(test)]
mod recipient_tests {
    use super::{aci_of, ParsedRecipient};
    use datalib_signal_backup::backup::{self, recipient::Destination};

    fn frame(destination: Option<Destination>) -> String {
        serde_json::to_string(&backup::Recipient { id: 7, destination }).unwrap()
    }

    /// Only the ACI's path is read: a frame written by an older proto,
    /// lacking a field this build's requires, or carrying one it does not
    /// know, still gives its ACI; an ACI that is not one says what it
    /// looked like instead of vanishing. Decoding the whole `Recipient`
    /// dropped every ACI the day the proto gained a required field.
    #[test]
    fn the_aci_is_read_alone_and_a_bad_one_is_said() {
        let aci: Vec<u8> = (0..16).collect();
        let mut v: serde_json::Value =
            serde_json::from_str(&frame(Some(Destination::Contact(backup::Contact {
                aci: Some(aci),
                ..Default::default()
            }))))
            .unwrap();
        v["destination"]["Contact"]["field_added_upstream"] = serde_json::json!(true);
        v["destination"]["Contact"]
            .as_object_mut()
            .unwrap()
            .remove("blocked")
            .expect("the frame carries `blocked`");
        v.as_object_mut().unwrap().remove("id").expect("and `id`");
        assert_eq!(
            aci_of(&v.to_string()),
            Ok(Some("00010203-0405-0607-0809-0a0b0c0d0e0f".to_string()))
        );
        v["destination"]["Contact"]["aci"] = serde_json::json!([1, 2, 3]);
        assert_eq!(aci_of(&v.to_string()), Err("[1,2,3]".to_string()));
        v["destination"]["Contact"]["aci"] = serde_json::json!("not bytes");
        assert!(aci_of(&v.to_string()).is_err());
        assert!(aci_of("not json").is_err());
    }

    /// A recipient the backup knows by ACI and nothing else reads as the
    /// ACI Signal spells, not the bare hex the download stored.
    #[test]
    fn a_nameless_recipient_reads_as_its_number_else_its_aci() {
        let r = |identifier: &str, aci: Option<&str>| ParsedRecipient {
            id: "8".into(),
            identifier: Some(identifier.into()),
            display_name: None,
            aci: aci.map(String::from),
        };
        let dashed = "0195683a-d140-87f9-bdf6-234da6d6880f";
        assert_eq!(
            r("0195683ad14087f9bdf6234da6d6880f", Some(dashed)).display(),
            dashed
        );
        assert_eq!(r("+17015550101", Some(dashed)).display(), "+17015550101");
        assert_eq!(r("abcdef", None).display(), "abcdef");
    }

    /// The ACI is read from the frame the download stored whole, so a
    /// recipient with no number still names a person.
    #[test]
    fn the_aci_is_read_from_the_stored_frame() {
        let aci: Vec<u8> = (0..16).map(|i| 0x10 * i as u8 + i as u8).collect();
        let contact = |aci: Option<Vec<u8>>, pni: Option<Vec<u8>>| {
            frame(Some(Destination::Contact(backup::Contact {
                aci,
                pni,
                ..Default::default()
            })))
        };
        assert_eq!(
            aci_of(&contact(Some(aci.clone()), None)),
            Ok(Some("00112233-4455-6677-8899-aabbccddeeff".to_string()))
        );
        assert_eq!(
            aci_of(&contact(None, Some(aci.clone()))),
            Ok(None),
            "a PNI is no ACI"
        );
        assert!(
            aci_of(&contact(Some(vec![1, 2, 3]), None)).is_err(),
            "not a UUID, and said so"
        );
        assert_eq!(
            aci_of(&frame(Some(Destination::Self_(backup::Self_::default())))),
            Ok(None)
        );
        assert_eq!(aci_of(&frame(None)), Ok(None));
    }
}
