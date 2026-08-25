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
/// * `imap:` → live IMAP server (Gmail, iCloud, Dovecot, …);
/// * neither, plus an `.mbox` at `common.input_path` → file-backed mbox
///   mode (e.g. a Google Takeout export).
///
/// Setting both `sync:` and `imap:` is a config error — see
/// [`EmailConfig::live_mode`]. All three paths live in `datalib_etl_email`
/// and write the same raw schema, so render is mode-agnostic.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmailConfig {
    /// Shared per-source envelope (paths + cross-source tunables).
    #[serde(default)]
    pub common: SourceCommon,
    /// JMAP sync knobs. `Some` selects the JMAP live-server download path.
    #[serde(default)]
    pub sync: Option<EmailSync>,
    /// IMAP sync knobs. `Some` selects the IMAP live-server download path.
    /// Mutually exclusive with [`sync`](Self::sync).
    #[serde(default)]
    pub imap: Option<EmailImap>,
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
    Imap(&'a EmailImap),
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
        match (&self.sync, &self.imap) {
            (Some(_), Some(_)) => anyhow::bail!(
                "email source sets both `sync` (JMAP) and `imap` — pick one. \
                 To mirror the same account over both, declare two sources."
            ),
            (Some(s), None) => Ok(Some(EmailLiveMode::Jmap(s))),
            (None, Some(i)) => Ok(Some(EmailLiveMode::Imap(i))),
            (None, None) => Ok(None),
        }
    }

    /// Provider-local validation, run by the step planner at config load.
    /// The mbox-vs-`input_path` check lives in the builder, which has the
    /// resolved paths; what can be checked from the schema alone is that
    /// at most one download mode is selected and that each one's own
    /// fields hang together.
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(EmailLiveMode::Imap(imap)) = self.live_mode()? {
            imap.validate()?;
        }
        Ok(())
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

/// IMAP sync tunables. Mirrors the `imap:` sub-stanza of a `type: email`
/// source.
///
/// The transport is generic RFC 3501 + whatever extensions the server
/// advertises; Gmail is the first target but nothing here is Gmail-only.
/// Gmail-shaped behavior (the `\All` folder holding one copy of every
/// message, `X-GM-LABELS` carrying label membership) is selected at
/// runtime off the `X-GM-EXT-1` capability, not from config.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmailImap {
    /// IMAP server hostname, e.g. `imap.gmail.com`, `imap.mail.me.com`.
    pub host: String,
    /// Implicit-TLS port. Defaults to 993; there is no STARTTLS-on-143
    /// path, because every server worth mirroring offers 993 and
    /// negotiating up from cleartext is a downgrade surface we don't need.
    #[serde(default)]
    pub port: Option<u16>,
    /// Name of the **latchkey service** holding this account's
    /// credential — not the credential itself. The password never appears
    /// in this file.
    ///
    /// ```sh
    /// latchkey services register gmail-imap --base-api-url="https://imap.gmail.com/"
    /// latchkey auth set gmail-imap -u "you@gmail.com:$(pbpaste)"
    /// ```
    ///
    /// A `-u user:pass` credential authenticates with SASL PLAIN; an
    /// `Authorization: Bearer` one authenticates with XOAUTH2. See
    /// `datalib_etl::latchkey::extract_credential`.
    pub latchkey_service: String,
    /// Stable id for the synthesized `accounts` row. Defaults to the
    /// credential's username (which for Gmail is the address).
    #[serde(default)]
    pub account_id: Option<String>,
    /// Display name for the `accounts` row. Defaults to `account_id`.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Canonical address for the `accounts` row. Defaults to the
    /// credential's username.
    #[serde(default)]
    pub email_address: Option<String>,
    /// Folder holding the canonical single copy of every message.
    /// Normally left unset: we discover it from the RFC 6154 `\All`
    /// special-use flag, which is locale-safe (Gmail localizes the
    /// display name of `[Gmail]/All Mail`). Set it only for a server that
    /// has such a folder but doesn't flag it.
    #[serde(default)]
    pub all_mail_folder: Option<String>,
    /// Folders to mirror when the server has no `\All` folder. Empty =
    /// every folder the server lists. Ignored when an all-mail folder is
    /// in play, since that one already contains everything.
    #[serde(default)]
    pub folders: Vec<String>,
    /// Discard the stored UID/MODSEQ cursors and re-enumerate. Applies to
    /// this run only; the next run resumes incrementally from the
    /// post-resync state.
    #[serde(default)]
    pub full_resync: bool,
    /// How many IMAP connections to keep open. `None` uses the built-in
    /// default. Kept deliberately small: IMAP connections are expensive
    /// to establish and servers count them (Gmail allows 15 simultaneous
    /// per account, shared with every other client the user is running).
    #[serde(default)]
    pub connection_concurrency: Option<usize>,
    /// Stop fetching message bodies once this many bytes have been pulled
    /// in one run, commit the cursor, and exit **successfully** with a
    /// partial result.
    ///
    /// This exists because Gmail caps IMAP at 2500 MB of downloads per
    /// day and throttles the account past that rather than failing
    /// cleanly. A large mailbox is therefore a multi-day backfill no
    /// matter what, and the honest way to model that is a run that stops
    /// early and says so — not one that fails and poisons the DAG
    /// subtree. `None` uses the built-in default.
    #[serde(default)]
    pub daily_download_budget_bytes: Option<u64>,
}

/// Default implicit-TLS IMAP port (RFC 8314).
pub const DEFAULT_IMAP_PORT: u16 = 993;

impl EmailImap {
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(DEFAULT_IMAP_PORT)
    }

    /// Schema-local checks. Both fields are `String` rather than
    /// `Option<String>`, so serde already rejects a missing one; what's
    /// left is rejecting a present-but-empty one, which serde won't.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.host.trim().is_empty() {
            anyhow::bail!("email `imap.host` is required (e.g. \"imap.gmail.com\")");
        }
        if self.latchkey_service.trim().is_empty() {
            anyhow::bail!(
                "email `imap.latchkey_service` is required — it names the latchkey \
                 service holding this account's credential, e.g. \"gmail-imap\""
            );
        }
        // A URL here means someone pasted the latchkey `--base-api-url`
        // into the wrong field; it would fail much later with a DNS error.
        if self.host.contains("://") || self.host.contains('/') {
            anyhow::bail!(
                "email `imap.host` should be a bare hostname, not a URL (got {:?})",
                self.host
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imap(host: &str) -> EmailImap {
        EmailImap {
            host: host.to_string(),
            latchkey_service: "gmail-imap".to_string(),
            ..Default::default()
        }
    }

    /// The whole reason mode selection became explicit: with three modes,
    /// inferring from `sync:` alone would silently pick one.
    #[test]
    fn rejects_both_live_modes_at_once() {
        let cfg = EmailConfig {
            sync: Some(EmailSync::default()),
            imap: Some(imap("imap.gmail.com")),
            ..Default::default()
        };
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("both"), "unhelpful message: {err}");
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

        let im = EmailConfig {
            imap: Some(imap("imap.gmail.com")),
            ..Default::default()
        };
        assert!(matches!(
            im.live_mode().unwrap(),
            Some(EmailLiveMode::Imap(_))
        ));
    }

    /// No live block is not an error — it's the mbox / render-only case,
    /// which the provider resolves by probing `input_path`.
    #[test]
    fn no_live_block_is_not_an_error() {
        assert!(EmailConfig::default().live_mode().unwrap().is_none());
        assert!(EmailConfig::default().validate().is_ok());
    }

    #[test]
    fn defaults_to_the_implicit_tls_port() {
        assert_eq!(imap("imap.gmail.com").port(), 993);
        assert_eq!(
            EmailImap {
                port: Some(1993),
                ..imap("localhost")
            }
            .port(),
            1993
        );
    }

    /// A pasted `--base-api-url` in `host` would otherwise surface as a
    /// DNS failure deep inside the connect path.
    #[test]
    fn rejects_a_url_in_the_host_field() {
        for bad in ["https://imap.gmail.com/", "imap.gmail.com/"] {
            let err = imap(bad).validate().unwrap_err().to_string();
            assert!(err.contains("bare hostname"), "for {bad:?}: {err}");
        }
    }

    #[test]
    fn requires_a_latchkey_service_name() {
        let cfg = EmailImap {
            latchkey_service: "  ".to_string(),
            ..imap("imap.gmail.com")
        };
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("latchkey_service"));
    }

    /// `deny_unknown_fields` is what turns a typo'd knob into an error at
    /// config load instead of a silently ignored setting.
    #[test]
    fn rejects_unknown_imap_keys() {
        let err = serde_json::from_value::<EmailImap>(serde_json::json!({
            "host": "imap.gmail.com",
            "latchkey_service": "gmail-imap",
            "full_resynk": true,
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("full_resynk"), "{err}");
    }

    /// The `imap` block has to survive the same serde round-trip the step
    /// planner puts it through (`--params` JSON → EmailConfig).
    #[test]
    fn parses_from_a_step_params_payload() {
        let cfg: EmailConfig = serde_json::from_value(serde_json::json!({
            "imap": {
                "host": "imap.gmail.com",
                "latchkey_service": "gmail-imap",
                "daily_download_budget_bytes": 2_000_000_000u64,
            },
            "only_extract_labels": ["Inbox"],
        }))
        .unwrap();
        cfg.validate().unwrap();
        let Some(EmailLiveMode::Imap(i)) = cfg.live_mode().unwrap() else {
            panic!("expected imap mode");
        };
        assert_eq!(i.host, "imap.gmail.com");
        assert_eq!(i.daily_download_budget_bytes, Some(2_000_000_000));
        assert_eq!(cfg.only_extract_labels, vec!["Inbox".to_string()]);
    }
}
