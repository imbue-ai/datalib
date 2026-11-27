//! Adding many people at once, as when the rows of an address book are
//! selected: one commit, and a person who is here already is not added
//! again.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use datalib_handle::Handle;
use datalib_time::IsoOffsetTimestamp;
use serde::Serialize;

use crate::review::Skipped;
use crate::{
    holders, insert_contact, insert_handle, link_in, names_of, ContactKind, Maker, Store, WhenHeld,
};
use datalib_sql::sql;

#[derive(Debug, Clone)]
pub struct NewPerson {
    pub name: String,
    pub handles: Vec<Handle>,
    /// Handles other contacts have, which the person said are this one's
    /// too or this one's alone. They never make the person one of those
    /// contacts, as one in `handles` would.
    pub held: Vec<(Handle, WhenHeld)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct AddedPeople {
    /// New contacts.
    pub created: usize,
    /// Contacts one of the person's handles was on, which got the rest.
    pub added_to: usize,
    /// People all of whose handles were on one contact already.
    pub already: usize,
    pub skipped: Vec<Skipped>,
}

/// What adding a person comes to, by which contacts hold their handles.
#[derive(Debug, PartialEq, Eq)]
enum Plan {
    Create,
    AddTo(String),
    /// Their handles are on several contacts, and nothing says which of
    /// them they are.
    OnSeveral(Vec<String>),
}

fn plan(holders: BTreeSet<String>) -> Plan {
    let mut holders: Vec<String> = holders.into_iter().collect();
    match holders.len() {
        0 => Plan::Create,
        1 => Plan::AddTo(holders.remove(0)),
        _ => Plan::OnSeveral(holders),
    }
}

impl Store {
    /// Each of `people` a contact, with the handles nobody has. One whose
    /// handle a contact has is that contact, and gets the rest of their
    /// handles, less any taken off it as not theirs. One with no handle
    /// is told apart by name alone.
    pub async fn add_people(&self, people: &[NewPerson]) -> Result<AddedPeople> {
        let (now, tz) = IsoOffsetTimestamp::now_local().to_utc_and_offset();
        let mut out = AddedPeople::default();
        let mut tx = self.pool.begin().await?;
        for person in people {
            let name = person.name.trim();
            let mut free: Vec<&Handle> = Vec::new();
            let mut holders_of_theirs = BTreeSet::new();
            for h in &person.handles {
                let held = holders(&mut *tx, h).await?;
                if held.is_empty() && !free.contains(&h) {
                    free.push(h);
                }
                holders_of_theirs.extend(held);
            }
            let into = match plan(holders_of_theirs) {
                Plan::OnSeveral(ids) => {
                    out.skipped.push(Skipped {
                        what: name.to_string(),
                        why: format!("their handles are on {}", names_of(&mut tx, &ids).await?),
                    });
                    continue;
                }
                Plan::AddTo(id) => {
                    let mut theirs = Vec::new();
                    for h in free {
                        if !turned_down(&mut tx, h, &id).await? {
                            theirs.push(h);
                        }
                    }
                    free = theirs;
                    if free.is_empty() && person.held.is_empty() {
                        out.already += 1;
                        continue;
                    }
                    out.added_to += 1;
                    id
                }
                Plan::Create if name.is_empty() => {
                    out.skipped.push(Skipped {
                        what: person.handles.first().map_or("", Handle::as_str).into(),
                        why: "a contact needs a name".into(),
                    });
                    continue;
                }
                Plan::Create
                    if free.is_empty()
                        && person.held.is_empty()
                        && is_a_name(&mut tx, name).await? =>
                {
                    out.already += 1;
                    continue;
                }
                Plan::Create => {
                    out.created += 1;
                    let kind = ContactKind::Person;
                    insert_contact(&mut tx, name, kind, Maker::Person, &now, &tz).await?
                }
            };
            for h in free {
                insert_handle(&mut tx, h, &into, Maker::Person, &now, &tz).await?;
            }
            for (h, when) in &person.held {
                link_in(&mut tx, h, &into, name, *when, &now, &tz).await?;
            }
        }
        tx.commit().await?;
        if out.created + out.added_to > 0 {
            self.seal(&format!(
                "contacts: add {} people, and handles to {}",
                out.created, out.added_to
            ))
            .await?;
        }
        Ok(out)
    }
}

/// Whether the person took `handle` off this contact as not theirs.
async fn turned_down(
    conn: &mut sqlx::SqliteConnection,
    handle: &Handle,
    contact_id: &str,
) -> Result<bool> {
    let row: Option<i64> = sql!(
        "contacts_app.is_handle_turned_down",
        "SELECT 1 FROM not_linked WHERE handle = ? AND contact_id = ?"
    )
    .query_scalar()
    .bind(handle.as_str())
    .bind(contact_id)
    .fetch_optional(&mut *conn)
    .await
    .context("read whether a handle was taken off")?;
    Ok(row.is_some())
}

async fn is_a_name(conn: &mut sqlx::SqliteConnection, name: &str) -> Result<bool> {
    let row: Option<i64> = sql!(
        "contacts_app.find_contact_by_name",
        "SELECT 1 FROM contacts WHERE merged_into IS NULL AND name = ? COLLATE NOCASE LIMIT 1"
    )
    .query_scalar()
    .bind(name)
    .fetch_optional(&mut *conn)
    .await
    .context("look for a contact by name")?;
    Ok(row.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store_path;

    fn person(name: &str, handles: &[&str]) -> NewPerson {
        NewPerson {
            name: name.into(),
            handles: handles.iter().map(|h| Handle::parse(h).unwrap()).collect(),
            held: Vec::new(),
        }
    }

    /// Many people in one go, and in one commit: a new one is a contact
    /// with every handle given; one a contact already has gives it the
    /// rest; adding the same people again changes nothing.
    #[tokio::test]
    async fn adding_many_people_adds_each_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&store_path(dir.path())).await.unwrap();
        let riker = store
            .create(
                "Will Riker",
                ContactKind::Person,
                &[Handle::parse("email:riker@enterprise.org").unwrap()],
            )
            .await
            .unwrap();
        let spam = Handle::parse("email:will@risa.test").unwrap();
        store.link(&spam, &riker, WhenHeld::Refuse).await.unwrap();
        store.unlink(&spam, &riker).await.unwrap();

        let people = [
            person(
                "William T. Riker",
                &[
                    "email:riker@enterprise.org",
                    "tel:+12025550102",
                    "email:will@risa.test",
                ],
            ),
            person(
                "Jean-Luc Picard",
                &["email:picard@enterprise.org", "tel:+12025550101"],
            ),
            // The second address book's card for him.
            person(
                "Captain Picard",
                &["tel:+12025550101", "email:jean-luc@labarre.test"],
            ),
            person("Kalita", &[]),
            person("", &["email:drone@borg.test"]),
            person(
                "The Captains",
                &["email:riker@enterprise.org", "email:picard@enterprise.org"],
            ),
        ];
        let got = store.add_people(&people).await.unwrap();
        assert_eq!((got.created, got.added_to, got.already), (2, 2, 0));
        let skipped: Vec<(&str, &str)> = got
            .skipped
            .iter()
            .map(|s| (s.what.as_str(), s.why.as_str()))
            .collect();
        assert_eq!(
            skipped[0],
            ("email:drone@borg.test", "a contact needs a name")
        );
        assert_eq!(skipped[1].0, "The Captains");
        assert!(
            skipped[1].1.contains("Will Riker") && skipped[1].1.contains("Jean-Luc Picard"),
            "{skipped:?}"
        );

        let listed = store.list().await.unwrap();
        let held: Vec<(&str, Vec<&str>)> = listed
            .iter()
            .map(|c| {
                let handles = c.handles.iter().filter_map(|h| h.handle.as_ref());
                (c.name().unwrap(), handles.map(Handle::as_str).collect())
            })
            .collect();
        assert_eq!(
            held,
            [
                (
                    "Jean-Luc Picard",
                    vec![
                        "email:jean-luc@labarre.test",
                        "email:picard@enterprise.org",
                        "tel:+12025550101"
                    ]
                ),
                ("Kalita", vec![]),
                (
                    "Will Riker",
                    vec!["email:riker@enterprise.org", "tel:+12025550102"]
                ),
            ],
            "the address taken off Riker stays off"
        );

        let again = store.add_people(&people).await.unwrap();
        assert_eq!((again.created, again.added_to, again.already), (0, 0, 4));
        assert_eq!(store.list().await.unwrap(), listed);
        store.close().await;
    }

    /// Adding someone from their page with an address another contact has,
    /// which the person said they share: a new contact beside the other,
    /// not more handles for it.
    #[tokio::test]
    async fn a_shared_handle_adds_a_new_person_beside_its_holder() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&store_path(dir.path())).await.unwrap();
        let vineyard = Handle::parse("email:vineyard@labarre.earth").unwrap();
        let robert = store
            .create(
                "Robert Picard",
                ContactKind::Person,
                std::slice::from_ref(&vineyard),
            )
            .await
            .unwrap();
        let mut marie = person("Marie Picard", &["email:marie@labarre.earth"]);
        marie.held = vec![(vineyard.clone(), WhenHeld::Share)];
        let got = store.add_people(&[marie]).await.unwrap();
        assert_eq!((got.created, got.added_to), (1, 0), "{got:?}");
        let names: Vec<Vec<String>> = store
            .resolve(&[vineyard])
            .await
            .unwrap()
            .into_values()
            .map(|cs| cs.into_iter().map(|c| c.names[0].clone()).collect())
            .collect();
        assert_eq!(names, [["Marie Picard", "Robert Picard"]]);
        let robert = store.contact(&robert).await.unwrap().unwrap();
        assert_eq!(robert.handles.len(), 1, "Marie's own address is hers");
        store.close().await;
    }
}
