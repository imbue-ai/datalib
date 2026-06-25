//! Provider-owned config schema for the `email` source — Program A goal #1
//! ("one config definition per source, adjacent to the source").
//!
//! This crate is **schema-only**: it depends on nothing but `serde`, so any
//! consumer that needs to *name* the email config (the orchestrator's
//! `ingest-config` oneof, the `http` backend) can do so without linking a
//! line of extraction code. The email provider crate
//! (`frankweiler_etl_email`) builds its [`DataProcessor`]s from these types;
//! the orchestrator deserializes them and never destructures the internals.
//!
//! During the email pilot these types are deserialized from the YAML *stanza*
//! the orchestrator already produces (`serde_yaml::to_value(source)`), so the
//! crate stays free of any dependency on `frankweiler_core::config`. When the
//! `ingest-config` oneof lands (Program A step 3), [`EmailConfig`] becomes the
//! variant payload directly — same type, no reparse.

use frankweiler_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

/// The full config for a `type: email` source: the shared `common:` envelope
/// (paths + cross-source knobs, composed from `source_common` and resolved by
/// the orchestrator's `normalize()`) plus everything email-specific. `name`
/// and `enabled` stay orchestrator-owned and are NOT here.
///
/// `sync:` present → JMAP server (Fastmail / any RFC 8620+8621 server);
/// `sync:` absent + an `.mbox` at `common.input_path` → file-backed mbox mode
/// (e.g. a Google Takeout export). Both paths live in `frankweiler_etl_email`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EmailConfig {
    /// Shared per-source envelope (paths + cross-source tunables).
    #[serde(default)]
    pub common: SourceCommon,
    /// JMAP sync knobs. `Some` selects the live-server extract path.
    #[serde(default)]
    pub sync: Option<EmailSync>,
    /// Account-row config for the mbox path (display name, address,
    /// is_personal). Ignored when `sync:` is present (JMAP carries that
    /// info itself).
    #[serde(default)]
    pub mbox: Option<MboxSync>,
    /// Webmail to build each email's `↗` outlink for. `gmail` for a Google
    /// Takeout `.mbox`, `fastmail` for a Fastmail JMAP account. Omit for any
    /// other server (no outlink).
    #[serde(default)]
    pub outlink_format: Option<EmailOutlink>,
    /// Limit **extraction** to mailboxes whose full label path (POSIX-like,
    /// e.g. `Work/Projects`) exactly matches one of these — nested labels must
    /// be listed explicitly. Empty = extract everything. Applies to both the
    /// JMAP and `.mbox` paths. Independent of `only_render_labels`.
    #[serde(default)]
    pub only_extract_labels: Vec<String>,
    /// Limit **rendering** to threads with at least one email under one of
    /// these mailbox label paths (same exact-match semantics). Empty = render
    /// everything extracted. Separate list, so a giant inbox can be extracted
    /// in full but rendered down to a subset.
    #[serde(default)]
    pub only_render_labels: Vec<String>,
}

impl EmailConfig {
    /// True when this source has a `sync:` block — i.e. the JMAP live-server
    /// extract path applies (vs. file-backed mbox).
    pub fn is_jmap(&self) -> bool {
        self.sync.is_some()
    }

    /// Provider-local validation. Email has no cross-field rules today (both
    /// modes are valid; the mbox-vs-input_path check lives in the builder,
    /// which has the resolved paths). Present so every `*-config` exposes the
    /// same `validate()` surface for the `http` schema-only validation path.
    pub fn validate(&self) -> anyhow::Result<()> {
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
}

/// Account-row data for the mbox extract path, so the synthesized `accounts`
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
/// owns the account picks the most robust scheme our extract identifiers
/// allow (Gmail → `rfc822msgid:` search; Fastmail → `app.fastmail.com` path).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmailOutlink {
    Gmail,
    Fastmail,
}
