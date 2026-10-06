//! Provider-owned config schema for the `contacts` source (Program A
//! goal #1). Schema-only (serde + anyhow), so the orchestrator can name
//! `ContactsConfig` without linking the provider. The crate keeps its
//! protocol name until the mechanical rename; the type is `contacts`.

use datalib_source_common::{LatchkeySettings, LocalPath, SourceCommon};
use serde::{Deserialize, Serialize};

/// Fastmail's CardDAV root. Its bare host answers 404 to a PROPFIND, so
/// discovery has to start here (or at `/.well-known/carddav`).
pub const FASTMAIL_CARDDAV_URL: &str = "https://carddav.fastmail.com/dav/";

/// The contacts-owned slice of a `contacts` source. Exactly one of the
/// three method tables is set: `fastmail` and `carddav` mirror a live
/// CardDAV server, `vcf` ingests `.vcf` exports under a directory on disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContactsConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    /// Which latchkey identity this source mirrors. Composed only by the
    /// providers that authenticate through the `latchkey` CLI, and
    /// forwarded whole to the download client — see [`LatchkeySettings`].
    #[serde(default)]
    pub latchkey_settings: LatchkeySettings,
    /// Fastmail, over CardDAV at [`FASTMAIL_CARDDAV_URL`] (latchkey's
    /// `fastmail-dav` service, which holds an app password).
    #[serde(default)]
    pub fastmail: Option<AddressBookSelection>,
    /// Any other CardDAV server: iCloud, Nextcloud, Radicale, ….
    #[serde(default)]
    pub carddav: Option<CarddavSync>,
    /// A directory of `.vcf` files (a Google or Fastmail export).
    #[serde(default)]
    pub vcf: Option<LocalPath>,
}

/// Which download path a source selected.
#[derive(Debug, Clone)]
pub enum ContactsMethod<'a> {
    /// CardDAV, with the server it starts discovery from. An empty
    /// `addressbooks` mirrors every address book.
    Carddav {
        server_url: &'a str,
        addressbooks: &'a [String],
    },
    Vcf(&'a LocalPath),
}

impl ContactsConfig {
    /// The one method this source holds, or why there is not exactly one.
    pub fn method(&self) -> anyhow::Result<ContactsMethod<'_>> {
        let mut held: Vec<(&str, ContactsMethod<'_>)> = Vec::new();
        if let Some(s) = &self.fastmail {
            held.push((
                "fastmail",
                ContactsMethod::Carddav {
                    server_url: FASTMAIL_CARDDAV_URL,
                    addressbooks: s.addressbooks.as_deref().unwrap_or_default(),
                },
            ));
        }
        if let Some(c) = &self.carddav {
            held.push((
                "carddav",
                ContactsMethod::Carddav {
                    server_url: &c.server_url,
                    addressbooks: c.addressbooks.as_deref().unwrap_or_default(),
                },
            ));
        }
        if let Some(p) = &self.vcf {
            held.push(("vcf", ContactsMethod::Vcf(p)));
        }
        match held.len() {
            1 => Ok(held.pop().expect("len checked").1),
            0 => anyhow::bail!(
                "a contacts source names none of `fastmail`, `carddav` (a server) or `vcf` \
                 (a directory of .vcf files)"
            ),
            _ => anyhow::bail!(
                "a contacts source sets more than one of {} — pick one. To mirror the same \
                 contacts two ways, declare two sources.",
                held.iter()
                    .map(|(name, _)| format!("`{name}`"))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        }
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        self.latchkey_settings
            .validate()
            .map_err(anyhow::Error::msg)?;
        if let ContactsMethod::Carddav { server_url, .. } = self.method()? {
            if !(server_url.starts_with("https://") || server_url.starts_with("http://")) {
                anyhow::bail!("`carddav.server_url` must be an http(s) URL, got {server_url:?}");
            }
        }
        Ok(())
    }
}

/// Which of an account's address books to mirror.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AddressBookSelection {
    /// Address book names as the server shows them (each one's
    /// `displayname`), matched exactly. Missing or empty mirrors every
    /// address book the account holds.
    #[serde(default)]
    pub addressbooks: Option<Vec<String>>,
}

/// The `carddav` table: a server, plus the same selection.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CarddavSync {
    /// Server URL. Discovery walks
    /// `current-user-principal` → `addressbook-home-set` from here, or
    /// from the host's `/.well-known/carddav` when this URL does not
    /// answer.
    /// Examples:
    ///   - `https://contacts.icloud.com/`
    ///   - `https://cloud.example.com/remote.php/dav/`
    pub server_url: String,
    /// As [`AddressBookSelection::addressbooks`].
    #[serde(default)]
    pub addressbooks: Option<Vec<String>>,
}

/// Params for the render step — no provider-specific render knobs, so
/// this is the shared bare envelope (see the per-phase params split).
pub type ContactsRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for ContactsConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] = &[
        datalib_source_common::IngestMethod::origin("fastmail"),
        datalib_source_common::IngestMethod::origin("carddav"),
        datalib_source_common::IngestMethod::local("vcf"),
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: serde_json::Value) -> ContactsConfig {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn fastmail_is_carddav_at_the_dav_root() {
        let cfg = parse(serde_json::json!({"fastmail": {"addressbooks": ["Crew"]}}));
        cfg.validate().unwrap();
        let ContactsMethod::Carddav {
            server_url,
            addressbooks,
        } = cfg.method().unwrap()
        else {
            panic!("carddav");
        };
        assert_eq!(server_url, FASTMAIL_CARDDAV_URL);
        assert_eq!(addressbooks, ["Crew"]);
    }

    #[test]
    fn an_empty_table_mirrors_every_address_book() {
        let cfg = parse(serde_json::json!({"fastmail": {}}));
        let ContactsMethod::Carddav { addressbooks, .. } = cfg.method().unwrap() else {
            panic!("carddav");
        };
        assert!(addressbooks.is_empty());
    }

    #[test]
    fn exactly_one_method() {
        assert!(parse(serde_json::json!({})).method().is_err());
        let two = parse(serde_json::json!({
            "fastmail": {},
            "carddav": {"server_url": "https://contacts.icloud.com/"}
        }));
        let err = two.method().unwrap_err().to_string();
        assert!(
            err.contains("`fastmail`") && err.contains("`carddav`"),
            "{err}"
        );
    }

    #[test]
    fn a_server_url_is_http() {
        let cfg = parse(serde_json::json!({"carddav": {"server_url": "contacts.icloud.com"}}));
        assert!(cfg.validate().is_err());
    }
}
