//! Provider-owned config schema for the `contacts` source (Program A
//! goal #1). Schema-only (serde + anyhow), so the orchestrator can name
//! `CarddavConfig` without linking the provider. The crate keeps its
//! protocol name until the mechanical rename; the type is `contacts`.

use datalib_source_common::{LatchkeySettings, LocalPath, SourceCommon};
use serde::{Deserialize, Serialize};

/// The contacts-owned slice of a `contacts` source. Two ways in:
/// `carddav` mirrors a live CardDAV server, `vcf` ingests `.vcf` exports
/// under a directory on disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CarddavConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    /// Which latchkey identity this source mirrors. Composed only by the
    /// providers that authenticate through the `latchkey` CLI, and
    /// forwarded whole to the download client — see [`LatchkeySettings`].
    #[serde(default)]
    pub latchkey_settings: LatchkeySettings,
    #[serde(default)]
    pub carddav: Option<CarddavSync>,
    /// A directory of `.vcf` files (a Google or Fastmail export).
    #[serde(default)]
    pub vcf: Option<LocalPath>,
}

impl CarddavConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.latchkey_settings
            .validate()
            .map_err(anyhow::Error::msg)?;
        if self.carddav.is_some() && self.vcf.is_some() {
            anyhow::bail!(
                "contacts sets both `carddav` and `vcf` — pick one. To mirror the same \
                 contacts two ways, declare two sources."
            );
        }
        Ok(())
    }
}

/// Tunables for the CardDAV server path (Apple, Fastmail, Google
/// contacts — see `datalib_etl_contacts`).
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

/// Params for the render step — no provider-specific render knobs, so
/// this is the shared bare envelope (see the per-phase params split).
pub type CarddavRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for CarddavConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] = &[
        datalib_source_common::IngestMethod::origin("carddav"),
        datalib_source_common::IngestMethod::local("vcf"),
    ];
}
