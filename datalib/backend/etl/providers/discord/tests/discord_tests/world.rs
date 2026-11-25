//! A TNG account's Discord, as the worlds the synthesizer turns into
//! playback tapes. Field names are as the live API answered them
//! (2026-11-24); the values are made up. A snowflake cannot say a date
//! after 2154, so the crew chat in 2024.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use datalib_etl_discord::ingest::snowflake;
use datalib_etl_discord::synthesize::write_world;
use datalib_time::IsoOffsetTimestamp;
use serde_json::{json, Value};

pub const PICARD: &str = "1701";
pub const RIKER: &str = "1702";
pub const TROI: &str = "1703";

pub const ENTERPRISE: &str = "7000";
pub const ACADEMY: &str = "8000";

pub const BRIDGE: &str = "7001";
pub const READY_ROOM: &str = "7002";
pub const IDEAS: &str = "7003";
pub const DECK: &str = "7009";
pub const RIKER_DM: &str = "9001";

/// Messages in the bridge, more than two pages of them.
pub const BRIDGE_MESSAGES: usize = 230;

pub fn now() -> IsoOffsetTimestamp {
    datalib_time::parse_with_assumed_utc("2024-06-01T00:00:00Z").unwrap()
}

/// The id of something made `minutes` after 2024-05-01.
pub fn at(minutes: u64) -> u64 {
    let start = datalib_time::parse_with_assumed_utc("2024-05-01T00:00:00Z").unwrap();
    snowflake::floor_at(start.to_unix_millis() + minutes as i64 * 60_000) | 1
}

pub fn stamp(id: u64) -> String {
    IsoOffsetTimestamp::from_unix_millis(snowflake::unix_ms(id))
        .unwrap()
        .to_rfc3339_micros()
}

pub fn user(id: &str) -> Value {
    let (username, name) = match id {
        PICARD => ("jlpicard", "Jean-Luc Picard"),
        RIKER => ("wtriker", "Will Riker"),
        _ => ("dtroi", "Deanna Troi"),
    };
    json!({"id": id, "username": username, "global_name": name, "discriminator": "0",
           "avatar": null, "banner": null, "public_flags": 0, "flags": 0, "clan": null,
           "primary_guild": null, "avatar_decoration_data": null})
}

pub fn message(channel: &str, id: u64, author: &str, content: &str) -> Value {
    json!({"id": id.to_string(), "channel_id": channel, "type": 0, "content": content,
           "author": user(author), "timestamp": stamp(id), "edited_timestamp": null,
           "mentions": [], "mention_roles": [], "mention_everyone": false, "attachments": [],
           "embeds": [], "components": [], "flags": 0, "pinned": false, "tts": false})
}

/// A file's link, good until `until`.
pub fn link(name: &str, until: i64) -> String {
    format!("https://cdn.discordapp.com/attachments/7001/5001/{name}?ex={until:x}&is=1&hm=abc&")
}

pub fn far_off() -> i64 {
    now().to_unix_millis() / 1000 + 86_400
}

pub fn file(id: &str, name: &str, url: &str, size: u64) -> Value {
    json!({"id": id, "filename": name, "size": size, "url": url,
           "proxy_url": url.replace("cdn.discordapp.com", "media.discordapp.net"),
           "content_type": "text/plain", "content_scan_version": 1})
}

pub fn channel(id: &str, kind: i64, name: &str, newest: Option<u64>) -> Value {
    json!({"id": id, "type": kind, "name": name, "guild_id": ENTERPRISE, "parent_id": DECK,
           "position": 1, "permission_overwrites": [], "flags": 0, "topic": null,
           "last_message_id": newest.map(|n| n.to_string())})
}

pub fn thread(id: u64, parent: &str, name: &str, newest: u64) -> Value {
    json!({"id": id.to_string(), "type": 11, "name": name, "guild_id": ENTERPRISE,
           "parent_id": parent, "owner_id": RIKER, "last_message_id": newest.to_string(),
           "message_count": 1, "member_count": 2, "total_message_sent": 1,
           "member_ids_preview": [RIKER],
           "thread_metadata": {"archived": false, "archive_timestamp": stamp(id),
                               "auto_archive_duration": 1440, "locked": false}})
}

pub struct World {
    pub value: Value,
    pub files: BTreeMap<String, Vec<u8>>,
}

impl World {
    /// The Enterprise: a busy bridge, a ready room the account may not
    /// read, a forum with one post, a thread off a bridge message, and a
    /// direct message; and the Academy, which no test copies.
    pub fn before() -> Self {
        let bridge_newest = at(BRIDGE_MESSAGES as u64 - 1);
        let mut bridge: Vec<Value> = (0..BRIDGE_MESSAGES as u64)
            .map(|n| message(BRIDGE, at(n), RIKER, &format!("Log entry {n}.")))
            .collect();
        let started = thread(at(100), BRIDGE, "Warp core", at(105));
        bridge[100]["thread"] = started.clone();
        bridge[200]["attachments"] =
            json!([file("6001", "log.txt", &link("log.txt", far_off()), 9)]);
        let post = thread(at(50), IDEAS, "A holodeck for the crew", at(51));
        let read = "66560";
        let mut ready_room = channel(READY_ROOM, 0, "ready-room", Some(at(9)));
        ready_room["permission_overwrites"] =
            json!([{"id": ENTERPRISE, "type": 0, "allow": "0", "deny": read}]);
        let value = json!({
            "me": {"id": PICARD, "username": "jlpicard", "global_name": "Jean-Luc Picard",
                   "email": "picard@enterprise.starfleet", "phone": "+12025550100",
                   "discriminator": "0", "avatar": null, "locale": "en-US", "bio": "Captain"},
            "guilds": [
                {"id": ENTERPRISE, "name": "USS Enterprise", "icon": null, "owner": false,
                 "permissions": read, "features": ["NEWS", "COMMUNITY"]},
                {"id": ACADEMY, "name": "Starfleet Academy", "icon": null, "owner": false,
                 "permissions": read, "features": []},
            ],
            "dms": [{"id": RIKER_DM, "type": 1, "flags": 0,
                     "last_message_id": at(301).to_string(), "recipients": [user(RIKER)]}],
            "channels": {
                (ENTERPRISE): [
                    channel(DECK, 4, "Deck One", None),
                    channel(BRIDGE, 0, "bridge", Some(bridge_newest)),
                    ready_room,
                    channel(IDEAS, 15, "ideas", Some(at(50))),
                ],
                (ACADEMY): [],
            },
            "roles": {(ENTERPRISE): [{"id": ENTERPRISE, "name": "@everyone", "permissions": read}],
                      (ACADEMY): []},
            "members": {(ENTERPRISE): {"roles": [], "nick": null, "user": user(PICARD)},
                        (ACADEMY): {"roles": []}},
            "threads": {(BRIDGE): [started], (IDEAS): [post]},
            "messages": {
                (BRIDGE): bridge,
                (at(100).to_string()): [message(&at(100).to_string(), at(105), TROI, "Breach imminent.")],
                (at(50).to_string()): [
                    message(&at(50).to_string(), at(50), RIKER, "We should build one."),
                    message(&at(50).to_string(), at(51), PICARD, "Make it so."),
                ],
                (RIKER_DM): [
                    message(RIKER_DM, at(300), RIKER, "Poker tonight?"),
                    message(RIKER_DM, at(301), PICARD, "I'll be there."),
                ],
            },
        });
        let files = [(link("log.txt", far_off()), b"Stardate.".to_vec())].into();
        World { value, files }
    }

    /// A day later: one bridge message edited, one deleted, one new; a
    /// reply in the forum post; the direct message quiet.
    pub fn after() -> Self {
        let mut w = Self::before();
        let newest = at(BRIDGE_MESSAGES as u64 + 60);
        let bridge = w.value["messages"][BRIDGE].as_array_mut().unwrap();
        bridge[220]["content"] = json!("Log entry 220, corrected.");
        bridge[220]["edited_timestamp"] = json!(stamp(newest));
        bridge.remove(225);
        bridge.push(message(BRIDGE, newest, TROI, "I sense something."));
        w.tip_channel(BRIDGE, newest);
        let post = at(50).to_string();
        let reply = at(400);
        let m = message(&post, reply, TROI, "Is it safe?");
        w.value["messages"][&post].as_array_mut().unwrap().push(m);
        w.value["threads"][IDEAS][0]["last_message_id"] = json!(reply.to_string());
        w
    }

    pub fn tip_channel(&mut self, id: &str, newest: u64) {
        for c in self.value["channels"][ENTERPRISE].as_array_mut().unwrap() {
            if c["id"] == id {
                c["last_message_id"] = json!(newest.to_string());
            }
        }
    }

    pub fn tape(&self, root: &Path, name: &str) -> PathBuf {
        let out = root.join(name);
        write_world(&self.value, &self.files, &out).unwrap();
        out
    }
}
