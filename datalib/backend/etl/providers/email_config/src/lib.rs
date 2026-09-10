//! Provider-owned config schema for the `email` source — Program A goal #1
//! ("one config definition per source, adjacent to the source").

use datalib_source_common::{expand_tilde, LatchkeySettings, RenderCommon, SourceCommon};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The full config for a `type: email` source: the shared `common:` envelope
/// (paths + cross-source knobs, composed from `source_common` and resolved by
/// the orchestrator's `normalize()`) plus everything email-specific. `name`
/// and `enabled` stay orchestrator-owned and are NOT here.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// Gmail REST API knobs. `Some` selects the Gmail API download path.
    /// Mutually exclusive with the other two.
    #[serde(default)]
    pub gmail_api: Option<EmailGmailApi>,
    /// The mbox path: where the `.mbox` is, plus the account row to
    /// synthesize for it (JMAP and Gmail learn that from the server).
    #[serde(default)]
    pub mbox: Option<MboxSync>,
    /// Legacy location of the outlink format — the knob now lives on
    /// the render step's params ([`EmailRenderConfig::outlink_format`]).
    /// Still parsed here so old-format configs migrate losslessly; the
    /// download planner rejects it with a pointer to the new home.
    #[serde(default)]
    pub outlink_format: Option<EmailOutlink>,
    /// Limit **extraction** to messages under *any* of these mailboxes,
    /// matched on the full label path (POSIX-like, e.g. `Work/Projects`)
    /// — nested labels must be listed explicitly. Several labels union,
    /// they do not intersect. Empty = download everything. Applies to
    /// every download mode. Independent of the render step's
    /// `only_render_labels`.
    #[serde(default)]
    pub only_extract_labels: Vec<String>,
    /// Legacy location of the render-label filter — now on the render
    /// step's params ([`EmailRenderConfig::only_render_labels`]). Parsed
    /// for migration; rejected by the download planner.
    #[serde(default)]
    pub only_render_labels: Vec<String>,
}

/// Params for the email **render** step. Split from [`EmailConfig`]
/// (the download-step params) so each step's params carry only what
/// that wave reads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
        if let Some(g) = &self.gmail_api {
            selected.push(("gmail_api", EmailLiveMode::GmailApi(g)));
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
    /// at most one download mode is selected and each one's own fields
    /// hang together.
    pub fn validate(&self) -> anyhow::Result<()> {
        self.latchkey_settings
            .validate()
            .map_err(anyhow::Error::msg)?;
        let live = self.live_mode()?;
        if live.is_some() && self.mbox.is_some() {
            anyhow::bail!(
                "email source sets a live download mode and `mbox` — pick one. To mirror \
                 the same account two ways, declare two sources."
            );
        }
        match live {
            Some(EmailLiveMode::GmailApi(gmail)) => gmail.validate(),
            _ => Ok(()),
        }
    }
}

/// JMAP tunables: the `jmap` table of a `type = "email"` ingest step.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmailSync {
    /// JMAP server hostname. Session discovered at
    /// `https://<hostname>/.well-known/jmap` (e.g. `api.fastmail.com`).
    pub hostname: String,
    /// JMAP account id. Defaults to the session's mail primary account.
    #[serde(default)]
    pub account_id: Option<String>,
    /// Force full `Email/query` enumeration even if a `changes` state token
    /// is stored. Defaults to false (incremental).
    #[serde(default)]
    pub full_resync: bool,
    /// How many `.eml` blob downloads to run concurrently in the
    /// end-of-sync blob phase. JMAP has no bulk-download method — each
    /// `.eml` is one HTTP GET against the substituted `downloadUrl` — so
    /// the only lever for a large initial backfill is fetching several at
    /// once. `None` uses the built-in default
    /// ([`DEFAULT_BLOB_CONCURRENCY`](../datalib_etl_email/download/constant.DEFAULT_BLOB_CONCURRENCY.html));
    /// `1` restores the old strictly-serial behavior. Clamped to ≥ 1.
    #[serde(default)]
    pub blob_download_concurrency: Option<usize>,
}

/// The mbox download path: a `.mbox` file, or a directory holding one,
/// plus account-row data so the synthesized `accounts` row matches
/// JMAP's shape. The row fields are optional (defaults: `account_id` ←
/// mbox file stem, `display_name` ← `account_id`, `is_personal` ← true).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MboxSync {
    /// The `.mbox` file, or a directory containing one.
    pub path: PathBuf,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub email_address: Option<String>,
    #[serde(default)]
    pub is_personal: Option<bool>,
}

impl MboxSync {
    pub fn path(&self) -> PathBuf {
        expand_tilde(&self.path)
    }
}

/// How to build the "open this email in webmail" outlink. The provider that
/// owns the account picks the most robust scheme our download identifiers
/// allow (Gmail → `rfc822msgid:` search; Fastmail → `app.fastmail.com` path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmailOutlink {
    Gmail,
    Fastmail,
}

/// Gmail REST API tunables. Mirrors the `gmail_api:` sub-stanza.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmailGmailApi {
    /// **Retired** — moved to the source-level `latchkey_settings.account`,
    /// which every latchkey-backed provider now shares (and which the JMAP
    /// mode needs too, so it could not stay under `gmail_api`). Still
    /// parsed so a config written against the old location fails at load
    /// time with the fix rather than being silently ignored; see
    /// [`EmailGmailApi::validate`].
    #[serde(default)]
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
    /// Discard the stored `historyId` cursor and re-enumerate every
    /// message. Applies to this run only.
    #[serde(default)]
    pub full_resync: bool,
    /// How many `messages.get` requests to keep in flight. `None` uses
    /// the built-in default. Raising it does not raise throughput past
    /// the quota ceiling below — it only helps hide per-request latency.
    #[serde(default)]
    pub request_concurrency: Option<usize>,
    /// Client-side ceiling on Gmail API quota units spent per minute.
    #[serde(default)]
    pub quota_units_per_minute: Option<u32>,
    /// Stop after fetching this many message bodies in one run, commit
    /// the cursor, and exit **successfully** with a partial result.
    #[serde(default)]
    pub message_budget: Option<usize>,
}

/// Gmail's per-user quota is 6000 units/minute. Default below it so
/// retries and a little clock skew don't push us into 429s.
pub const DEFAULT_QUOTA_UNITS_PER_MINUTE: u32 = 5_000;
/// Quota cost of one `users.messages.get`, per Google's quota table.
/// The dominant cost of any backfill.
pub const GMAIL_UNITS_MESSAGES_GET: u32 = 20;
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
                "email `gmail_api.quota_units_per_minute` must be > 0 (omit it for the default \
                 of {DEFAULT_QUOTA_UNITS_PER_MINUTE})"
            );
        }
        if let Some(account) = &self.account {
            anyhow::bail!(
                "email `gmail_api.account` has moved to `latchkey_settings.account`, which \
                 every latchkey-backed source shares. Replace it with a sibling of \
                 `gmail_api`:\n\n    [steps.params.latchkey_settings]\n    account = \
                 {account:?}\n"
            );
        }
        Ok(())
    }
}

impl datalib_source_common::IngestMethods for EmailConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] = &[
        datalib_source_common::IngestMethod::origin("jmap"),
        datalib_source_common::IngestMethod::origin("gmail_api"),
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
            gmail_api: Some(EmailGmailApi::default()),
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
            gmail_api: Some(EmailGmailApi::default()),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("jmap"), "{err}");
        assert!(err.contains("gmail_api"), "{err}");
    }

    /// A live mode beside an mbox table is the same mistake in a
    /// different shape.
    #[test]
    fn rejects_a_live_mode_beside_an_mbox() {
        let cfg: EmailConfig = serde_json::from_value(serde_json::json!({
            "gmail_api": {},
            "mbox": { "path": "/mail.mbox" },
        }))
        .unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("mbox"), "{err}");
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
            "gmail_api": { "message_budget": 5000 },
        }))
        .unwrap();
        cfg.validate().unwrap();
        assert_eq!(
            cfg.latchkey_settings.account(),
            Some("thad@imbue.com"),
            "the account is a source-level latchkey setting, not a gmail knob",
        );
        let Some(EmailLiveMode::GmailApi(g)) = cfg.live_mode().unwrap() else {
            panic!("expected gmail_api mode");
        };
        assert_eq!(g.message_budget, Some(5000));
    }

    /// The account used to live under `gmail_api`. A config written against
    /// that location must fail with the fix rather than mirror the wrong
    /// identity (or, once `google-gmail` holds two accounts, fail deep in a
    /// download with latchkey's own ambiguity error).
    #[test]
    fn rejects_the_retired_gmail_api_account_location() {
        let cfg: EmailConfig = serde_json::from_value(serde_json::json!({
            "gmail_api": { "account": "thad@imbue.com" },
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
            "gmail_api": {},
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
            gmail_api: Some(EmailGmailApi::default()),
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
