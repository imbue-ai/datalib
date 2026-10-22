//! Read the mirrored Beeper Texts tables into rooms and `(room, period)`
//! buckets.
//!
//! The raw store is `index.db` table for table (`../beeper/INGEST.md`): a
//! room is a `threads` row, a message an `mx_room_messages` row, a
//! reaction either a `REACTION` message or an `mx_reactions` row, and a
//! cached attachment a `beeper_media_files` row. Their Matrix ids are the
//! identity. Everything Beeper keeps in its JSON columns is read here, and
//! who each person is: their handle and avatar (`Person`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use datalib_contact_schema::ContactHandle;
use datalib_etl::doltlite_raw;
use datalib_etl_render::inputs::{changed_rows, Inputs, RawRange};
use datalib_handle::Handle;
use datalib_schema::problems::{Problem, Reason};
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use super::Period;

/// The raw tables whose rows each name their room: the room's own row,
/// its people, its messages and its reactions.
const FORWARD_TABLES: &[&str] = &[
    "threads",
    "participants",
    "mx_room_messages",
    "mx_reactions",
];

const MEDIA_FILES: &str = "beeper_media_files";

/// A person's phone numbers and addresses; its key leads with no room,
/// so a change reaches the rooms its person is in.
const IDENTIFIERS: &str = "participant_identifiers";

#[derive(Debug, Clone)]
pub struct Room {
    pub room_uuid: String,
    pub network: String,
    pub native_room_id: String,
    pub external_room_id: Option<String>,
    pub external_workspace_id: Option<String>,
    pub account_id: Option<String>,
    pub title: Option<String>,
    pub is_dm: bool,
    /// Where the room opens on its own network, where that is known.
    pub source_url: Option<String>,
}

/// Someone in a room, as the room's `participants` row and Beeper's
/// identifiers for them say.
#[derive(Debug, Clone)]
pub struct Person {
    pub mxid: String,
    /// The name Beeper shows first.
    pub names: Vec<String>,
    /// The one a document names them by.
    pub handle: Option<Handle>,
    /// Every way the source knows to reach them, `handle` among them.
    pub handles: Vec<ContactHandle>,
    pub is_self: bool,
    /// blake3 of their avatar in the blob CAS, where the app had cached it.
    pub avatar: Option<String>,
}

impl Person {
    pub fn label(&self) -> String {
        self.names
            .first()
            .cloned()
            .unwrap_or_else(|| self.mxid.clone())
    }
}

/// One rendered-event entry. Both top-level messages and reactions
/// flow through this same shape; renderers branch on
/// [`Event::is_reaction`].
#[derive(Debug, Clone)]
pub struct Event {
    pub event_uuid: String,
    pub native_event_id: String,
    pub event_type: String,
    pub timestamp_ms: i64,
    pub sender_label: Option<String>,
    pub sender_handle: Option<Handle>,
    pub text_content: Option<String>,
    pub reply_to_native_event_id: Option<String>,
    pub reaction_emoji: Option<String>,
    pub reaction_target_native_event_id: Option<String>,
    pub blobs: Vec<Blob>,
    pub problems: Vec<Problem>,
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
    pub slot: String,
    pub content_type: Option<String>,
    pub byte_len: Option<i64>,
    /// blake3 hex of the CAS object holding the bytes, when copied.
    pub blake3: Option<String>,
}

/// One rendered document's worth of events: all messages whose
/// own period bucket matches the doc's `(room, period_key)`, plus
/// any reactions whose targets fall here regardless of when the
/// reaction itself landed.
#[derive(Debug, Clone)]
pub struct DocBucket {
    pub room_uuid: String,
    pub period_key: String,
    /// Messages in chronological order.
    pub messages: Vec<Event>,
    /// Reactions whose target falls in this bucket, keyed by target
    /// `native_event_id`.
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
    /// The people of each loaded room, by room uuid.
    pub people: HashMap<String, Vec<Person>>,
    /// Ordered by `(room_uuid, period_key)`.
    pub docs: Vec<DocBucket>,
    /// The raw rows each loaded room reads, by room uuid.
    pub inputs: HashMap<String, Inputs>,
    pub scan: ScanResult,
}

/// Open the raw store beside `input` and produce one [`DocBucket`] per
/// `(room, period)`, for the rooms of `networks` (every room when `None`).
pub fn parse(
    input: &Path,
    source_id: &str,
    period: Period,
    networks: Option<&[String]>,
    range: RawRange<'_>,
) -> Result<ParsedBeeper> {
    let db_path = doltlite_raw::db_path_for(input);
    if !db_path.is_file() {
        return Ok(ParsedBeeper::default());
    }
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            // No commit means nothing has been committed here to render:
            // emptiness, not a reason to read the working set.
            let Some(reader) = doltlite_raw::open_reader(&db_path, range.pin)
                .await
                .with_context(|| {
                    format!("open raw doltlite for render at {}", db_path.display())
                })?
            else {
                return Ok(ParsedBeeper::default());
            };
            // `index.db`'s tables arrive with the first mirror.
            let parsed = if doltlite_raw::table_exists(reader.pool(), "threads").await? {
                load(
                    reader.pool(),
                    &db_path,
                    source_id,
                    period,
                    networks,
                    range,
                    reader.pin(),
                )
                .await
            } else {
                Ok(ParsedBeeper::default())
            };
            // Closed, not dropped: the next open of this store is a
            // second connection until this one is actually gone.
            reader.close().await;
            parsed
        })
    })
}

async fn load(
    pool: &SqlitePool,
    db_path: &Path,
    source_id: &str,
    period: Period,
    networks: Option<&[String]>,
    range: RawRange<'_>,
    pin: &datalib_etl::pin::Pin,
) -> Result<ParsedBeeper> {
    let mut rooms: HashMap<String, Room> = HashMap::new();
    for r in sqlx::query("SELECT threadID, accountID, thread FROM threads")
        .fetch_all(pool)
        .await
        .context("read threads")?
    {
        let account: Option<String> = r.try_get("accountID")?;
        let Some(network) = account.as_deref().and_then(network_for_account) else {
            continue;
        };
        if networks.is_some_and(|wanted| !wanted.iter().any(|n| n == network)) {
            continue;
        }
        let thread_id: String = r.try_get("threadID")?;
        let thread: Value = json_column(r.try_get("thread")?);
        rooms.insert(
            thread_id.clone(),
            room(thread_id, network, account, &thread),
        );
    }

    // ── which rooms to load ────────────────────────────────────────
    // Every forward table's key holds its room id, the one part a Matrix
    // room id can be: it starts with `!`. An identifier's key is
    // `account|person|identifier`, and reaches every room its person is in.
    let rooms_of: HashMap<String, Vec<String>> =
        sqlx::query_as::<_, (String, String)>("SELECT id, room_id FROM participants")
            .fetch_all(pool)
            .await
            .context("read participants' rooms")?
            .into_iter()
            .fold(HashMap::new(), |mut acc, (person, room)| {
                acc.entry(person).or_default().push(room);
                acc
            });
    let mut forward_tables = FORWARD_TABLES.to_vec();
    forward_tables.push(IDENTIFIERS);
    let forward = changed_rows(pool, range, pin, &forward_tables)
        .await?
        .map(|changed| {
            let by_room = FORWARD_TABLES
                .iter()
                .filter_map(|t| changed.get(*t))
                .flatten()
                .filter_map(|key| key.split('|').find(|part| part.starts_with('!')))
                .map(String::from);
            let by_person = changed
                .get(IDENTIFIERS)
                .into_iter()
                .flatten()
                .filter_map(|key| key.split('|').nth(1))
                .flat_map(|person| rooms_of.get(person).into_iter().flatten().cloned());
            by_room
                .chain(by_person)
                .filter(|room| rooms.contains_key(room))
                .collect::<HashSet<String>>()
        });
    let room_by_bucket: HashMap<String, &str> = rooms
        .keys()
        .map(|room| (super::ids::room(source_id, room).uuid, room.as_str()))
        .collect();
    let narrowed = range.narrow_by(forward.as_ref(), |key| {
        room_by_bucket.get(key).map(|r| r.to_string())
    });
    let scan = ScanResult {
        render: narrowed.render,
        gone: narrowed.gone,
        new_head: Some(pin.commit().to_string()),
    };
    let loading: Vec<&str> = match &scan.render {
        None => rooms.keys().map(String::as_str).collect(),
        Some(set) => set.iter().map(String::as_str).collect(),
    };
    let loading_json = serde_json::to_string(&loading)?;

    let mut inputs: HashMap<String, Inputs> = HashMap::new();
    for room in &loading {
        inputs
            .entry(room.to_string())
            .or_default()
            .read("threads", room);
    }

    let media = MediaIndex::read(pool, db_path).await?;
    let people = people(pool, &loading_json, &rooms, &media, &mut inputs).await?;
    let person_of: HashMap<(&str, &str), &Person> = people
        .iter()
        .flat_map(|(room, ps)| {
            ps.iter()
                .map(move |p| ((room.as_str(), p.mxid.as_str()), p))
        })
        .collect();
    // A sender who is no participant of the room shows as their Matrix id.
    let sender_of = |room: &str, user: &Option<String>| -> (Option<String>, Option<Handle>) {
        let Some(user) = user else {
            return (None, None);
        };
        match person_of.get(&(room, user.as_str())) {
            Some(p) => (Some(p.label()), p.handle.clone()),
            None => (Some(user.clone()), None),
        }
    };

    // ── messages, and the reactions kept as messages ───────────────
    let mut events: Vec<(String, i64, Event)> = Vec::new();
    for r in sqlx::query(
        "SELECT roomID, eventID, senderContactID, timestamp, type, hsOrder, \
                inReplyToID, text_content, message \
         FROM mx_room_messages WHERE roomID IN (SELECT value FROM json_each(?))",
    )
    .bind(&loading_json)
    .fetch_all(pool)
    .await
    .context("read mx_room_messages")?
    {
        let room: String = r.try_get("roomID")?;
        let event_id: String = r.try_get("eventID")?;
        let message = json_column(r.try_get("message")?);
        let event_type: String = r
            .try_get::<Option<String>, _>("type")?
            .unwrap_or_else(|| "UNKNOWN".to_string());
        let (reaction_emoji, reaction_target) = if event_type == "REACTION" {
            (
                str_at(&message, "/action/reactionKey"),
                str_at(&message, "/extra/partialReactionContent/relatedEventID"),
            )
        } else {
            (None, None)
        };
        let text = r
            .try_get::<Option<String>, _>("text_content")?
            .or_else(|| str_at(&message, "/text"))
            .filter(|s| !s.is_empty());
        // A key is its columns in key order, joined by `|` (`changed_keys`).
        let read = inputs.entry(room.clone()).or_default();
        read.read("mx_room_messages", &format!("{room}|{event_id}"));
        let (blobs, problems) = attachments(&message, &media, read);
        let order: i64 = r.try_get::<Option<i64>, _>("hsOrder")?.unwrap_or(0);
        let (sender_label, sender_handle) = sender_of(&room, &r.try_get("senderContactID")?);
        events.push((
            room.clone(),
            order,
            Event {
                event_uuid: event_id.clone(),
                native_event_id: event_id,
                event_type,
                timestamp_ms: r.try_get("timestamp")?,
                sender_label,
                sender_handle,
                text_content: text,
                reply_to_native_event_id: r.try_get("inReplyToID")?,
                reaction_emoji,
                reaction_target_native_event_id: reaction_target,
                blobs,
                problems,
            },
        ));
    }

    // ── reactions kept in their own table ──────────────────────────
    for r in sqlx::query(
        "SELECT roomID, reactionID, eventID, senderID, description, timestamp \
         FROM mx_reactions WHERE roomID IN (SELECT value FROM json_each(?))",
    )
    .bind(&loading_json)
    .fetch_all(pool)
    .await
    .context("read mx_reactions")?
    {
        let room: String = r.try_get("roomID")?;
        let reaction_id: String = r.try_get("reactionID")?;
        inputs
            .entry(room.clone())
            .or_default()
            .read("mx_reactions", &format!("{room}|{reaction_id}"));
        let (sender_label, sender_handle) = sender_of(&room, &r.try_get("senderID")?);
        events.push((
            room.clone(),
            0,
            Event {
                event_uuid: reaction_id.clone(),
                native_event_id: reaction_id,
                event_type: "REACTION".to_string(),
                timestamp_ms: r.try_get("timestamp")?,
                sender_label,
                sender_handle,
                text_content: None,
                reply_to_native_event_id: None,
                reaction_emoji: r.try_get("description")?,
                reaction_target_native_event_id: r.try_get("eventID")?,
                blobs: Vec::new(),
                problems: Vec::new(),
            },
        ));
    }
    events.sort_by(|(ra, oa, a), (rb, ob, b)| {
        (ra, a.timestamp_ms, oa, &a.native_event_id).cmp(&(
            rb,
            b.timestamp_ms,
            ob,
            &b.native_event_id,
        ))
    });

    Ok(ParsedBeeper {
        docs: bucket(events.into_iter().map(|(room, _, e)| (room, e)), period),
        people,
        rooms,
        inputs,
        scan,
    })
}

/// Each loaded room's people, with the handle each is known by: a Slack
/// user by their Slack id, which the bridge spells into the Matrix id
/// (`@slackgo_<team>-<user>`, lowercased); the account's owner in a Slack
/// room by the Slack user the room was bridged for; anyone by a number or
/// address Beeper lists for them. Every read is declared on its room.
async fn people(
    pool: &SqlitePool,
    loading_json: &str,
    rooms: &HashMap<String, Room>,
    media: &MediaIndex,
    inputs: &mut HashMap<String, Inputs>,
) -> Result<HashMap<String, Vec<Person>>> {
    let mut reach: HashMap<String, Vec<ContactHandle>> = HashMap::new();
    if doltlite_raw::table_exists(pool, IDENTIFIERS).await? {
        for (person, identifier, kind) in sqlx::query_as::<_, (String, String, String)>(
            "SELECT participant_id, identifier, identifier_type FROM participant_identifiers \
             WHERE participant_id IN (SELECT id FROM participants \
                                      WHERE room_id IN (SELECT value FROM json_each(?)))",
        )
        .bind(loading_json)
        .fetch_all(pool)
        .await
        .context("read participant_identifiers")?
        {
            let handle = match kind.as_str() {
                "phone" => ContactHandle::phone(None, identifier),
                "email" => ContactHandle::email(None, identifier),
                _ => continue,
            };
            reach.entry(person).or_default().push(handle);
        }
    }

    let mut out: HashMap<String, Vec<Person>> = HashMap::new();
    for r in sqlx::query(
        "SELECT room_id, id, full_name, nickname, img_url, is_self FROM participants \
         WHERE room_id IN (SELECT value FROM json_each(?)) ORDER BY room_id, id",
    )
    .bind(loading_json)
    .fetch_all(pool)
    .await
    .context("read participants")?
    {
        let room_id: String = r.try_get("room_id")?;
        let mxid: String = r.try_get("id")?;
        let is_self = r.try_get::<Option<i64>, _>("is_self")?.unwrap_or(0) != 0;
        let read = inputs.entry(room_id.clone()).or_default();
        read.read("participants", &format!("{room_id}|{mxid}"));
        let room = rooms.get(&room_id);
        let own = slack_user(&mxid).or_else(|| {
            let room = room.filter(|r| is_self && r.network == "slack")?;
            let (team, user) = room.external_workspace_id.as_deref()?.split_once('-')?;
            Handle::slack(team, user)
        });
        let mut handles: Vec<ContactHandle> = own.iter().cloned().map(ContactHandle::of).collect();
        handles.extend(reach.get(&mxid).cloned().unwrap_or_default());
        let names: Vec<String> = [
            r.try_get::<Option<String>, _>("full_name")?,
            r.try_get::<Option<String>, _>("nickname")?,
        ]
        .into_iter()
        .flatten()
        .filter(|n| !n.trim().is_empty())
        .fold(Vec::new(), |mut acc, n| {
            if !acc.contains(&n) {
                acc.push(n);
            }
            acc
        });
        let files = read.lookup(MEDIA_FILES, &media.blake3_by_path);
        let avatar = r
            .try_get::<Option<String>, _>("img_url")?
            .as_deref()
            .and_then(media_path)
            .and_then(|path| files.get(&path).cloned());
        out.entry(room_id).or_default().push(Person {
            handle: handles.iter().find_map(|h| h.handle.clone()),
            mxid,
            names,
            handles,
            is_self,
            avatar,
        });
    }
    Ok(out)
}

/// The Slack user a bridged Slack account's Matrix id names. The bridge
/// lowercases Slack's ids, which are upper-case letters and digits.
fn slack_user(mxid: &str) -> Option<Handle> {
    let local = mxid.strip_prefix("@slackgo_")?.split(':').next()?;
    let (team, user) = local.split_once('-')?;
    Handle::slack(&team.to_ascii_uppercase(), &user.to_ascii_uppercase())
}

/// File each message under its `(room, period)`, and each reaction under
/// its target's period however late it arrived. A reaction whose target
/// is not in the store at all files under its own period.
fn bucket(events: impl Iterator<Item = (String, Event)>, period: Period) -> Vec<DocBucket> {
    let (reactions, messages): (Vec<_>, Vec<_>) = events.partition(|(_, e)| e.is_reaction());
    let mut docs: BTreeMap<(String, String), DocBucket> = BTreeMap::new();
    let mut target_period: HashMap<(String, String), String> = HashMap::new();
    for (room, m) in messages {
        let key = period.key_for_ms(m.timestamp_ms);
        target_period.insert((room.clone(), m.native_event_id.clone()), key.clone());
        doc(&mut docs, &room, key).messages.push(m);
    }
    for (room, r) in reactions {
        let target = r
            .reaction_target_native_event_id
            .clone()
            .unwrap_or_else(|| r.native_event_id.clone());
        let key = target_period
            .get(&(room.clone(), target.clone()))
            .cloned()
            .unwrap_or_else(|| period.key_for_ms(r.timestamp_ms));
        doc(&mut docs, &room, key)
            .reactions_by_target
            .entry(target)
            .or_default()
            .push(r);
    }
    docs.into_values().collect()
}

fn doc<'a>(
    docs: &'a mut BTreeMap<(String, String), DocBucket>,
    room: &str,
    period_key: String,
) -> &'a mut DocBucket {
    docs.entry((room.to_string(), period_key.clone()))
        .or_insert_with(|| DocBucket {
            room_uuid: room.to_string(),
            period_key,
            messages: Vec::new(),
            reactions_by_target: BTreeMap::new(),
        })
}

/// The cached files the store holds, by path under `media/`, with what
/// the blob store knows of each one's bytes.
struct MediaIndex {
    blake3_by_path: HashMap<String, String>,
    bytes_of: HashMap<String, (Option<String>, Option<i64>)>,
}

impl MediaIndex {
    async fn read(pool: &SqlitePool, db_path: &Path) -> Result<Self> {
        let blake3_by_path = sqlx::query_as::<_, (String, String)>(
            "SELECT rel_path, blake3 FROM beeper_media_files",
        )
        .fetch_all(pool)
        .await
        .context("read beeper_media_files")?
        .into_iter()
        .collect();
        let mut bytes_of = HashMap::new();
        if let Some(cas) = datalib_etl::blob_cas::open_cas_for_render(db_path)
            .await
            .with_context(|| format!("open the blob store beside {}", db_path.display()))?
        {
            let rows = sqlx::query_as::<_, (String, Option<String>, Option<i64>)>(
                "SELECT blake3, content_type, byte_len FROM cas_objects",
            )
            .fetch_all(cas.pool())
            .await
            .context("read cas_objects");
            cas.close().await;
            bytes_of = rows?
                .into_iter()
                .map(|(h, ct, len)| (h, (ct, len)))
                .collect();
        }
        Ok(Self {
            blake3_by_path,
            bytes_of,
        })
    }
}

/// A message's attachments, each with its bytes where the app had cached
/// the file and the download copied it. One this build cannot place is a
/// problem on the message.
fn attachments(message: &Value, media: &MediaIndex, read: &Inputs) -> (Vec<Blob>, Vec<Problem>) {
    let (mut blobs, mut problems) = (Vec::new(), Vec::new());
    let files = read.lookup(MEDIA_FILES, &media.blake3_by_path);
    let listed = message.pointer("/attachments").and_then(Value::as_array);
    for (i, att) in listed.into_iter().flatten().enumerate() {
        let slot = str_at(att, "/fileName").unwrap_or_else(|| format!("attachment_{i}"));
        let Some(id) = str_at(att, "/id") else {
            problems.push(Problem::field("attachments.id", Reason::NoIdentity, &slot));
            continue;
        };
        let blake3 = match media_path(&id) {
            Some(path) => files.get(&path).cloned(),
            None => {
                problems.push(Problem::field("attachments.id", Reason::UncoveredType, &id));
                None
            }
        };
        let (stored_type, byte_len) = blake3
            .as_ref()
            .and_then(|h| media.bytes_of.get(h).cloned())
            .unwrap_or_default();
        blobs.push(Blob {
            slot,
            content_type: stored_type.or_else(|| str_at(att, "/mimeType")),
            byte_len,
            blake3,
        });
    }
    (blobs, problems)
}

/// Where Beeper Texts caches the file an attachment's id names, under
/// `media/`. An on-device bridge's ids are `localmxc://<bridge>/…`, and
/// the app files those under `localhost<bridge>`.
fn media_path(id: &str) -> Option<String> {
    let (dir_prefix, rest) = if let Some(rest) = id.strip_prefix("mxc://") {
        ("", rest)
    } else {
        ("localhost", id.strip_prefix("localmxc://")?)
    };
    let (server, media_id) = rest.split_once('/')?;
    let media_id = media_id.split('?').next().unwrap_or(media_id);
    Some(format!("{dir_prefix}{server}/{media_id}"))
}

fn room(thread_id: String, network: &str, account: Option<String>, thread: &Value) -> Room {
    let thread_type = str_at(thread, "/type");
    let room_type = str_at(thread, "/extra/bridge/com.beeper.room_type").or(thread_type.clone());
    // Beeper stamps the network's own ids for the room under
    // `extra.bridge.channel`: Signal's conversation and account UUIDs,
    // Google Chat's `dm:…`, Slack's `<team>-<channel>` and `<team>-<user>`.
    let external_room_id = str_at(thread, "/extra/bridge/channel/id");
    Room {
        source_url: (network == "slack")
            .then(|| external_room_id.as_deref().and_then(slack_channel_url))
            .flatten(),
        is_dm: room_type.as_deref() == Some("dm") || thread_type.as_deref() == Some("single"),
        room_uuid: thread_id.clone(),
        network: network.to_string(),
        native_room_id: thread_id,
        external_room_id,
        external_workspace_id: str_at(thread, "/extra/bridge/channel/fi.mau.receiver"),
        account_id: str_at(thread, "/accountID").or(account),
        title: str_at(thread, "/title"),
    }
}

/// Slack's documented link that opens a channel in whichever client the
/// reader has, from the bridge's `<team>-<channel>` id.
fn slack_channel_url(team_channel: &str) -> Option<String> {
    let (team, channel) = team_channel.split_once('-')?;
    (!team.is_empty() && !channel.is_empty())
        .then(|| format!("https://slack.com/app_redirect?channel={channel}&team={team}"))
}

/// The canonical network an `index.db` account id belongs to: the id is
/// the bridge's name, alone or followed by `.` or `_` and the login.
fn network_for_account(account_id: &str) -> Option<&'static str> {
    const BRIDGES: &[(&str, &str)] = &[
        ("local-signal", "signal"),
        ("googlechat", "googlechat"),
        ("local-googlechat", "googlechat"),
        ("slackgo", "slack"),
        ("local-slack", "slack"),
        ("slack", "slack"),
        ("whatsapp", "whatsapp"),
        ("local-whatsapp", "whatsapp"),
        ("telegram", "telegram"),
        ("local-telegram", "telegram"),
        ("discordgo", "discord"),
        ("local-discord", "discord"),
        ("discord", "discord"),
        ("linkedin", "linkedin"),
        ("local-linkedin", "linkedin"),
        ("twitter", "twitter"),
        ("local-twitter", "twitter"),
        ("instagramgo", "instagram"),
        ("local-instagram", "instagram"),
        ("instagram", "instagram"),
        ("facebookgo", "facebook"),
        ("local-facebook", "facebook"),
        ("facebook", "facebook"),
        ("gmessages", "sms"),
        ("local-gmessages", "sms"),
    ];
    BRIDGES.iter().find_map(|(bridge, network)| {
        let rest = account_id.strip_prefix(bridge)?;
        (rest.is_empty() || rest.starts_with('.') || rest.starts_with('_')).then_some(*network)
    })
}

fn str_at(v: &Value, pointer: &str) -> Option<String> {
    v.pointer(pointer).and_then(Value::as_str).map(String::from)
}

fn json_column(text: Option<String>) -> Value {
    text.and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_account_id_names_its_network() {
        assert_eq!(network_for_account("local-signal_ba_xyz"), Some("signal"));
        assert_eq!(network_for_account("googlechat"), Some("googlechat"));
        assert_eq!(network_for_account("googlechat.foo"), Some("googlechat"));
        assert_eq!(network_for_account("slackgo.TXY12345-UAB"), Some("slack"));
        assert_eq!(network_for_account("googlechatlikethis"), None);
        assert_eq!(network_for_account("$space"), None);
        assert_eq!(network_for_account("hungryserv"), None);
    }

    #[test]
    fn an_attachment_id_names_its_cached_file() {
        assert_eq!(
            media_path("mxc://local.beeper.com/abc").as_deref(),
            Some("local.beeper.com/abc")
        );
        assert_eq!(
            media_path("localmxc://local-signal/AwAU").as_deref(),
            Some("localhostlocal-signal/AwAU")
        );
        assert_eq!(
            media_path("mxc://server/abc?encryptedFileInfoJSON=xyz").as_deref(),
            Some("server/abc")
        );
        assert_eq!(media_path("https://example.com/warp.png"), None);
    }

    #[test]
    fn a_bridged_slack_user_is_their_slack_handle() {
        assert_eq!(
            slack_user("@slackgo_t1701d-u0worf:beeper.local").map(|h| h.as_str().to_string()),
            Some("slack:T1701D/U0WORF".to_string())
        );
        assert_eq!(slack_user("@signal_abc:local-signal.localhost"), None);
        assert_eq!(slack_user("@picard:beeper.com"), None);
    }

    #[test]
    fn a_slack_room_links_to_its_channel() {
        assert_eq!(
            slack_channel_url("T1701D-C0BRIDGE").as_deref(),
            Some("https://slack.com/app_redirect?channel=C0BRIDGE&team=T1701D")
        );
        assert_eq!(slack_channel_url("C0BRIDGE"), None);
    }
}
