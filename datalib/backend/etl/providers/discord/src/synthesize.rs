//! Discord HTTP fixture synthesizer: a `world.json` saying what an
//! account can see becomes every answer a download of it could ask for.
//!
//! ```json
//! {"me": {…}, "guilds": [{…}], "dms": [{…}],
//!  "channels": {"<guild id>": [{…}]}, "roles": {"<guild id>": [{…}]},
//!  "members": {"<guild id>": {…}},
//!  "threads": {"<channel id>": [{…}]},
//!  "messages": {"<channel id>": [{…}]},
//!  "files": {"<link>": "<a file beside world.json>"},
//!  "befores": {"<channel id>": ["<id>"]}}
//! ```
//!
//! Messages may be in any order; a channel with none listed answers an
//! empty page. `befores` names ids a walk may page from besides its
//! messages' own.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl_web::http::{HttpRequest, HttpResponse};
use datalib_etl_web::synthesize::{json_response, write_fixture, SynthesizeReport, Synthesizer};
use serde_json::{json, Value};

use crate::ingest::{api, snowflake};

pub struct DiscordSynth {
    pub dir: PathBuf,
}

impl DiscordSynth {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }
}

impl Synthesizer for DiscordSynth {
    fn name(&self) -> &'static str {
        "discord"
    }

    fn synthesize(&self, out_root: &Path) -> Result<SynthesizeReport> {
        let path = self.dir.join("world.json");
        if !path.is_file() {
            return Ok(SynthesizeReport::default());
        }
        let text =
            std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let world: Value =
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        let mut files = BTreeMap::new();
        for (url, name) in world["files"].as_object().into_iter().flatten() {
            let name = name.as_str().context("a file's name is a string")?;
            let bytes =
                std::fs::read(self.dir.join(name)).with_context(|| format!("read {name}"))?;
            files.insert(url.clone(), bytes);
        }
        write_world(&world, &files, out_root)
    }
}

fn list<'a>(world: &'a Value, table: &str, id: &str) -> Vec<&'a Value> {
    world[table][id].as_array().into_iter().flatten().collect()
}

fn id_of(v: &Value) -> Option<&str> {
    v.get("id").and_then(Value::as_str)
}

/// Every answer about `world`, and `files` by their links.
pub fn write_world(
    world: &Value,
    files: &BTreeMap<String, Vec<u8>>,
    out: &Path,
) -> Result<SynthesizeReport> {
    let mut count = 0;
    let mut put = |req: HttpRequest, body: &Value| -> Result<()> {
        count += 1;
        write_fixture(out, &req, &json_response(body))
    };
    put(api::me_request(), &world["me"])?;
    let empty = json!([]);
    let or_empty = |v: &Value| {
        if v.is_null() {
            empty.clone()
        } else {
            v.clone()
        }
    };
    put(api::guilds_request(), &or_empty(&world["guilds"]))?;
    put(api::dms_request(), &or_empty(&world["dms"]))?;
    let mut places: Vec<&Value> = world["dms"].as_array().into_iter().flatten().collect();
    for guild in world["guilds"].as_array().into_iter().flatten() {
        let Some(id) = id_of(guild) else { continue };
        put(api::channels_request(id), &or_empty(&world["channels"][id]))?;
        put(api::roles_request(id), &or_empty(&world["roles"][id]))?;
        put(api::member_request(id), &world["members"][id])?;
        places.extend(list(world, "channels", id));
    }
    let parents: Vec<&str> = places.iter().filter_map(|c| id_of(c)).collect();
    for parent in parents {
        let mut threads = list(world, "threads", parent);
        threads.sort_by_key(|t| {
            std::cmp::Reverse(t["last_message_id"].as_str().and_then(snowflake::parse))
        });
        let size = api::THREADS_PER_PAGE as usize;
        let mut offset = 0;
        loop {
            let page: Vec<&&Value> = threads.iter().skip(offset).take(size).collect();
            let more = offset + page.len() < threads.len();
            put(
                api::threads_request(parent, offset),
                &json!({"threads": page, "members": [], "has_more": more,
                        "total_results": threads.len()}),
            )?;
            if !more {
                break;
            }
            offset += page.len();
        }
        places.extend(threads);
    }
    // A thread started by a message is nowhere in `threads` until the
    // channel's are listed again.
    for m in world["messages"]
        .as_object()
        .into_iter()
        .flat_map(|m| m.values())
    {
        places.extend(
            m.as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| m.get("thread")),
        );
    }
    for place in places {
        let Some(id) = id_of(place) else { continue };
        let extra = list(world, "befores", id);
        let extra = extra.into_iter().filter_map(Value::as_str);
        let extra: Vec<u64> = extra.filter_map(snowflake::parse).collect();
        for (req, page) in pages(id, &list(world, "messages", id), &extra) {
            put(req, &page)?;
        }
    }
    for (url, bytes) in files {
        let Some(req) = api::file_request(url, None) else {
            anyhow::bail!("{url} is not on Discord's file host");
        };
        count += 1;
        let resp = HttpResponse {
            status: 200,
            headers: BTreeMap::new(),
            body: bytes.clone(),
            duration_ms: 0,
        };
        write_fixture(out, &req, &resp)?;
    }
    Ok(SynthesizeReport {
        fixtures_written: count,
    })
}

/// Every page of a conversation a walk can ask for: the newest, what is
/// older than each page's oldest message, and, from any one message of a
/// short conversation, what is older than it, with it and without.
fn pages(channel: &str, messages: &[&Value], extra: &[u64]) -> Vec<(HttpRequest, Value)> {
    let mut by_id: Vec<(u64, &Value)> = messages
        .iter()
        .filter_map(|m| Some((snowflake::parse(id_of(m)?)?, *m)))
        .collect();
    by_id.sort_by_key(|(id, _)| std::cmp::Reverse(*id));
    let size = api::MESSAGES_PER_PAGE as usize;
    let older = |before: Option<u64>| -> Vec<&Value> {
        by_id
            .iter()
            .filter(|(id, _)| before.is_none_or(|b| *id < b))
            .take(size)
            .map(|(_, m)| *m)
            .collect()
    };
    let mut befores: Vec<Option<u64>> = vec![None];
    befores.extend(extra.iter().copied().map(Some));
    befores.extend(
        by_id
            .chunks(size)
            .filter_map(|page| Some(Some(page.last()?.0))),
    );
    for start in extra {
        let mut before = *start;
        while let Some((oldest, _)) = by_id
            .iter()
            .filter(|(id, _)| *id < before)
            .take(size)
            .last()
        {
            before = *oldest;
            befores.push(Some(before));
        }
    }
    if by_id.len() <= size {
        befores.extend(by_id.iter().flat_map(|(id, _)| [Some(*id), Some(*id + 1)]));
    }
    let mut out: Vec<(HttpRequest, Value)> = befores
        .into_iter()
        .map(|b| (api::messages_request(channel, b), json!(older(b))))
        .collect();
    for (id, m) in &by_id {
        if !crate::ingest::db::files_of(m).is_empty() {
            out.push((api::around_request(channel, *id), json!([m])));
        }
    }
    out
}
