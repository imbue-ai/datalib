//! What the search reads of the contacts store without being its writer:
//! a detached read of `main`'s head (`doltlite_raw::open_reader`), so it
//! takes no lock the `datalib_contacts` applet needs. It answers a
//! `contact:` value with the handles it reaches, and the people
//! suggestions with the contacts whose name holds what was typed.

use std::collections::{BTreeSet, VecDeque};
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::doltlite_raw;

use crate::{search_in, store_path, ContactSummary};

pub struct ContactsReader {
    reader: doltlite_raw::Reader,
}

impl ContactsReader {
    /// `None` when the root has no contacts store, or nothing committed
    /// in one yet.
    pub async fn open(data_root: &Path) -> Result<Option<Self>> {
        let path = store_path(data_root);
        if !path.is_file() {
            return Ok(None);
        }
        Ok(doltlite_raw::open_reader(&path, None)
            .await?
            .map(|reader| Self { reader }))
    }

    pub async fn close(self) {
        self.reader.close().await;
    }

    /// Every handle `contact_id` reaches: its own; those of the contact
    /// it was merged into and of any merged into it, one person either
    /// way; and each member's when it is a group, a member that is a group
    /// included. Each contact is read once, however they loop. Stopped
    /// handles count: they still name the person in what they sent.
    /// `None` for a contact the store does not have.
    pub async fn handles(&self, contact_id: &str) -> Result<Option<BTreeSet<String>>> {
        let pool = self.reader.pool();
        let known: Option<String> =
            sqlx::query_scalar("SELECT contact_id FROM contacts WHERE contact_id = ?")
                .bind(contact_id)
                .fetch_optional(pool)
                .await
                .context("read a contact")?;
        if known.is_none() {
            return Ok(None);
        }
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut handles: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<String> = VecDeque::from([contact_id.to_string()]);
        while let Some(id) = queue.pop_front() {
            if !seen.insert(id.clone()) {
                continue;
            }
            let own: Vec<String> =
                sqlx::query_scalar("SELECT handle FROM handles WHERE contact_id = ?")
                    .bind(&id)
                    .fetch_all(pool)
                    .await
                    .context("read a contact's handles")?;
            handles.extend(own);
            let next: Vec<String> = sqlx::query_scalar(
                "SELECT merged_into FROM contacts WHERE contact_id = ?1 AND merged_into IS NOT NULL \
                 UNION SELECT contact_id FROM contacts WHERE merged_into = ?1 \
                 UNION SELECT member_id FROM members WHERE group_id = ?1",
            )
            .bind(&id)
            .fetch_all(pool)
            .await
            .context("read who a contact reaches")?;
            queue.extend(next);
        }
        Ok(Some(handles))
    }

    /// Contacts whose name contains `q`, as the store's own search.
    pub async fn search(&self, q: &str, limit: u32) -> Result<Vec<ContactSummary>> {
        search_in(self.reader.pool(), q, limit).await
    }

    /// The commit this read is of: what a search over its answers is
    /// cached under.
    pub fn commit(&self) -> &str {
        self.reader.pin().commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContactKind, Store};
    use datalib_handle::Handle;

    fn email(s: &str) -> Handle {
        Handle::email(s).unwrap()
    }

    /// A merged-away contact reads as its survivor; a group as its own
    /// handles and every member's, a nested group's too, each contact read
    /// once however the groups loop.
    #[tokio::test]
    async fn a_contact_reaches_its_survivor_and_its_members() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Store::open(&store_path(tmp.path())).await.unwrap();
        let riker = store
            .create(
                "Will Riker",
                ContactKind::Person,
                &[email("riker@enterprise.org")],
            )
            .await
            .unwrap();
        let old = store
            .create("W. Riker", ContactKind::Person, &[email("wtr@old.org")])
            .await
            .unwrap();
        let troi = store
            .create(
                "Deanna Troi",
                ContactKind::Person,
                &[email("troi@enterprise.org")],
            )
            .await
            .unwrap();
        let couple = store
            .create(
                "Riker and Troi",
                ContactKind::Group,
                &[email("home@betazed.org")],
            )
            .await
            .unwrap();
        let crew = store
            .create("Bridge crew", ContactKind::Group, &[])
            .await
            .unwrap();
        for (sql, a, b) in [
            (
                "UPDATE contacts SET merged_into = ? WHERE contact_id = ?",
                &riker,
                &old,
            ),
            (
                "INSERT INTO members VALUES (?, ?, '2026-10-09T00:00:00Z', '+00:00')",
                &couple,
                &old,
            ),
            (
                "INSERT INTO members VALUES (?, ?, '2026-10-09T00:00:00Z', '+00:00')",
                &couple,
                &troi,
            ),
            (
                "INSERT INTO members VALUES (?, ?, '2026-10-09T00:00:00Z', '+00:00')",
                &crew,
                &couple,
            ),
            (
                "INSERT INTO members VALUES (?, ?, '2026-10-09T00:00:00Z', '+00:00')",
                &couple,
                &crew,
            ),
        ] {
            sqlx::query(sql)
                .bind(a)
                .bind(b)
                .execute(&store.pool)
                .await
                .unwrap();
        }
        doltlite_raw::commit_run(&store.pool, "merge and groups")
            .await
            .unwrap();
        store.close().await;

        let r = ContactsReader::open(tmp.path()).await.unwrap().unwrap();
        let set = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<BTreeSet<_>>();
        let both = set(&["email:riker@enterprise.org", "email:wtr@old.org"]);
        assert_eq!(
            r.handles(&old).await.unwrap(),
            Some(both.clone()),
            "a merged-away contact and its survivor are one person"
        );
        assert_eq!(r.handles(&riker).await.unwrap(), Some(both));
        assert_eq!(
            r.handles(&crew).await.unwrap(),
            Some(set(&[
                "email:home@betazed.org",
                "email:riker@enterprise.org",
                "email:troi@enterprise.org",
                "email:wtr@old.org",
            ])),
            "a group is its own handles and every member's, nested and looped"
        );
        assert_eq!(r.handles("no-such-contact").await.unwrap(), None);
        let found: Vec<String> = r
            .search("riker", 10)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert_eq!(found, ["Riker and Troi", "Will Riker"]);
        r.close().await;
    }

    #[tokio::test]
    async fn a_root_without_a_contacts_store_has_no_reader() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(ContactsReader::open(tmp.path()).await.unwrap().is_none());
    }
}
