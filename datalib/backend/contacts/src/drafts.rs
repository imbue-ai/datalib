// Editing a contact at length: what the card edits (`ContactEdit`), its
// draft on a branch of the store, and the save that publishes it with
// the draft winning only the cells it changed. The mechanism is
// `datalib_etl::draft`; why it is shaped this way is
// docs/dev/plans/contact_editing.md.

use anyhow::{bail, Context, Result};
use datalib_etl::bulk::bulk_upsert_entity_in_tx;
use datalib_etl::draft::{self, OnDraft};
use datalib_handle::Handle;
use datalib_time::IsoOffsetTimestamp;
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqliteConnection};

use crate::schema::fields::FieldRow;
use crate::{FieldKind, Store};

/// What a contact says, as its card edits it: everything but its links
/// and its photo, which change at once rather than through a draft.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContactEdit {
    pub name: String,
    pub note: Option<String>,
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    /// Minted by the card for a new field, so autosaves of it agree.
    pub field_id: String,
    pub kind: FieldKind,
    pub label: Option<String>,
    pub value: String,
    /// The value as a handle, where it is an email or a number the
    /// handle rules read. Worked out by the store; ignored on the way in.
    #[serde(default)]
    pub handle: Option<String>,
    pub copied_from: Option<CopiedFrom>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopiedFrom {
    pub source_id: String,
    pub key: String,
}

/// A contact's draft beside what it was cut from and what is published
/// now, for the card's three-way view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DraftView {
    pub base: Option<ContactEdit>,
    pub mine: Option<ContactEdit>,
    pub published: Option<ContactEdit>,
    /// What a save must name to say "I have seen this".
    pub published_commit: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Saved {
    /// Published as `commit`; `None` when the draft changed nothing.
    Saved { commit: Option<String> },
    /// The contact moved since the card last looked: nothing was saved.
    Stale { view: Box<DraftView> },
}

/// Which state of the store a read is of.
#[derive(Clone, Copy)]
enum At<'a> {
    /// The connection's own branch, uncommitted rows included.
    Here,
    Commit(&'a str),
}

fn branch(contact_id: &str) -> String {
    draft::branch_of(contact_id)
}

/// Whether `edit` can be stored, and why not.
fn check(edit: &ContactEdit) -> Result<()> {
    if edit.name.trim().is_empty() {
        bail!("a contact needs a name");
    }
    let mut ids = std::collections::HashSet::new();
    for f in &edit.fields {
        if f.field_id.trim().is_empty() {
            bail!("a field needs an id");
        }
        if !ids.insert(f.field_id.as_str()) {
            bail!("two fields share the id {}", f.field_id);
        }
    }
    Ok(())
}

/// A field's value as a handle, where its kind is one a handle can be.
fn handle_of(kind: FieldKind, value: &str) -> Option<String> {
    let h = match kind {
        FieldKind::Email => Handle::email(value),
        FieldKind::Phone => Handle::tel(value),
        _ => None,
    };
    h.map(|h| h.as_str().to_string())
}

async fn read_edit(
    conn: &mut SqliteConnection,
    at: At<'_>,
    contact_id: &str,
) -> Result<Option<ContactEdit>> {
    let contact = match at {
        At::Here => {
            sqlx::query("SELECT name, note FROM contacts WHERE contact_id = ?")
                .bind(contact_id)
                .fetch_optional(&mut *conn)
                .await
        }
        At::Commit(c) => {
            sqlx::query("SELECT name, note FROM dolt_at_contacts(?) WHERE contact_id = ?")
                .bind(c)
                .bind(contact_id)
                .fetch_optional(&mut *conn)
                .await
        }
    }
    .context("read a contact")?;
    let Some(contact) = contact else {
        return Ok(None);
    };
    const COLS: &str = "field_id, kind, label, value, handle, copied_from_source, copied_from_key";
    let rows = match at {
        At::Here => {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                // Audited: COLS is a constant.
                "SELECT {COLS} FROM fields WHERE contact_id = ? ORDER BY position, field_id"
            )))
            .bind(contact_id)
            .fetch_all(&mut *conn)
            .await
        }
        At::Commit(c) => {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                // Audited: COLS is a constant.
                "SELECT {COLS} FROM dolt_at_fields(?) WHERE contact_id = ? \
                  ORDER BY position, field_id"
            )))
            .bind(c)
            .bind(contact_id)
            .fetch_all(&mut *conn)
            .await
        }
    }
    .context("read a contact's fields")?;
    let mut fields = Vec::with_capacity(rows.len());
    for r in rows {
        let kind: String = r.get("kind");
        let Some(kind) = FieldKind::parse(&kind) else {
            bail!("a field of {contact_id} is of a kind this build does not know: {kind:?}");
        };
        let source: Option<String> = r.get("copied_from_source");
        let key: Option<String> = r.get("copied_from_key");
        fields.push(Field {
            field_id: r.get("field_id"),
            kind,
            label: r.get("label"),
            value: r.get("value"),
            handle: r.get("handle"),
            copied_from: source
                .zip(key)
                .map(|(source_id, key)| CopiedFrom { source_id, key }),
        });
    }
    Ok(Some(ContactEdit {
        name: contact.get("name"),
        note: contact.get("note"),
        fields,
    }))
}

/// Write `edit` over the contact on this connection's branch: its name
/// and note, and its fields as given, in their order.
async fn write_edit(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    contact_id: &str,
    edit: &ContactEdit,
) -> Result<()> {
    let (now, tz) = IsoOffsetTimestamp::now_local().to_utc_and_offset();
    let done = sqlx::query(
        "UPDATE contacts SET name = ?, note = ?, updated_at_utc = ?, tz_offset = ? \
          WHERE contact_id = ?",
    )
    .bind(edit.name.trim())
    .bind(
        edit.note
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty()),
    )
    .bind(&now)
    .bind(&tz)
    .bind(contact_id)
    .execute(&mut **tx)
    .await
    .context("write a contact")?;
    if done.rows_affected() == 0 {
        bail!("no contact {contact_id}");
    }
    let kept: Vec<&str> = edit.fields.iter().map(|f| f.field_id.as_str()).collect();
    let held: Vec<String> = sqlx::query_scalar("SELECT field_id FROM fields WHERE contact_id = ?")
        .bind(contact_id)
        .fetch_all(&mut **tx)
        .await
        .context("read a contact's fields")?;
    for gone in held.iter().filter(|id| !kept.contains(&id.as_str())) {
        sqlx::query("DELETE FROM fields WHERE field_id = ?")
            .bind(gone)
            .execute(&mut **tx)
            .await
            .context("drop a field")?;
    }
    let rows: Vec<FieldRow> = edit
        .fields
        .iter()
        .enumerate()
        .map(|(i, f)| FieldRow {
            field_id: f.field_id.clone(),
            contact_id: contact_id.to_string(),
            kind: f.kind,
            label: f.label.clone().filter(|l| !l.trim().is_empty()),
            value: f.value.trim().to_string(),
            handle: handle_of(f.kind, &f.value),
            position: i as i64,
            copied_from_source: f.copied_from.as_ref().map(|c| c.source_id.clone()),
            copied_from_key: f.copied_from.as_ref().map(|c| c.key.clone()),
        })
        .collect();
    bulk_upsert_entity_in_tx(tx, &rows)
        .await
        .context("write a contact's fields")
}

impl Store {
    /// The contact as published: what every reader sees.
    pub async fn edit_of(&self, contact_id: &str) -> Result<Option<ContactEdit>> {
        let mut conn = self.pool.acquire().await?;
        read_edit(&mut conn, At::Here, contact_id).await
    }

    /// The contact's draft, cut now if it has none, beside what it was cut
    /// from and what is published.
    pub async fn draft(&self, contact_id: &str) -> Result<DraftView> {
        let _held = self.drafts.lock().await;
        if self.edit_of(contact_id).await?.is_none() {
            bail!("no contact {contact_id}");
        }
        let b = branch(contact_id);
        draft::cut(&self.pool, &b).await?;
        self.view(contact_id).await
    }

    /// The contact's draft view, or `None` when it has no draft.
    pub async fn draft_view(&self, contact_id: &str) -> Result<Option<DraftView>> {
        let _held = self.drafts.lock().await;
        if !draft::exists(&self.pool, &branch(contact_id)).await? {
            return Ok(None);
        }
        self.view(contact_id).await.map(Some)
    }

    async fn view(&self, contact_id: &str) -> Result<DraftView> {
        let b = branch(contact_id);
        let base_commit = draft::base(&self.pool, &b).await?;
        let published_commit = draft::published(&self.pool).await?;
        let mut on = OnDraft::enter(&self.pool, &b).await?;
        let mine = read_edit(on.conn(), At::Here, contact_id).await;
        let base = read_edit(on.conn(), At::Commit(&base_commit), contact_id).await;
        let published = read_edit(on.conn(), At::Commit(&published_commit), contact_id).await;
        on.leave().await?;
        Ok(DraftView {
            base: base?,
            mine: mine?,
            published: published?,
            published_commit,
        })
    }

    /// Autosave: write `edit` onto the contact's draft, uncommitted.
    /// Nothing is published; a reader sees none of it until [`Store::save`].
    pub async fn autosave(&self, contact_id: &str, edit: &ContactEdit) -> Result<()> {
        check(edit)?;
        let _held = self.drafts.lock().await;
        let b = branch(contact_id);
        if !draft::exists(&self.pool, &b).await? {
            bail!("{contact_id} has no draft to save into");
        }
        let mut on = OnDraft::enter(&self.pool, &b).await?;
        let written = async {
            let mut tx = sqlx::Connection::begin(on.conn()).await?;
            write_edit(&mut tx, contact_id, edit).await?;
            tx.commit().await?;
            anyhow::Ok(())
        }
        .await;
        on.leave().await?;
        written
    }

    /// Publish the contact's draft, as one commit, and drop it — unless
    /// the contact has moved since `seen`, the published commit the card
    /// last showed: then nothing is saved and the card gets the new view.
    /// A save elsewhere in the store does not count; only a change to
    /// this contact's name, note or fields does.
    pub async fn save(&self, contact_id: &str, seen: &str) -> Result<Saved> {
        let _held = self.drafts.lock().await;
        let b = branch(contact_id);
        if !draft::exists(&self.pool, &b).await? {
            bail!("{contact_id} has no draft to save");
        }
        let published_commit = draft::published(&self.pool).await?;
        if seen != published_commit {
            let mut conn = self.pool.acquire().await?;
            // A commit this store does not have, or one from before the
            // contact had fields, reads as different: the card refetches.
            let then = read_edit(&mut conn, At::Commit(seen), contact_id)
                .await
                .ok()
                .flatten();
            let now = read_edit(&mut conn, At::Commit(&published_commit), contact_id).await?;
            drop(conn);
            if then != now {
                return Ok(Saved::Stale {
                    view: Box::new(self.view(contact_id).await?),
                });
            }
        }
        let mut on = OnDraft::enter(&self.pool, &b).await?;
        let mine = read_edit(on.conn(), At::Here, contact_id).await;
        on.leave().await?;
        let name = mine?.map_or_else(|| contact_id.to_string(), |e| e.name);
        let commit = draft::save(&self.pool, &b, &format!("contacts: edit {name:?}")).await?;
        Ok(Saved::Saved { commit })
    }

    /// Throw the contact's draft away.
    pub async fn discard(&self, contact_id: &str) -> Result<()> {
        let _held = self.drafts.lock().await;
        draft::discard(&self.pool, &branch(contact_id)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ContactKind;
    use strum::VariantArray;

    struct Fixture {
        _dir: tempfile::TempDir,
        store: Store,
        riker: String,
    }

    async fn riker() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&crate::store_path(dir.path())).await.unwrap();
        let riker = store
            .create("William Riker", ContactKind::Person, &[])
            .await
            .unwrap();
        Fixture {
            _dir: dir,
            store,
            riker,
        }
    }

    fn field(id: &str, kind: FieldKind, value: &str) -> Field {
        Field {
            field_id: id.into(),
            kind,
            label: None,
            value: value.into(),
            handle: None,
            copied_from: None,
        }
    }

    fn edit(name: &str, note: Option<&str>, fields: Vec<Field>) -> ContactEdit {
        ContactEdit {
            name: name.into(),
            note: note.map(Into::into),
            fields,
        }
    }

    async fn log(store: &Store, n: i64) -> Vec<String> {
        sqlx::query_scalar("SELECT message FROM dolt_log() LIMIT ?")
            .bind(n)
            .fetch_all(&store.pool)
            .await
            .unwrap()
    }

    #[test]
    fn strum_and_serde_spell_field_kinds_alike() {
        for k in FieldKind::VARIANTS {
            assert_eq!(serde_json::to_value(k).unwrap(), k.as_str());
            assert_eq!(FieldKind::parse(k.as_str()), Some(*k));
        }
    }

    #[tokio::test]
    async fn an_autosave_is_seen_by_nobody_until_the_save_publishes_it() {
        let f = riker().await;
        let view = f.store.draft(&f.riker).await.unwrap();
        let mine = edit(
            "Will Riker",
            Some("plays trombone"),
            vec![
                field("f1", FieldKind::Title, "First officer"),
                field("f2", FieldKind::Phone, "+1 (555) 012-3456"),
            ],
        );
        f.store.autosave(&f.riker, &mine).await.unwrap();

        let published = f.store.edit_of(&f.riker).await.unwrap().unwrap();
        assert_eq!(published.name, "William Riker", "not published yet");
        assert!(published.fields.is_empty());
        let resolved = f.store.contact(&f.riker).await.unwrap().unwrap();
        assert_eq!(resolved.names, ["William Riker"]);

        let saved = f
            .store
            .save(&f.riker, &view.published_commit)
            .await
            .unwrap();
        assert!(
            matches!(saved, Saved::Saved { commit: Some(_) }),
            "{saved:?}"
        );
        let now = f.store.edit_of(&f.riker).await.unwrap().unwrap();
        assert_eq!(now.name, "Will Riker");
        assert_eq!(now.note.as_deref(), Some("plays trombone"));
        assert_eq!(
            now.fields
                .iter()
                .map(|f| (f.field_id.as_str(), f.value.as_str(), f.handle.as_deref()))
                .collect::<Vec<_>>(),
            [
                ("f1", "First officer", None),
                ("f2", "+1 (555) 012-3456", Some("tel:+15550123456"))
            ],
            "a number field carries its handle, and links nothing"
        );
        assert!(f
            .store
            .resolve(&[Handle::tel("+15550123456").unwrap()])
            .await
            .unwrap()
            .is_empty());
        assert_eq!(log(&f.store, 1).await, ["contacts: edit \"Will Riker\""]);
    }

    #[tokio::test]
    async fn the_view_holds_what_the_draft_was_cut_from_what_it_says_and_what_is_published() {
        let f = riker().await;
        f.store.draft(&f.riker).await.unwrap();
        f.store
            .autosave(&f.riker, &edit("Will Riker", None, vec![]))
            .await
            .unwrap();
        f.store.rename(&f.riker, "Number One").await.unwrap();

        let view = f.store.draft(&f.riker).await.unwrap();
        assert_eq!(view.base.unwrap().name, "William Riker");
        assert_eq!(view.mine.unwrap().name, "Will Riker");
        assert_eq!(view.published.unwrap().name, "Number One");
    }

    /// Guards the stale check: a save after a change the card never saw
    /// would overwrite it silently.
    #[tokio::test]
    async fn a_save_after_an_unseen_change_to_the_contact_is_refused_with_the_new_view() {
        let f = riker().await;
        let view = f.store.draft(&f.riker).await.unwrap();
        f.store
            .autosave(&f.riker, &edit("Will Riker", None, vec![]))
            .await
            .unwrap();
        f.store.rename(&f.riker, "Number One").await.unwrap();

        let Saved::Stale { view: newer } = f
            .store
            .save(&f.riker, &view.published_commit)
            .await
            .unwrap()
        else {
            panic!("saved over a change it had not seen");
        };
        assert_eq!(
            f.store.edit_of(&f.riker).await.unwrap().unwrap().name,
            "Number One"
        );

        // Saving again, having seen it, the draft wins the name.
        let saved = f
            .store
            .save(&f.riker, &newer.published_commit)
            .await
            .unwrap();
        assert!(
            matches!(saved, Saved::Saved { commit: Some(_) }),
            "{saved:?}"
        );
        assert_eq!(
            f.store.edit_of(&f.riker).await.unwrap().unwrap().name,
            "Will Riker"
        );
    }

    #[tokio::test]
    async fn a_change_to_another_contact_does_not_make_the_draft_stale() {
        let f = riker().await;
        let troi = f
            .store
            .create("Deanna Troi", ContactKind::Person, &[])
            .await
            .unwrap();
        let view = f.store.draft(&f.riker).await.unwrap();
        f.store
            .autosave(&f.riker, &edit("Will Riker", None, vec![]))
            .await
            .unwrap();
        f.store.rename(&troi, "Counselor Troi").await.unwrap();

        let saved = f
            .store
            .save(&f.riker, &view.published_commit)
            .await
            .unwrap();
        assert!(
            matches!(saved, Saved::Saved { commit: Some(_) }),
            "{saved:?}"
        );
        assert_eq!(
            f.store.edit_of(&troi).await.unwrap().unwrap().name,
            "Counselor Troi"
        );
        assert_eq!(
            f.store.edit_of(&f.riker).await.unwrap().unwrap().name,
            "Will Riker"
        );
    }

    /// A link made while the card is open is not part of the draft, and
    /// the save keeps it.
    #[tokio::test]
    async fn a_link_made_while_editing_survives_the_save() {
        let f = riker().await;
        let view = f.store.draft(&f.riker).await.unwrap();
        f.store
            .autosave(&f.riker, &edit("Will Riker", None, vec![]))
            .await
            .unwrap();
        let email = Handle::email("riker@enterprise.org").unwrap();
        f.store.link(&email, &f.riker).await.unwrap();

        let saved = f
            .store
            .save(&f.riker, &view.published_commit)
            .await
            .unwrap();
        assert!(matches!(saved, Saved::Saved { .. }), "{saved:?}");
        let got = f.store.resolve(std::slice::from_ref(&email)).await.unwrap();
        assert_eq!(got[email.as_str()].names, ["Will Riker"]);
    }

    #[tokio::test]
    async fn fields_are_kept_in_the_order_given_and_dropped_when_left_out() {
        let f = riker().await;
        let view = f.store.draft(&f.riker).await.unwrap();
        let both = vec![
            field("a", FieldKind::Url, "https://example.org/riker"),
            field("b", FieldKind::Birthday, "2335-08-19"),
        ];
        f.store
            .autosave(&f.riker, &edit("William Riker", None, both))
            .await
            .unwrap();
        f.store
            .autosave(
                &f.riker,
                &edit(
                    "William Riker",
                    None,
                    vec![
                        field("b", FieldKind::Birthday, "2335-08-19"),
                        field("c", FieldKind::Other, "x"),
                    ],
                ),
            )
            .await
            .unwrap();
        f.store
            .save(&f.riker, &view.published_commit)
            .await
            .unwrap();
        let ids: Vec<String> = f
            .store
            .edit_of(&f.riker)
            .await
            .unwrap()
            .unwrap()
            .fields
            .into_iter()
            .map(|f| f.field_id)
            .collect();
        assert_eq!(ids, ["b", "c"]);
    }

    #[tokio::test]
    async fn an_edit_without_a_name_or_with_two_fields_alike_is_refused() {
        let f = riker().await;
        f.store.draft(&f.riker).await.unwrap();
        assert!(f
            .store
            .autosave(&f.riker, &edit("  ", None, vec![]))
            .await
            .is_err());
        let twice = vec![
            field("a", FieldKind::Url, "x"),
            field("a", FieldKind::Url, "y"),
        ];
        assert!(f
            .store
            .autosave(&f.riker, &edit("Will", None, twice))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn discard_drops_the_draft_and_a_new_one_starts_from_what_is_published() {
        let f = riker().await;
        f.store.draft(&f.riker).await.unwrap();
        f.store
            .autosave(&f.riker, &edit("Will Riker", None, vec![]))
            .await
            .unwrap();
        f.store.discard(&f.riker).await.unwrap();
        let view = f.store.draft(&f.riker).await.unwrap();
        assert_eq!(view.mine.unwrap().name, "William Riker");
    }

    #[tokio::test]
    async fn a_draft_outlives_the_store_being_closed_and_opened() {
        let dir = tempfile::tempdir().unwrap();
        let path = crate::store_path(dir.path());
        let store = Store::open(&path).await.unwrap();
        let riker = store
            .create("William Riker", ContactKind::Person, &[])
            .await
            .unwrap();
        store.draft(&riker).await.unwrap();
        store
            .autosave(&riker, &edit("Will Riker", None, vec![]))
            .await
            .unwrap();
        store.close().await;

        let store = Store::open(&path).await.unwrap();
        let view = store.draft(&riker).await.unwrap();
        assert_eq!(view.mine.unwrap().name, "Will Riker");
        store.close().await;
    }
}
