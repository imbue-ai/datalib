//! The raw store: one `(id, payload)` table per kind of Discord object,
//! the payload the object as the API sent it less what its sidecar
//! keeps. Ids are Discord's own, unique across the service, so a table's
//! key is the object's id; a message's is its channel's and its own.

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::doltlite_raw::{self as dr, Migration, WirePayload, WirePayloadRow};
use datalib_etl_macros::{CasEdgeRow, WirePayloadRow};
use serde_json::{Map, Value};

use super::snowflake;

pub const ACCOUNT: &str = "account";
pub const USERS: &str = "users";
pub const GUILDS: &str = "guilds";
pub const MEMBERS: &str = "members";
pub const ROLES: &str = "roles";
pub const CHANNELS: &str = "channels";
pub const MESSAGES: &str = "messages";
pub const ATTACHMENTS: &str = "discord_attachments";

/// Every table with a bookkeeping sidecar.
pub const DATA_TABLES: &[&str] = &[
    ACCOUNT,
    USERS,
    GUILDS,
    MEMBERS,
    ROLES,
    CHANNELS,
    MESSAGES,
    ATTACHMENTS,
];

/// Discord's channel types (its API reference, "Channel Types").
pub const DM: i64 = 1;
pub const GROUP_DM: i64 = 3;
pub const CATEGORY: i64 = 4;
pub const FORUM: i64 = 15;
pub const MEDIA: i64 = 16;
pub const THREADS: &[i64] = &[10, 11, 12];
/// Text, announcements, and the chat of a voice and a stage channel.
const TEXT: &[i64] = &[0, 2, 5, 13];
/// Text and announcement channels, forums and media channels.
const THREAD_PARENTS: &[i64] = &[0, 5, FORUM, MEDIA];

pub fn channel_type(channel: &Value) -> i64 {
    channel.get("type").and_then(Value::as_i64).unwrap_or(-1)
}

/// A server channel that holds messages itself.
pub fn is_text(channel: &Value) -> bool {
    TEXT.contains(&channel_type(channel))
}

/// A server channel that threads, or a forum's posts, are in.
pub fn is_thread_parent(channel: &Value) -> bool {
    THREAD_PARENTS.contains(&channel_type(channel))
}

pub fn is_thread(channel: &Value) -> bool {
    THREADS.contains(&channel_type(channel))
}

/// `account`: the signed-in account, one row.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "account")]
pub struct AccountRow {
    pub id_and_payload: WirePayload,
}

/// `users`: everyone a stored message or direct message names, as Discord
/// last showed them.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "users")]
pub struct UserRow {
    pub id_and_payload: WirePayload,
}

/// `guilds`: a server, which Discord's API calls a guild.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "guilds")]
pub struct GuildRow {
    pub id_and_payload: WirePayload,
}

/// `members`: the account in one server, keyed by the server.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "members")]
pub struct MemberRow {
    pub id_and_payload: WirePayload,
}

#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "roles")]
pub struct RoleRow {
    pub id_and_payload: WirePayload,
    pub guild_id: String,
}

/// `channels`: every conversation and every place one can be in: a
/// server's channels and categories, its threads, direct messages.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "channels")]
pub struct ChannelRow {
    pub id_and_payload: WirePayload,
    pub guild_id: Option<String>,
    /// A thread's channel, a channel's category.
    pub parent_id: Option<String>,
    pub kind: Option<i64>,
}

#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "messages")]
pub struct MessageRow {
    pub id_and_payload: WirePayload,
    pub channel_id: String,
}

/// `discord_attachments`: the edge from a message to each file it
/// carries; `blake3` null until the bytes are held.
#[derive(Debug, Clone, CasEdgeRow)]
#[cas_edge_row(table = "discord_attachments")]
pub struct DiscordAttachmentRow {
    pub id: String,
    pub message_key: String,
    pub attachment_id: String,
    pub blake3: Option<String>,
}

pub const MESSAGES_BY_CHANNEL_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS messages_by_channel ON messages(channel_id, id)";
pub const CHANNELS_BY_PARENT_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS channels_by_parent ON channels(parent_id)";
pub const ROLES_BY_GUILD_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS roles_by_guild ON roles(guild_id)";
/// The edges with no bytes, and no others: a page's are found by its
/// messages, and every one is a walk of what is owed alone.
pub const ATTACHMENTS_OWED_INDEX_DDL: &str = "CREATE INDEX IF NOT EXISTS discord_attachments_owed \
     ON discord_attachments(message_key) WHERE blake3 IS NULL";

/// `{channel}#{message}`, the message's id at a fixed width, so a
/// channel's messages are one range of the table in the order they were
/// sent.
pub fn message_key(channel_id: &str, message_id: u64) -> String {
    format!("{channel_id}#{}", snowflake::key(message_id))
}

/// The channel and the message's own id, as Discord writes it.
pub fn split_message_key(key: &str) -> Option<(&str, String)> {
    let (channel, id) = key.split_once('#')?;
    Some((channel, snowflake::from_key(id)?.to_string()))
}

/// The `coverage` scope of one conversation's messages, as message keys.
pub fn history_scope(channel_id: &str) -> String {
    format!("history:{channel_id}")
}

/// The `coverage` scope of one channel's thread listing, over each
/// thread's newest message.
pub fn threads_scope(channel_id: &str) -> String {
    format!("threads:{channel_id}")
}

// What an object gives up to its sidecar.

fn split_by(payload: &Value, paths: &[Vec<&str>]) -> (Value, Option<Value>) {
    let paths: Vec<&[&str]> = paths.iter().map(Vec::as_slice).collect();
    dr::split_volatile(payload, &paths)
}

/// The fields of a person that say who they are. The rest of a profile
/// (a banner, a server tag, a decoration) differs from one answer to the
/// next and says nothing a page shows.
const PERSON: &[&str] = &[
    "id",
    "username",
    "global_name",
    "discriminator",
    "avatar",
    "bot",
    "system",
];

/// What a message keeps of a person it names; the person is in `users`.
const NAMED: &[&str] = &["id", "bot"];

fn keys_but<'a>(object: &'a Value, keep: &[&str]) -> Vec<&'a str> {
    object
        .as_object()
        .into_iter()
        .flat_map(Map::keys)
        .map(String::as_str)
        .filter(|k| !keep.contains(k))
        .collect()
}

pub fn split_user(user: &Value) -> (Value, Option<Value>) {
    let paths: Vec<Vec<&str>> = keys_but(user, PERSON)
        .into_iter()
        .map(|k| vec![k])
        .collect();
    split_by(user, &paths)
}

/// The account's phone number and how it proves itself are no part of a
/// copy of its messages.
const ACCOUNT_NEVER_STORED: &[&str] = &["phone", "authenticator_types", "linked_users"];

pub fn split_account(account: &Value) -> (Value, Option<Value>) {
    let mut account = account.clone();
    if let Some(fields) = account.as_object_mut() {
        for key in ACCOUNT_NEVER_STORED {
            fields.remove(*key);
        }
    }
    let keep: Vec<&str> = PERSON.iter().copied().chain(["email", "locale"]).collect();
    let paths: Vec<Vec<&str>> = keys_but(&account, &keep)
        .into_iter()
        .map(|k| vec![k])
        .collect();
    split_by(&account, &paths)
}

/// Discord lists a server's features in another order each time.
pub fn split_guild(guild: &Value) -> (Value, Option<Value>) {
    let mut guild = guild.clone();
    sort_strings(&mut guild, "features");
    split_by(
        &guild,
        &[
            vec!["approximate_member_count"],
            vec!["approximate_presence_count"],
        ],
    )
}

pub fn split_member(member: &Value) -> (Value, Option<Value>) {
    let mut member = member.clone();
    sort_strings(&mut member, "roles");
    split_by(
        &member,
        &[
            vec!["user"],
            vec!["bio"],
            vec!["collectibles"],
            vec!["display_name_styles"],
            vec!["member_gaming_leaderboard_data"],
            vec!["unusual_dm_activity_until"],
        ],
    )
}

/// A channel's newest message and its counts move with every message
/// sent in it, and a thread is archived and revived as it is used; what
/// the channel is does not. A direct message's people are in `users`.
pub fn split_channel(channel: &Value) -> (Value, Option<Value>) {
    let mut channel = channel.clone();
    sort_by_id(&mut channel, "permission_overwrites");
    sort_by_id(&mut channel, "recipients");
    let mut paths: Vec<Vec<&str>> = [
        "last_message_id",
        "last_pin_timestamp",
        "message_count",
        "member_count",
        "total_message_sent",
        "member_ids_preview",
        "member",
        "owner",
        "recipient_flags",
        "newly_created",
        "position",
    ]
    .into_iter()
    .map(|k| vec![k])
    .collect();
    paths.push(vec!["thread_metadata", "archived"]);
    paths.push(vec!["thread_metadata", "archive_timestamp"]);
    let profiles = profile_fields(channel.get("recipients"));
    paths.extend(profiles.iter().map(|k| vec!["recipients", "*", k.as_str()]));
    split_by(&channel, &paths)
}

/// Every profile field any of `people` carries beyond who they are.
fn profile_fields(people: Option<&Value>) -> Vec<String> {
    let mut fields: Vec<String> = people
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|p| keys_but(p, NAMED))
        .map(String::from)
        .collect();
    fields.sort_unstable();
    fields.dedup();
    fields
}

/// A file's own description that moves while the file does not: its
/// links, signed anew each day, and what Discord works out of it after.
const FILE_VOLATILE: &[&str] = &[
    "url",
    "proxy_url",
    "content_scan_version",
    "placeholder",
    "placeholder_version",
];

/// A message keeps the ids of who it names, not their profiles, which
/// Discord fills in with each person's profile of the moment, and not its
/// copies of the message it answers and of the thread it started, which
/// are records of their own. A webhook's author is nobody's profile: its
/// name is the message's own.
pub fn split_message(message: &Value) -> (Value, Option<Value>) {
    let mut message = message.clone();
    sort_strings(&mut message, "mention_roles");
    sort_by_id(&mut message, "mentions");
    let mut paths: Vec<Vec<&str>> = vec![
        vec!["referenced_message"],
        vec!["thread"],
        vec!["resolved"],
        vec!["interaction", "user"],
        vec!["interaction_metadata", "user"],
    ];
    let author: Vec<String> = match is_webhook(&message) {
        true => Vec::new(),
        false => message
            .get("author")
            .map(|a| keys_but(a, NAMED))
            .into_iter()
            .flatten()
            .map(String::from)
            .collect(),
    };
    let mentions = profile_fields(message.get("mentions"));
    paths.extend(author.iter().map(|k| vec!["author", k.as_str()]));
    paths.extend(mentions.iter().map(|k| vec!["mentions", "*", k.as_str()]));
    for field in FILE_VOLATILE {
        paths.push(vec!["attachments", "*", field]);
        paths.push(vec![
            "message_snapshots",
            "*",
            "message",
            "attachments",
            "*",
            field,
        ]);
    }
    paths.push(vec!["embeds", "*", "content_scan_version"]);
    for part in ["thumbnail", "image", "video"] {
        for field in [
            "proxy_url",
            "placeholder",
            "placeholder_version",
            "content_type",
        ] {
            paths.push(vec!["embeds", "*", part, field]);
        }
    }
    split_by(&message, &paths)
}

/// Posted through a webhook: its author is a name the poster chose, whose
/// id is the webhook's. (An app answering a command also carries a
/// webhook id, its own, and is a user.)
pub fn is_webhook(message: &Value) -> bool {
    let webhook = message.get("webhook_id").and_then(Value::as_str);
    webhook.is_some() && webhook != message.get("application_id").and_then(Value::as_str)
}

fn sort_strings(object: &mut Value, key: &str) {
    if let Some(list) = object.get_mut(key).and_then(Value::as_array_mut) {
        list.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    }
}

fn sort_by_id(object: &mut Value, key: &str) {
    if let Some(list) = object.get_mut(key).and_then(Value::as_array_mut) {
        list.sort_by(|a, b| {
            let id = |v: &Value| v.get("id").and_then(Value::as_str).map(String::from);
            id(a).cmp(&id(b))
        });
    }
}

pub const LADDER: &[Migration] = &[];

pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        AccountRow::ddl(),
        UserRow::ddl(),
        GuildRow::ddl(),
        MemberRow::ddl(),
        RoleRow::ddl(),
        ROLES_BY_GUILD_INDEX_DDL.to_string(),
        ChannelRow::ddl(),
        CHANNELS_BY_PARENT_INDEX_DDL.to_string(),
        MessageRow::ddl(),
        MESSAGES_BY_CHANNEL_INDEX_DDL.to_string(),
        datalib_etl_web::coverage::DDL.to_string(),
    ];
    out.extend(DiscordAttachmentRow::all_ddl());
    out.push(ATTACHMENTS_OWED_INDEX_DDL.to_string());
    out.extend(DATA_TABLES.iter().map(|t| dr::bookkeeping_ddl_for(t)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn person(id: &str, banner: &str) -> Value {
        json!({"id": id, "username": "jlpicard", "global_name": "Jean-Luc Picard",
               "discriminator": "0", "avatar": null, "banner": banner, "clan": null,
               "primary_guild": {"tag": "NCC"}})
    }

    fn message(banner: &str, signature: &str) -> Value {
        json!({
            "id": "1001", "channel_id": "100", "type": 0, "content": "Make it so.",
            "author": person("1701", banner),
            "mentions": [person("1703", banner), person("1702", banner)],
            "mention_roles": ["9", "8"],
            "attachments": [{"id": "5", "filename": "warp.png", "size": 12,
                "url": format!("https://cdn.discordapp.com/attachments/100/5/warp.png?ex={signature}"),
                "proxy_url": format!("https://media.discordapp.net/x?ex={signature}")}],
            "referenced_message": {"id": "1000", "author": person("1702", banner)},
            "thread": {"id": "1001", "member_count": signature.len()},
        })
    }

    /// A person who changed their banner, and a file whose link was
    /// signed again, leave the message as it was.
    #[test]
    fn a_profile_and_a_signature_do_not_move_a_message() {
        let (a, av) = split_message(&message("old", "66"));
        let (b, bv) = split_message(&message("new", "6789"));
        assert_eq!(a, b);
        assert_ne!(av, bv, "what moved belongs in the sidecar, not dropped");
        assert_eq!(a["author"], json!({"id": "1701"}));
        assert_eq!(a["mentions"], json!([{"id": "1702"}, {"id": "1703"}]));
        assert_eq!(a["mention_roles"], json!(["8", "9"]));
        assert_eq!(a["attachments"][0]["filename"], "warp.png");
        assert!(a.get("thread").is_none() && a.get("referenced_message").is_none());
        let whole = dr::overlay(&a, &av.unwrap());
        assert_eq!(whole["author"]["username"], "jlpicard");
        assert!(whole["attachments"][0]["url"]
            .as_str()
            .unwrap()
            .contains("ex=66"));
    }

    /// A webhook posts under a name of its own each time; that name is
    /// nowhere else.
    #[test]
    fn a_webhooks_name_stays_on_its_message() {
        let mut m = message("old", "66");
        m["webhook_id"] = json!("1701");
        assert_eq!(split_message(&m).0["author"]["username"], "jlpicard");
        // An app answering a command is a user like any other.
        m["application_id"] = json!("1701");
        assert!(split_message(&m).0["author"].get("username").is_none());
    }

    #[test]
    fn a_new_message_does_not_move_its_channel() {
        let at = |newest: &str, archived: bool| {
            json!({"id": "100", "type": 11, "name": "Warp theory", "last_message_id": newest,
                   "thread_metadata": {"archived": archived, "locked": false}})
        };
        let (a, av) = split_channel(&at("1", false));
        let (b, _) = split_channel(&at("2", true));
        assert_eq!(a, b);
        assert_eq!(a["thread_metadata"], json!({"locked": false}));
        assert_eq!(av.unwrap()["last_message_id"], "1");
    }

    #[test]
    fn the_accounts_phone_is_never_stored() {
        let (kept, volatile) =
            split_account(&json!({"id": "1701", "email": "picard@enterprise.org",
            "phone": "+12025550100", "authenticator_types": [2], "bio": "Captain"}));
        assert_eq!(
            kept,
            json!({"id": "1701", "email": "picard@enterprise.org"})
        );
        assert_eq!(volatile, Some(json!({"bio": "Captain"})));
    }

    #[test]
    fn a_message_key_comes_apart_again() {
        let key = message_key("100", 1001);
        assert_eq!(key, "100#00000000000000001001");
        assert_eq!(split_message_key(&key), Some(("100", "1001".to_string())));
        assert_eq!(split_message_key("100"), None);
    }
}
