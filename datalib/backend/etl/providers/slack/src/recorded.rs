//! Recorded Slack calls for tests that build their own workspace, in the
//! layout [`SlackSynth`](crate::synthesize::SlackSynth) reads. The
//! synthesizer turns every line under `raw_api/<method>/` into one
//! fixture keyed by its params, so calls are appended in any order and
//! no test names a file.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// `datetime_to_slack_ts` of UTC midnight on
/// [`DEFAULT_SINCE`](crate::ingest::DEFAULT_SINCE): the `oldest` a cold
/// start's history walk sends.
pub const DEFAULT_SINCE_TS: &str = "1704067200.000000";

/// What a download lists when DMs are off.
pub const CHANNEL_TYPES: &str = "public_channel,private_channel";

pub fn record_call(api_dir: &Path, method: &str, params: Value, response: Value) -> Result<()> {
    let path = api_dir.join(format!("raw_api/{method}/recorded.jsonl"));
    fs::create_dir_all(path.parent().expect("a file under raw_api"))
        .with_context(|| format!("mkdir -p {}", path.display()))?;
    let line = json!({"method": method, "params": params, "response": response});
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .and_then(|mut f| writeln!(f, "{line}"))
        .with_context(|| format!("append to {}", path.display()))
}

/// `auth.test` for user `U1` on team `T1`, "Enterprise".
pub fn record_auth(api_dir: &Path) -> Result<()> {
    record_call(
        api_dir,
        "auth.test",
        json!({}),
        json!({"ok": true, "user_id": "U1", "team": "Enterprise", "team_id": "T1"}),
    )
}

pub fn record_users(api_dir: &Path, members: Value) -> Result<()> {
    record_call(
        api_dir,
        "users.list",
        json!({"limit": "200"}),
        json!({"ok": true, "members": members}),
    )
}

/// One `conversations.list` page listing `channels`, asked for `types`.
pub fn record_conversations(api_dir: &Path, types: &str, channels: Value) -> Result<()> {
    record_call(
        api_dir,
        "conversations.list",
        json!({"exclude_archived": "true", "limit": "200", "types": types}),
        json!({"ok": true, "channels": channels, "has_more": false}),
    )
}

/// One `conversations.history` page. Playback keys on the exact params,
/// so each `(oldest, latest, inclusive)` the downloader sends needs its
/// own; [`History::cold`] is the one a cold start sends.
#[derive(Clone, Copy)]
pub struct History<'a> {
    pub channel: &'a str,
    pub oldest: &'a str,
    pub latest: Option<&'a str>,
    pub inclusive: bool,
    /// Claims more pages without a cursor to follow: the shape that ends
    /// a walk short of its range.
    pub has_more: bool,
}

impl<'a> History<'a> {
    pub fn cold(channel: &'a str) -> Self {
        Self::from(channel, DEFAULT_SINCE_TS)
    }

    pub fn from(channel: &'a str, oldest: &'a str) -> Self {
        Self {
            channel,
            oldest,
            latest: None,
            inclusive: true,
            has_more: false,
        }
    }

    pub fn record(self, api_dir: &Path, messages: Value) -> Result<()> {
        let mut params = json!({
            "channel": self.channel,
            "include_all_metadata": "true",
            "inclusive": if self.inclusive { "true" } else { "false" },
            "limit": "200",
            "oldest": self.oldest,
        });
        if let Some(latest) = self.latest {
            params["latest"] = json!(latest);
        }
        record_call(
            api_dir,
            "conversations.history",
            params,
            json!({"ok": true, "messages": messages, "has_more": self.has_more}),
        )
    }
}

/// Picard's workspace as a cold start reads it: a member of every
/// channel, each holding what `messages` gives it.
pub fn record_workspace(
    api_dir: &Path,
    channels: &[&str],
    messages: impl Fn(usize, &str) -> Value,
) -> Result<()> {
    record_auth(api_dir)?;
    record_users(
        api_dir,
        json!([{"id": "U1", "name": "picard", "real_name": "Jean-Luc Picard"}]),
    )?;
    let listed: Vec<Value> = channels
        .iter()
        .map(
            |c| json!({"id": c, "name": c.to_lowercase(), "is_member": true, "is_archived": false}),
        )
        .collect();
    record_conversations(api_dir, CHANNEL_TYPES, json!(listed))?;
    for (i, c) in channels.iter().enumerate() {
        History::cold(c).record(api_dir, messages(i, c))?;
    }
    Ok(())
}
