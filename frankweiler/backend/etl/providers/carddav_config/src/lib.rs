//! Provider-owned config schema for the `carddav` source (Program A
//! goal #1). Schema-only (serde + anyhow), so the orchestrator can name
//! `CarddavConfig` without linking the provider.

use frankweiler_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

/// The carddav-owned slice of a `carddav` source. `sync:` present →
/// live CardDAV server mirror (the extract path); absent → file mode,
/// ingesting `.vcf` exports under `input_path`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CarddavConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub sync: Option<CarddavSync>,
}

impl CarddavConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Tunables for the CardDAV server path (Apple, Fastmail, Google
/// contacts — see `frankweiler_etl_contacts`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CarddavSync {
    /// Server URL. Discovery walks
    /// `current-user-principal` → `addressbook-home-set` from here.
    /// Examples:
    ///   - `https://contacts.icloud.com/`
    ///   - `https://carddav.fastmail.com/`
    ///   - `https://www.googleapis.com/carddav/v1/principals/`
    pub server_url: String,
    /// Restrict the run to the named addressbooks (matched against
    /// each addressbook's `displayname` returned in PROPFIND).
    /// `None`/missing = sync every addressbook the server lists
    /// under the principal.
    #[serde(default)]
    pub addressbooks: Option<Vec<String>>,
}
