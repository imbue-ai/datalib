//! "Check connection" for a contacts source — do these credentials
//! reach the account? — and the address books `addressbooks` can name.
//! Finding the account is the address-book listing, so a check runs it
//! too and keeps only the account. No card is fetched.

use anyhow::{anyhow, bail, Result};
use datalib_etl_contacts_config::{ContactsConfig, ContactsMethod};
use datalib_probe::{ProbeAccount, ProbeAsk, ProbeItem, ProbeItemKind, ProbeList, ProbeReport};

use crate::ingest::{self, FetchSummary};

pub async fn probe(config: &ContactsConfig, ask: ProbeAsk) -> Result<ProbeReport> {
    let mut report = reach(config).await?;
    match ask {
        ProbeAsk::Account => {
            report.items.clear();
            report.notes.clear();
        }
        ProbeAsk::List(ProbeList::Addressbooks) => {}
        ProbeAsk::List(other) => bail!("a contacts source has no `{}` list", other.as_str()),
    }
    Ok(report)
}

async fn reach(config: &ContactsConfig) -> Result<ProbeReport> {
    config.validate()?;
    match config.method()? {
        ContactsMethod::Carddav { server_url, .. } => {
            let mut summary = FetchSummary::default();
            let reached =
                ingest::reach(server_url, &mut summary, &config.latchkey_settings).await?;
            let method = if config.fastmail.is_some() {
                "fastmail"
            } else {
                "carddav"
            };
            let names = reached.books.into_iter().map(|b| b.display_name);
            Ok(report(
                method,
                login_from_principal(&reached.principal_url),
                names,
            ))
        }
        ContactsMethod::Vcf(_) => Err(anyhow!(
            "a `vcf` source reads files on disk, so there is no connection to test"
        )),
    }
}

/// The principal's last path segment, which is the login on the
/// servers that name one there (Fastmail's
/// `/dav/principals/user/<address>/`, iCloud's numeric id).
fn login_from_principal(principal_url: &str) -> Option<String> {
    let after_scheme = principal_url
        .split_once("://")
        .map_or(principal_url, |(_, r)| r);
    let (_, path) = after_scheme.split_once('/')?;
    let last = path.trim_end_matches('/').rsplit('/').next()?;
    let login = last.replace("%40", "@");
    (!login.is_empty()).then_some(login)
}

/// One item per named address book, sorted by name. An address book
/// with no `displayname` cannot be named by the filter, so it is left
/// out of the picker and counted in a note instead.
fn report(
    method: &str,
    login: Option<String>,
    names: impl Iterator<Item = Option<String>>,
) -> ProbeReport {
    let mut unnamed = 0;
    let mut items: Vec<ProbeItem> = names
        .filter_map(|n| {
            if n.is_none() {
                unnamed += 1;
            }
            n
        })
        .map(|n| ProbeItem::new(n, ProbeItemKind::AddressBook))
        .collect();
    items.sort_by_key(|i| i.path.to_lowercase());
    let notes = if unnamed > 0 {
        vec![format!(
            "{unnamed} address book(s) have no name, so the filter cannot pick them; \
             leave it empty to mirror them."
        )]
    } else {
        Vec::new()
    };
    ProbeReport {
        mode: method.to_string(),
        account: ProbeAccount {
            id: login.clone().unwrap_or_default(),
            address: login.filter(|l| l.contains('@')),
            display_name: None,
            message_estimate: None,
        },
        items,
        notes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn items_are_named_the_way_the_filter_matches() {
        let r = report(
            "fastmail",
            login_from_principal(
                "https://carddav.enterprise.test/dav/principals/user/picard%40enterprise.test/",
            ),
            vec![
                Some("Bridge".to_string()),
                None,
                Some("away team".to_string()),
            ]
            .into_iter(),
        );
        let paths: Vec<&str> = r.items.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, vec!["away team", "Bridge"]);
        assert_eq!(r.account.address.as_deref(), Some("picard@enterprise.test"));
        assert_eq!(
            r.notes.len(),
            1,
            "the unnamed book is mentioned, not listed"
        );
    }

    #[test]
    fn a_principal_with_no_login_in_it_names_none() {
        assert_eq!(login_from_principal("https://example.test/"), None);
    }
}
