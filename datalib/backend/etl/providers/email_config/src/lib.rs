//! Provider-owned config schema for the `email` source — Program A goal #1
//! ("one config definition per source, adjacent to the source").

use datalib_source_common::{expand_tilde, LatchkeySettings, RenderCommon, SourceCommon};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// This source's config stanzas, for `datalib-step help`.
pub const EXAMPLES: &[datalib_source_common::ConfigExample] = &[
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Minimal,
        method: "jmap",
        toml: include_str!("../examples/minimal_jmap.toml"),
    },
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Complete,
        method: "jmap",
        toml: include_str!("../examples/complete_jmap.toml"),
    },
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Minimal,
        method: "gmail",
        toml: include_str!("../examples/minimal_gmail.toml"),
    },
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Complete,
        method: "gmail",
        toml: include_str!("../examples/complete_gmail.toml"),
    },
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Minimal,
        method: "mbox",
        toml: include_str!("../examples/minimal_mbox.toml"),
    },
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Complete,
        method: "mbox",
        toml: include_str!("../examples/complete_mbox.toml"),
    },
];

/// The full config for a `type: email` source: the shared `common:` envelope
/// (paths + cross-source knobs, composed from `source_common` and resolved by
/// the orchestrator's `normalize()`) plus everything email-specific. `name`
/// and `enabled` stay orchestrator-owned and are NOT here.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmailConfig {
    /// Shared per-source envelope (paths + cross-source tunables).
    #[serde(default)]
    pub common: SourceCommon,
    /// Which latchkey identity this source mirrors. Composed only by the
    /// providers that authenticate through the `latchkey` CLI, and
    /// forwarded whole to the download client — see [`LatchkeySettings`].
    #[serde(default)]
    pub latchkey_settings: LatchkeySettings,
    /// JMAP knobs. `Some` selects the JMAP live-server download path.
    #[serde(default)]
    pub jmap: Option<EmailSync>,
    /// Gmail REST API knobs. `Some` selects the Gmail download path.
    /// Excludes `jmap`; beside `mbox`, the mbox is a Google Takeout of
    /// the same account that the API starts from.
    #[serde(default)]
    pub gmail: Option<EmailGmailApi>,
    /// The mbox path: where the `.mbox` is, plus the account row to
    /// synthesize for it (JMAP and Gmail learn that from the server).
    /// Beside `gmail`, only `path` is read: the account is the one Gmail
    /// signs in as.
    #[serde(default)]
    pub mbox: Option<MboxSync>,
    /// Retired here: `outlink_format` belongs on the render step's
    /// params now. Still read, so a config that sets it on the ingest step
    /// is refused with that fix rather than ignored.
    #[serde(default)]
    #[schemars(skip)]
    pub outlink_format: Option<EmailOutlink>,
    /// Limit **extraction** to messages under *any* of these mailboxes,
    /// matched on the full label path (POSIX-like, e.g. `Work/Projects`)
    /// — nested labels must be listed explicitly. Several labels union,
    /// they do not intersect. Empty = download everything. Applies to
    /// every download mode. Independent of the render step's
    /// `only_render_labels`.
    #[serde(default)]
    pub only_extract_labels: Vec<String>,
    /// Retired here: `only_render_labels` belongs on the render step's
    /// params now. Still read, so a config that sets it on the ingest step
    /// is refused with that fix rather than ignored.
    #[serde(default)]
    #[schemars(skip)]
    pub only_render_labels: Vec<String>,
}

/// Params for the email **render** step. Split from [`EmailConfig`]
/// (the download-step params) so each step's params carry only what
/// that wave reads.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmailRenderConfig {
    #[serde(default)]
    pub common: RenderCommon,
    /// Webmail to build each email's `↗` outlink for. `gmail` for a Google
    /// Takeout `.mbox`, `fastmail` for a Fastmail JMAP account. Omit for any
    /// other server (no outlink).
    #[serde(default)]
    pub outlink_format: Option<EmailOutlink>,
    /// Limit **rendering** to threads with at least one email under one of
    /// these mailbox label paths (POSIX-like, exact match). Empty = render
    /// everything extracted. Separate from the download step's
    /// `only_extract_labels`, so a giant inbox can be extracted in full but
    /// rendered down to a subset.
    #[serde(default)]
    pub only_render_labels: Vec<String>,
}

/// Which live-server transport a source selected, if any. The file-backed
/// mbox mode is deliberately *not* a variant: it is [`EmailConfig::mbox`],
/// and the provider asks for [`EmailConfig::live_mode`] first and reads
/// the mbox table when it comes back `None`.
#[derive(Debug, Clone)]
pub enum EmailLiveMode<'a> {
    Jmap(&'a EmailSync),
    GmailApi(&'a EmailGmailApi),
}

impl EmailConfig {
    pub fn live_mode(&self) -> anyhow::Result<Option<EmailLiveMode<'_>>> {
        let mut selected: Vec<(&str, EmailLiveMode<'_>)> = Vec::new();
        if let Some(s) = &self.jmap {
            selected.push(("jmap", EmailLiveMode::Jmap(s)));
        }
        if let Some(g) = &self.gmail {
            selected.push(("gmail", EmailLiveMode::GmailApi(g)));
        }
        match selected.len() {
            0 => Ok(None),
            1 => Ok(Some(selected.pop().expect("len checked").1)),
            _ => anyhow::bail!(
                "email source sets more than one download mode ({}) — pick one. \
                 To mirror the same account two ways, declare two sources.",
                selected
                    .iter()
                    .map(|(name, _)| *name)
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        }
    }

    /// Provider-local validation, run by the step planner at config load:
    /// at most one live mode is selected, an mbox sits only beside Gmail,
    /// and each mode's own fields hang together.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.latchkey_settings
            .validate()
            .map_err(anyhow::Error::msg)?;
        let live = self.live_mode()?;
        match (&live, &self.mbox) {
            (Some(EmailLiveMode::Jmap(_)), Some(_)) => anyhow::bail!(
                "email source sets `jmap` and `mbox` — pick one. Only a Gmail account can \
                 start from an mbox, a Google Takeout: its messages carry Gmail's ids."
            ),
            (Some(EmailLiveMode::GmailApi(_)), Some(mbox)) => mbox.validate_beside_gmail()?,
            _ => {}
        }
        match live {
            Some(EmailLiveMode::GmailApi(gmail)) => gmail.validate(),
            _ => Ok(()),
        }
    }

    /// The Google Takeout a Gmail source starts from, when it names one.
    pub fn takeout(&self) -> Option<PathBuf> {
        match (&self.gmail, &self.mbox) {
            (Some(_), Some(mbox)) => Some(mbox.path()),
            _ => None,
        }
    }
}

/// JMAP tunables: the `jmap` table of a `type = "email"` ingest step.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmailSync {
    /// JMAP server hostname. Session discovered at
    /// `https://<hostname>/.well-known/jmap` (e.g. `api.fastmail.com`).
    pub hostname: String,
    /// JMAP account id. Defaults to the session's mail primary account.
    #[serde(default)]
    pub account_id: Option<String>,
    /// List the account again with `Email/query` and fetch every email
    /// again, even if a `changes` state token is stored. Defaults to
    /// false (incremental).
    #[serde(default)]
    pub full_resync: bool,
    /// How many `.eml` blob downloads to run concurrently in the
    /// end-of-sync blob phase. JMAP has no bulk-download method — each
    /// `.eml` is one HTTP GET against the substituted `downloadUrl` — so
    /// the only lever for a large initial backfill is fetching several at
    /// once. Unset uses the built-in default of 8; `1` downloads one at a
    /// time, and anything lower counts as 1.
    #[serde(default)]
    pub blob_download_concurrency: Option<usize>,
}

/// The mbox download path: a `.mbox` file, or a directory holding one,
/// plus account-row data so the synthesized `accounts` row matches
/// JMAP's shape. The row fields are optional (defaults: `account_id` ←
/// mbox file stem, `display_name` ← `account_id`, `is_personal` ← true).
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MboxSync {
    /// The `.mbox` file, or a directory containing one.
    pub path: PathBuf,
    /// Id of the account the mailbox is filed under. Defaults to the last
    /// part of `path`, without its extension, in lowercase with underscores.
    #[serde(default)]
    pub account_id: Option<String>,
    /// The account's display name. Defaults to `account_id`.
    #[serde(default)]
    pub display_name: Option<String>,
    /// The account's email address. Unset by default.
    #[serde(default)]
    pub email_address: Option<String>,
    /// Whether this is a personal (rather than work) account. Defaults to
    /// true.
    #[serde(default)]
    pub is_personal: Option<bool>,
}

impl MboxSync {
    pub fn path(&self) -> PathBuf {
        expand_tilde(&self.path)
    }

    /// Beside `gmail` the account row is Gmail's, so a field that would
    /// describe a second one is a mistake, not a setting.
    fn validate_beside_gmail(&self) -> anyhow::Result<()> {
        let set: Vec<&str> = [
            ("account_id", self.account_id.is_some()),
            ("display_name", self.display_name.is_some()),
            ("email_address", self.email_address.is_some()),
            ("is_personal", self.is_personal.is_some()),
        ]
        .into_iter()
        .filter_map(|(key, is_set)| is_set.then_some(key))
        .collect();
        if set.is_empty() {
            return Ok(());
        }
        anyhow::bail!(
            "email `mbox` beside `gmail` is the account's Takeout, filed under the account \
             Gmail signs in as, so `mbox.{}` does nothing there. Remove it; `gmail` has \
             its own `account_id`, `display_name` and `email_address`.",
            set.join("`, `mbox.")
        )
    }
}

/// How to build the "open this email in webmail" outlink. The provider that
/// owns the account picks the most robust scheme our download identifiers
/// allow (Gmail → `rfc822msgid:` search; Fastmail → `app.fastmail.com` path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EmailOutlink {
    Gmail,
    Fastmail,
}

/// Gmail REST API tunables: the `gmail` table.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EmailGmailApi {
    /// **Retired** — use `latchkey_settings.account` beside the `gmail`
    /// table instead, which every source that signs in through latchkey
    /// shares. Still read, so a config that sets it here is refused at
    /// load with that fix rather than ignored.
    #[serde(default)]
    #[schemars(skip)]
    pub account: Option<String>,
    /// Gmail `userId` path segment. `me` (the default) is the
    /// authenticated user and is almost always right; a literal address
    /// only differs under domain-wide delegation.
    #[serde(default)]
    pub user_id: Option<String>,
    /// Stable id for the synthesized `accounts` row. Defaults to the
    /// address reported by `users.getProfile`.
    #[serde(default)]
    pub account_id: Option<String>,
    /// Display name for the `accounts` row. Defaults to `account_id`.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Canonical address for the `accounts` row. Defaults to the address
    /// reported by `users.getProfile`.
    #[serde(default)]
    pub email_address: Option<String>,
    /// Discard the stored `historyId` cursor and walk every configured
    /// label again. Messages already mirrored are not fetched again.
    /// Applies to this run only.
    #[serde(default)]
    pub full_resync: bool,
    /// How many `messages.get` requests to keep in flight. Unset uses the
    /// built-in default of 8. Raising it does not raise throughput past
    /// `quota_units_per_minute`; it only hides the wait on each request.
    #[serde(default)]
    pub request_concurrency: Option<usize>,
    /// Client-side ceiling on Gmail API quota units spent per minute.
    /// A starting point: the throttle lowers it on its own whenever
    /// Google rate-limits a request.
    #[serde(default)]
    pub quota_units_per_minute: Option<u32>,
    /// Stop after fetching this many message bodies in one run and exit
    /// **successfully** with a partial result; the rest stays owed, so
    /// the next run goes on from there. For experiments and the live test; a large
    /// mailbox needs no budget, the run paces itself.
    #[serde(default)]
    pub message_budget: Option<usize>,
}

/// Gmail's per-user quota is 6000 units/minute. Default below it so
/// retries and a little clock skew don't push us into rate limits.
pub const DEFAULT_QUOTA_UNITS_PER_MINUTE: u32 = 5_000;
/// Enough in flight to hide per-request latency; the quota throttle, not
/// this, is what actually bounds throughput.
pub const DEFAULT_GMAIL_CONCURRENCY: usize = 8;

impl EmailGmailApi {
    pub fn user_id(&self) -> &str {
        self.user_id.as_deref().unwrap_or("me")
    }

    pub fn quota_units_per_minute(&self) -> u32 {
        self.quota_units_per_minute
            .unwrap_or(DEFAULT_QUOTA_UNITS_PER_MINUTE)
    }

    pub fn request_concurrency(&self) -> usize {
        self.request_concurrency
            .unwrap_or(DEFAULT_GMAIL_CONCURRENCY)
            .max(1)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.quota_units_per_minute.is_some_and(|q| q == 0) {
            anyhow::bail!(
                "email `gmail.quota_units_per_minute` must be > 0 (omit it for the default \
                 of {DEFAULT_QUOTA_UNITS_PER_MINUTE})"
            );
        }
        if let Some(account) = &self.account {
            anyhow::bail!(
                "email `gmail.account` has moved to `latchkey_settings.account`, which \
                 every latchkey-backed source shares. Replace it with a sibling of \
                 `gmail`:\n\n    [steps.params.latchkey_settings]\n    account = \
                 {account:?}\n"
            );
        }
        Ok(())
    }
}

impl datalib_source_common::IngestMethods for EmailConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] = &[
        datalib_source_common::IngestMethod::origin("jmap"),
        datalib_source_common::IngestMethod::origin("gmail"),
        datalib_source_common::IngestMethod::local("mbox"),
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole reason mode selection became explicit: with more than
    /// one mode, inferring from one table alone would silently pick one.
    #[test]
    fn rejects_more_than_one_live_mode() {
        let cfg = EmailConfig {
            jmap: Some(EmailSync::default()),
            gmail: Some(EmailGmailApi::default()),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("more than one"), "unhelpful message: {err}");
    }

    /// The message has to name *which* modes collided, or the user has to
    /// go re-read their own config to find out.
    #[test]
    fn names_the_colliding_modes() {
        let cfg = EmailConfig {
            jmap: Some(EmailSync::default()),
            gmail: Some(EmailGmailApi::default()),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("jmap"), "{err}");
        assert!(err.contains("gmail"), "{err}");
    }

    /// A JMAP server's ids are its own, so a Takeout beside it would
    /// double every message rather than seed it.
    #[test]
    fn rejects_jmap_beside_an_mbox() {
        let cfg: EmailConfig = serde_json::from_value(serde_json::json!({
            "jmap": { "hostname": "api.fastmail.com" },
            "mbox": { "path": "/mail.mbox" },
        }))
        .unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("mbox"), "{err}");
        assert!(cfg.takeout().is_none());
    }

    /// Gmail beside an mbox is a Takeout the API starts from: the one
    /// pairing of methods the email source accepts.
    #[test]
    fn accepts_gmail_beside_a_takeout() {
        let cfg: EmailConfig = serde_json::from_value(serde_json::json!({
            "gmail": {},
            "mbox": { "path": "~/Takeout/Mail" },
        }))
        .unwrap();
        cfg.validate().unwrap();
        assert!(cfg.takeout().unwrap().ends_with("Takeout/Mail"));
        assert!(matches!(
            cfg.live_mode().unwrap(),
            Some(EmailLiveMode::GmailApi(_))
        ));
    }

    /// The account a Takeout is filed under is the one Gmail signs in
    /// as; a second account named on the mbox would be ignored, so it is
    /// refused by name.
    #[test]
    fn refuses_an_mbox_account_beside_gmail() {
        let cfg: EmailConfig = serde_json::from_value(serde_json::json!({
            "gmail": {},
            "mbox": { "path": "/mail.mbox", "account_id": "old", "display_name": "Old" },
        }))
        .unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("mbox.account_id"), "{err}");
        assert!(err.contains("mbox.display_name"), "{err}");
    }

    #[test]
    fn gmail_api_defaults_match_googles_documented_limits() {
        let g = EmailGmailApi::default();
        // `me` is the authenticated user; a literal address only differs
        // under domain-wide delegation.
        assert_eq!(g.user_id(), "me");
        // Google's per-user ceiling is 6000 units/min; stay under it.
        assert!(g.quota_units_per_minute() < 6_000);
        assert!(g.request_concurrency() >= 1);
    }

    /// A zero ceiling would wedge the run forever rather than failing.
    #[test]
    fn rejects_a_zero_quota_ceiling() {
        let g = EmailGmailApi {
            quota_units_per_minute: Some(0),
            ..Default::default()
        };
        assert!(g.validate().unwrap_err().to_string().contains("> 0"));
    }

    /// Concurrency is clamped, not trusted: 0 would deadlock the fan-out.
    #[test]
    fn clamps_zero_concurrency_up_to_one() {
        let g = EmailGmailApi {
            request_concurrency: Some(0),
            ..Default::default()
        };
        assert_eq!(g.request_concurrency(), 1);
    }

    #[test]
    fn parses_a_gmail_api_step_params_payload() {
        let cfg: EmailConfig = serde_json::from_value(serde_json::json!({
            "latchkey_settings": { "account": "thad@imbue.com" },
            "gmail": { "message_budget": 5000 },
        }))
        .unwrap();
        cfg.validate().unwrap();
        assert_eq!(
            cfg.latchkey_settings.account(),
            Some("thad@imbue.com"),
            "the account is a source-level latchkey setting, not a gmail knob",
        );
        let Some(EmailLiveMode::GmailApi(g)) = cfg.live_mode().unwrap() else {
            panic!("expected gmail mode");
        };
        assert_eq!(g.message_budget, Some(5000));
    }

    /// The account used to live under `gmail`. A config written against
    /// that location must fail with the fix rather than mirror the wrong
    /// identity (or, once `google-gmail` holds two accounts, fail deep in a
    /// download with latchkey's own ambiguity error).
    #[test]
    fn rejects_the_retired_gmail_api_account_location() {
        let cfg: EmailConfig = serde_json::from_value(serde_json::json!({
            "gmail": { "account": "thad@imbue.com" },
        }))
        .unwrap();
        let err = cfg
            .validate()
            .expect_err("the retired location must not be silently ignored")
            .to_string();
        assert!(err.contains("latchkey_settings.account"), "{err}");
        assert!(err.contains("thad@imbue.com"), "{err}");
    }

    /// An empty account is never what anyone means: latchkey's unnamed
    /// default account is addressed by omitting the field.
    #[test]
    fn rejects_an_empty_latchkey_account() {
        let cfg: EmailConfig = serde_json::from_value(serde_json::json!({
            "latchkey_settings": { "account": "  " },
            "gmail": {},
        }))
        .unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn rejects_unknown_gmail_api_keys() {
        let err = serde_json::from_value::<EmailGmailApi>(serde_json::json!({
            "full_resynk": true,
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("full_resynk"), "{err}");
    }

    #[test]
    fn selects_each_live_mode_on_its_own() {
        let jmap = EmailConfig {
            jmap: Some(EmailSync::default()),
            ..Default::default()
        };
        assert!(matches!(
            jmap.live_mode().unwrap(),
            Some(EmailLiveMode::Jmap(_))
        ));

        let gmail = EmailConfig {
            gmail: Some(EmailGmailApi::default()),
            ..Default::default()
        };
        assert!(matches!(
            gmail.live_mode().unwrap(),
            Some(EmailLiveMode::GmailApi(_))
        ));
    }

    /// No live block is not an error — it's the mbox case, or a config
    /// `datalib-step` will refuse for naming no method at all; neither
    /// is this crate's call.
    #[test]
    fn no_live_block_is_not_an_error() {
        assert!(EmailConfig::default().live_mode().unwrap().is_none());
        assert!(EmailConfig::default().validate().is_ok());
        let mbox: EmailConfig = serde_json::from_value(serde_json::json!({
            "mbox": { "path": "~/Takeout/Mail/All mail.mbox" },
        }))
        .unwrap();
        mbox.validate().unwrap();
        assert!(mbox.mbox.unwrap().path().ends_with("All mail.mbox"));
    }
}
