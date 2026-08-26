//! Provider-owned config schema for the `email` source — Program A goal #1
//! ("one config definition per source, adjacent to the source").
//!
//! This crate is **schema-only**: it depends on nothing but `serde`, so any
//! consumer that needs to *name* the email config (the orchestrator's
//! `ingest-config` oneof, the `http` backend) can do so without linking a
//! line of extraction code. The email provider crate
//! (`datalib_etl_email`) builds its [`DataProcessor`]s from these types;
//! the orchestrator deserializes them and never destructures the internals.
//!
//! During the email pilot these types are deserialized from the YAML *stanza*
//! the orchestrator already produces (`serde_yaml::to_value(source)`), so the
//! crate stays free of any dependency on `datalib_core::config`. When the
//! `ingest-config` oneof lands (Program A step 3), [`EmailConfig`] becomes the
//! variant payload directly — same type, no reparse.

use datalib_source_common::{RenderCommon, SourceCommon};
use serde::{Deserialize, Serialize};

/// The full config for a `type: email` source: the shared `common:` envelope
/// (paths + cross-source knobs, composed from `source_common` and resolved by
/// the orchestrator's `normalize()`) plus everything email-specific. `name`
/// and `enabled` stay orchestrator-owned and are NOT here.
///
/// Three download modes, at most one of which may be selected:
///
/// * `sync:` → JMAP server (Fastmail / any RFC 8620+8621 server);
/// * `gmail_api:` → Gmail REST API (the path for a Gmail account);
/// * neither, plus an `.mbox` at `common.input_path` → file-backed mbox
///   mode (e.g. a Google Takeout export).
///
/// Setting more than one live block is a config error — see
/// [`EmailConfig::live_mode`]. All three paths live in `datalib_etl_email`
/// and write the same raw schema, so render is mode-agnostic and the same
/// mailbox ingested two ways dedupes rather than doubling.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmailConfig {
    /// Shared per-source envelope (paths + cross-source tunables).
    #[serde(default)]
    pub common: SourceCommon,
    /// JMAP sync knobs. `Some` selects the JMAP live-server download path.
    #[serde(default)]
    pub sync: Option<EmailSync>,
    /// Gmail REST API knobs. `Some` selects the Gmail API download path.
    /// Mutually exclusive with the other two.
    #[serde(default)]
    pub gmail_api: Option<EmailGmailApi>,
    /// Account-row config for the mbox path (display name, address,
    /// is_personal). Ignored when `sync:` is present (JMAP carries that
    /// info itself).
    #[serde(default)]
    pub mbox: Option<MboxSync>,
    /// Legacy location of the outlink format — the knob now lives on
    /// the render step's params ([`EmailRenderConfig::outlink_format`]).
    /// Still parsed here so old-format configs migrate losslessly; the
    /// download planner rejects it with a pointer to the new home.
    #[serde(default)]
    pub outlink_format: Option<EmailOutlink>,
    /// Limit **extraction** to mailboxes whose full label path (POSIX-like,
    /// e.g. `Work/Projects`) exactly matches one of these — nested labels must
    /// be listed explicitly. Empty = download everything. Applies to both the
    /// JMAP and `.mbox` paths. Independent of the render step's
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
/// mbox mode is deliberately *not* a variant: choosing it requires probing
/// the filesystem for an `.mbox`, and this crate is schema-only. The
/// provider asks for [`EmailConfig::live_mode`] first and falls back to
/// the mbox probe when it comes back `None`.
#[derive(Debug, Clone)]
pub enum EmailLiveMode<'a> {
    Jmap(&'a EmailSync),
    GmailApi(&'a EmailGmailApi),
}

impl EmailConfig {
    /// Which live-server transport this source selected, or `None` for a
    /// file-backed source.
    ///
    /// Errors when more than one is set. Mode selection used to be
    /// inferable from `sync:` alone; with a third mode it has to be
    /// explicit, and silently preferring one over the other would mirror
    /// a mailbox the user didn't ask for.
    pub fn live_mode(&self) -> anyhow::Result<Option<EmailLiveMode<'_>>> {
        let mut selected: Vec<(&str, EmailLiveMode<'_>)> = Vec::new();
        if let Some(s) = &self.sync {
            selected.push(("sync (JMAP)", EmailLiveMode::Jmap(s)));
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

    /// Provider-local validation, run by the step planner at config load.
    /// The mbox-vs-`input_path` check lives in the builder, which has the
    /// resolved paths; what can be checked from the schema alone is that
    /// at most one download mode is selected and that each one's own
    /// fields hang together.
    pub fn validate(&self) -> anyhow::Result<()> {
        match self.live_mode()? {
            Some(EmailLiveMode::GmailApi(gmail)) => gmail.validate(),
            _ => Ok(()),
        }
    }
}

/// JMAP sync tunables. Mirrors the `sync:` sub-stanza of a `type: email`
/// source. (Named `EmailSync` rather than `JmapApiSync` because the source
/// variant covers more than the JMAP API surface.)
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

/// Account-row data for the mbox download path, so the synthesized `accounts`
/// row matches JMAP's shape. All fields optional (defaults: `account_id` ←
/// mbox file stem, `display_name` ← `account_id`, `is_personal` ← true).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MboxSync {
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub email_address: Option<String>,
    #[serde(default)]
    pub is_personal: Option<bool>,
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
///
/// The mode for a Gmail account.
///
/// * **Credentials need no configuration at all.** latchkey ships a
///   built-in `google-gmail` service and routes to it by URL host, so
///   there is no service name to name — the ordinary `latchkey curl` path
///   every other HTTP provider in this tree uses just works, refresh
///   included. Set it up once with
///   `latchkey auth browser google-gmail`.
/// * **Incremental sync is explicit.** `users.history.list` reports
///   `messagesAdded` / `messagesDeleted` / `labelsAdded` /
///   `labelsRemoved`, so deletions arrive as events rather than having to
///   be inferred by re-enumeration.
/// * **Throughput is quota-limited, not byte-limited**: ~300
///   messages/minute regardless of message size.
///
/// For a non-Gmail account, use the JMAP mode or a file export. See
/// `docs/dev/email_download_modes.md` for why IMAP was tried and dropped.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmailGmailApi {
    /// Which stored latchkey account to use, when `google-gmail` holds
    /// more than one (work + personal is the normal case). latchkey
    /// *requires* this once a service has two credentials. Omit when
    /// there is only one.
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
    ///
    /// Google's per-user limit is 6000 units/minute and `messages.get`
    /// costs 20, so the ceiling is ~300 messages/minute. The default sits
    /// below 6000 to leave headroom for retries; raising it past 6000
    /// just moves the failure from our throttle to Google's 429.
    #[serde(default)]
    pub quota_units_per_minute: Option<u32>,
    /// Stop after fetching this many message bodies in one run, commit
    /// the cursor, and exit **successfully** with a partial result.
    ///
    /// A large mailbox is a multi-run backfill: at ~300 messages/minute a
    /// 100k-message account takes about six hours. The honest way to model
    /// that is a run that stops and says how far it got, not one that
    /// fails and poisons the DAG subtree. `None` = no limit.
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
        if self.account.as_ref().is_some_and(|a| a.trim().is_empty()) {
            anyhow::bail!(
                "email `gmail_api.account` names a stored latchkey account; omit it entirely \
                 if `google-gmail` holds only one"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole reason mode selection became explicit: with more than
    /// one mode, inferring from `sync:` alone would silently pick one.
    #[test]
    fn rejects_more_than_one_live_mode() {
        let cfg = EmailConfig {
            sync: Some(EmailSync::default()),
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
            sync: Some(EmailSync::default()),
            gmail_api: Some(EmailGmailApi::default()),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("sync (JMAP)"), "{err}");
        assert!(err.contains("gmail_api"), "{err}");
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
            "gmail_api": { "account": "thad@imbue.com", "message_budget": 5000 },
        }))
        .unwrap();
        cfg.validate().unwrap();
        let Some(EmailLiveMode::GmailApi(g)) = cfg.live_mode().unwrap() else {
            panic!("expected gmail_api mode");
        };
        assert_eq!(g.account.as_deref(), Some("thad@imbue.com"));
        assert_eq!(g.message_budget, Some(5000));
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
            sync: Some(EmailSync::default()),
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

    /// No live block is not an error — it's the mbox / render-only case,
    /// which the provider resolves by probing `input_path`.
    #[test]
    fn no_live_block_is_not_an_error() {
        assert!(EmailConfig::default().live_mode().unwrap().is_none());
        assert!(EmailConfig::default().validate().is_ok());
    }
}
