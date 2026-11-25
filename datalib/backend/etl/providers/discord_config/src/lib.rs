//! Provider-owned config schema for the `discord` source. Schema-only
//! (serde + anyhow), so whatever has to understand a config can name
//! `DiscordConfig` without linking the provider.

use datalib_source_common::{LatchkeySettings, SourceCommon};
use serde::{Deserialize, Serialize};

/// This source's config stanzas, for `datalib-step help`.
pub const EXAMPLES: &[datalib_source_common::ConfigExample] = &[
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Minimal,
        method: "api",
        toml: include_str!("../examples/minimal_api.toml"),
    },
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Complete,
        method: "api",
        toml: include_str!("../examples/complete_api.toml"),
    },
];

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiscordConfig {
    #[serde(default)]
    pub common: SourceCommon,
    /// Which stored latchkey account this source signs in as.
    #[serde(default)]
    pub latchkey_settings: LatchkeySettings,
    /// Discord's API as your own account, the one way in.
    #[serde(default)]
    pub api: Option<DiscordSync>,
}

impl DiscordConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.latchkey_settings
            .validate()
            .map_err(anyhow::Error::msg)?;
        if let Some(api) = &self.api {
            api.validate()?;
        }
        Ok(())
    }
}

/// What to copy.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DiscordSync {
    /// The servers to copy, by name or by id. Unset copies none.
    #[serde(default)]
    pub servers: Option<Vec<String>>,
    /// Copy every server the account is in, whatever `servers` says. An
    /// account is often in servers far too big to copy.
    #[serde(default)]
    pub all_servers: bool,
    /// Copy only these channels of those servers: each a channel's id, its
    /// name, or a pasted link to it. A category's id or name takes every
    /// channel in it. Unset copies every channel the account can read.
    #[serde(default)]
    pub channels: Option<Vec<String>>,
    /// Copy the threads and forum posts of the channels copied. Finding
    /// them costs a request per channel each time they are listed.
    #[serde(default = "yes")]
    pub threads: bool,
    /// Copy direct messages, one-to-one and group.
    #[serde(default = "yes")]
    pub dms: bool,
    /// Copy only these direct messages, each by its id or a pasted link.
    /// Unset, with `dms`, copies every one in the account's list.
    #[serde(default)]
    pub dm_conversations: Option<Vec<String>>,
    /// Oldest message to fetch: a day (`2026-09-01`), an RFC 3339
    /// instant, or an offset from the run's start (`-30d`). An offset
    /// slides forward each run; a message it leaves behind stays in the
    /// mirror. Unset fetches the whole history, 100 messages a request.
    #[serde(default)]
    pub since: Option<String>,
    /// Download the files messages carry. Off keeps each file's name and
    /// details.
    #[serde(default = "yes")]
    pub media: bool,
    /// Re-read this many trailing days of what is already mirrored, so
    /// edits, reactions and deletions land. It never decides how far back
    /// the mirror goes; that is `since`. Unset is 7; `0` turns it off.
    #[serde(default)]
    pub refresh_window_days: Option<i64>,
    /// How many minutes after a conversation's window was last re-read
    /// before a run re-reads it again. Unset is 360; `0` re-reads it on
    /// every run.
    #[serde(default)]
    pub refresh_every_minutes: Option<i64>,
    /// How many hours after a server's roles and a channel's threads were
    /// last listed before a run lists them again. A thread new in a
    /// channel shows with the message that started it either way. Unset
    /// is 6; `0` lists them on every run.
    #[serde(default)]
    pub list_every_hours: Option<i64>,
}

pub const DEFAULT_REFRESH_WINDOW_DAYS: i64 = 7;
pub const DEFAULT_REFRESH_EVERY_MINUTES: i64 = 360;
pub const DEFAULT_LIST_EVERY_HOURS: i64 = 6;

fn yes() -> bool {
    true
}

impl Default for DiscordSync {
    fn default() -> Self {
        Self {
            servers: None,
            all_servers: false,
            channels: None,
            threads: true,
            dms: true,
            dm_conversations: None,
            since: None,
            media: true,
            refresh_window_days: None,
            refresh_every_minutes: None,
            list_every_hours: None,
        }
    }
}

impl DiscordSync {
    pub fn wants_servers(&self) -> bool {
        self.all_servers || self.servers.as_ref().is_some_and(|s| !s.is_empty())
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        datalib_time::TimeWindow::check("api", self.since.as_deref(), None)
            .map_err(anyhow::Error::msg)?;
        for (name, value) in [
            ("api.refresh_window_days", self.refresh_window_days),
            ("api.refresh_every_minutes", self.refresh_every_minutes),
            ("api.list_every_hours", self.list_every_hours),
        ] {
            datalib_source_common::not_negative(name, value).map_err(anyhow::Error::msg)?;
        }
        if !self.dms && !self.wants_servers() {
            anyhow::bail!(
                "`api.dms` is false and no server is named, so nothing would be copied. Name \
                 the servers to copy in `api.servers`, or leave `dms` on."
            );
        }
        // Either silent reading of these is wrong: a list nothing takes
        // up would copy less than the file plainly says, or more.
        if !self.dms
            && self
                .dm_conversations
                .as_ref()
                .is_some_and(|c| !c.is_empty())
        {
            anyhow::bail!(
                "`api.dm_conversations` names conversations but `api.dms` is false, so none \
                 would be copied. Turn `dms` on, or drop the list."
            );
        }
        if self.channels.is_some() && !self.wants_servers() {
            anyhow::bail!(
                "`api.channels` names channels but no server is copied. Name their servers in \
                 `api.servers`, or set `all_servers = true`."
            );
        }
        Ok(())
    }
}

/// No render knobs.
pub type DiscordRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for DiscordConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::origin("api")];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api(json: &str) -> anyhow::Result<()> {
        let cfg: DiscordConfig = serde_json::from_str(&format!(r#"{{"api":{json}}}"#))?;
        cfg.validate()
    }

    /// The serde defaults and the `Default` impl have to agree, or a
    /// config that omits a knob behaves differently from one built in
    /// code. An empty table copies the direct messages and no server.
    #[test]
    fn an_empty_api_table_copies_the_dms_and_no_server() {
        let cfg: DiscordConfig = serde_json::from_str(r#"{"api":{}}"#).unwrap();
        cfg.validate().unwrap();
        let api = cfg.api.unwrap();
        assert!(api.dms && api.threads && api.media);
        assert!(!api.wants_servers());
        let default = DiscordSync::default();
        assert_eq!(
            serde_json::to_value(&api).unwrap(),
            serde_json::to_value(&default).unwrap()
        );
    }

    #[test]
    fn a_config_that_copies_nothing_is_refused() {
        let e = api(r#"{"dms":false}"#).unwrap_err().to_string();
        assert!(e.contains("api.servers"), "{e}");
        assert!(api(r#"{"dms":false,"servers":[]}"#).is_err());
        api(r#"{"dms":false,"servers":["Starfleet"]}"#).unwrap();
        api(r#"{"dms":false,"all_servers":true}"#).unwrap();
    }

    #[test]
    fn a_list_nothing_takes_up_is_refused() {
        let e = api(r#"{"dms":false,"all_servers":true,"dm_conversations":["1"]}"#)
            .unwrap_err()
            .to_string();
        assert!(e.contains("dm_conversations"), "{e}");
        let e = api(r#"{"channels":["bridge"]}"#).unwrap_err().to_string();
        assert!(e.contains("api.channels"), "{e}");
    }

    #[test]
    fn since_takes_the_shared_window_grammar() {
        for ok in ["2364-01-01", "2364-01-01T09:00:00Z", "1900-01-01", "-90d"] {
            api(&format!(r#"{{"since":"{ok}"}}"#)).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        let e = api(r#"{"since":"stardate 41153.7"}"#)
            .unwrap_err()
            .to_string();
        assert!(e.contains("`api.since`"), "{e}");
    }

    #[test]
    fn an_unknown_key_is_refused() {
        assert!(api(r#"{"guilds":["x"]}"#).is_err());
        assert!(api(r#"{"list_every_hours":-1}"#).is_err());
    }
}
