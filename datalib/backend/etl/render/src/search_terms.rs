//! The search terms in the grid index: every term each `grid_rows` row
//! answers to (`datalib_schema::search_terms`), in the `rows`, `vals`,
//! `vals_fts` and `terms` tables. It is the grid's rows melted, one
//! indexed row per term: what each row's columns say (`search_terms_of`)
//! and what its render supplied (`supplied_terms`). The index's writer
//! rewrites a row's terms in the transaction that writes the row, so the
//! two never disagree; and the people the terms name (`names`,
//! `account_handles`) once a pass.

use anyhow::{Context, Result};
use datalib_schema::search_terms::{
    decode_supplied, search_terms_of, SearchTerm, SearchTermSource,
};
use sqlx::SqliteConnection;

/// Rewrite the terms of the grid rows `uuids` names, as `grid_rows` holds
/// them on `conn` now: a row it no longer holds only loses its terms.
/// Returns the terms written.
pub(crate) async fn write_rows(conn: &mut SqliteConnection, uuids: &[String]) -> Result<usize> {
    let mut written = 0;
    // A part at a time, so a source that changed every row does not hold
    // all of their terms in memory at once.
    for part in uuids.chunks(10_000) {
        written += write_part(conn, part).await?;
    }
    Ok(written)
}

async fn write_part(conn: &mut SqliteConnection, uuids: &[String]) -> Result<usize> {
    let rows = rows_by_uuid(conn, uuids).await?;
    // The batch is staged first, so the dictionary tables fill by
    // joins rather than a lookup per term. `dropped` holds the values
    // the removed rows used: the only ones that can have lost their
    // last term.
    for sql in [
        "CREATE TEMP TABLE IF NOT EXISTS incoming (uuid TEXT NOT NULL, \
             touched_at_utc TEXT, kind INTEGER NOT NULL, value TEXT NOT NULL)",
        "CREATE TEMP TABLE IF NOT EXISTS dropped (val_id INTEGER PRIMARY KEY)",
        "DELETE FROM temp.incoming",
        "DELETE FROM temp.dropped",
    ] {
        sqlx::query(sql).execute(&mut *conn).await?;
    }
    for chunk in uuids.chunks(CHUNK) {
        let p = placeholders(chunk.len(), 1);
        for sql in [
            format!(
                "INSERT OR IGNORE INTO temp.dropped SELECT t.val_id FROM terms t \
                     JOIN rows r ON r.row_id = t.row_id WHERE r.uuid IN ({p})"
            ),
            format!(
                "DELETE FROM terms WHERE row_id IN \
                     (SELECT row_id FROM rows WHERE uuid IN ({p}))"
            ),
            format!("DELETE FROM rows WHERE uuid IN ({p})"),
        ] {
            // Audited: a placeholder per value, every value bound.
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
            for uuid in chunk {
                q = q.bind(uuid);
            }
            q.execute(&mut *conn).await?;
        }
    }
    let terms = every_term(&rows)?;
    for chunk in terms.chunks(CHUNK) {
        // Audited: four placeholders per term, every value bound.
        let sql = format!(
            "INSERT INTO temp.incoming (uuid, touched_at_utc, kind, value) VALUES {}",
            placeholders(chunk.len(), 4)
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for (row, term) in chunk {
            q = q
                .bind(&row.uuid)
                .bind(&row.touched_at_utc)
                .bind(i64::from(term.kind.code()))
                .bind(&term.value);
        }
        q.execute(&mut *conn).await?;
    }
    // New values take keys above every one there now, so the index
    // picks up exactly these.
    let floor: i64 = sqlx::query_scalar("SELECT coalesce(max(val_id), 0) FROM vals")
        .fetch_one(&mut *conn)
        .await?;
    for sql in [
        "INSERT INTO rows (uuid, touched_at_utc) \
             SELECT uuid, max(touched_at_utc) FROM temp.incoming GROUP BY uuid",
        // `WHERE true`: an upsert's SELECT needs a WHERE to parse.
        "INSERT INTO vals (value) SELECT DISTINCT value FROM temp.incoming WHERE true \
             ON CONFLICT (value) DO NOTHING",
    ] {
        sqlx::query(sql).execute(&mut *conn).await?;
    }
    sqlx::query(
        "INSERT INTO vals_fts (rowid, value) SELECT val_id, value FROM vals WHERE val_id > ?",
    )
    .bind(floor)
    .execute(&mut *conn)
    .await?;
    for sql in [
        "INSERT OR IGNORE INTO terms (val_id, kind, row_id) \
             SELECT v.val_id, i.kind, r.row_id FROM temp.incoming i \
             JOIN vals v ON v.value = i.value JOIN rows r ON r.uuid = i.uuid",
        // Seeks `terms`' key: `val_id IN (SELECT …)` reads every term.
        "DELETE FROM temp.dropped WHERE EXISTS \
             (SELECT 1 FROM terms t WHERE t.val_id = dropped.val_id)",
        "DELETE FROM vals_fts WHERE rowid IN (SELECT val_id FROM temp.dropped)",
        "DELETE FROM vals WHERE val_id IN (SELECT val_id FROM temp.dropped)",
    ] {
        sqlx::query(sql).execute(&mut *conn).await?;
    }
    Ok(terms.len())
}

/// Each handle with every name it was seen under: the author a grid row
/// shows beside its handle, and every name a source's record of a person
/// (`source_contacts`) gives for each handle it ties to them; and the
/// handles of each source's own account. Written whole: it is a few
/// thousand rows.
pub(crate) async fn write_people(conn: &mut SqliteConnection) -> Result<()> {
    let authors: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT author_handle, author FROM grid_rows \
         WHERE author_handle IS NOT NULL AND author_handle != '' \
         AND author IS NOT NULL AND author != ''",
    )
    .fetch_all(&mut *conn)
    .await
    .context("read the grid's authors")?;
    let contacts: Vec<(String, String)> = sqlx::query_as(
        "SELECT h.handle, c.contact_json FROM source_contact_handles h \
         JOIN source_contacts c \
         ON c.markdown_uuid = h.markdown_uuid AND c.contact_key = h.contact_key",
    )
    .fetch_all(&mut *conn)
    .await
    .context("read the source contacts")?;
    let accounts = account_handles(&contacts);
    let names = names_seen_under(authors, &contacts);
    sqlx::query("DELETE FROM names").execute(&mut *conn).await?;
    let pairs: Vec<&(String, String)> = names.iter().collect();
    for chunk in pairs.chunks(CHUNK) {
        // Audited: a placeholder pair per name, every value bound.
        let sql = format!(
            "INSERT INTO names (handle, name) VALUES {}",
            placeholders(chunk.len(), 2)
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for (handle, name) in chunk {
            q = q.bind(handle).bind(name);
        }
        q.execute(&mut *conn).await.context("write the names")?;
    }
    sqlx::query("DELETE FROM account_handles")
        .execute(&mut *conn)
        .await?;
    for (handle, source_id) in &accounts {
        sqlx::query("INSERT INTO account_handles (handle, source_id) VALUES (?, ?)")
            .bind(handle)
            .bind(source_id)
            .execute(&mut *conn)
            .await
            .context("write the account handles")?;
    }
    Ok(())
}

/// The `(handle, source_id)` pairs [`write_people`] keeps as the
/// account's own: each handle of a source contact marked as the account
/// (`NormalizedContact::account`), beside the source that says so.
pub fn account_handles(
    contacts: &[(String, String)],
) -> std::collections::BTreeSet<(String, String)> {
    contacts
        .iter()
        // `account` is written only when true; most records skip the parse.
        .filter(|(_, json)| json.contains("\"account\""))
        .filter_map(|(handle, json)| {
            let contact: datalib_contact_schema::NormalizedContact =
                serde_json::from_str(json).ok()?;
            contact.account.then(|| (handle.clone(), contact.source_id))
        })
        .collect()
}

/// The `(handle, name)` pairs [`write_people`] keeps: the grid's authors,
/// then each source contact's names (`NormalizedContact::names`) for each
/// of its handles. A record this build cannot read gives no names.
pub fn names_seen_under(
    authors: Vec<(String, String)>,
    contacts: &[(String, String)],
) -> std::collections::BTreeSet<(String, String)> {
    let mut out: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();
    let mut unread = 0usize;
    for (handle, name) in authors {
        out.insert((handle, name.trim().to_string()));
    }
    for (handle, json) in contacts {
        match serde_json::from_str::<datalib_contact_schema::NormalizedContact>(json) {
            Ok(contact) => {
                for name in contact.names {
                    out.insert((handle.clone(), name.trim().to_string()));
                }
            }
            Err(_) => unread += 1,
        }
    }
    if unread > 0 {
        tracing::warn!(
            unread,
            "source contacts this build cannot read gave no names"
        );
    }
    out.retain(|(_, name)| !name.is_empty());
    out
}

/// The rows behind `uuids` that the grid still holds; a removed row has
/// none, and only loses its terms.
async fn rows_by_uuid(
    conn: &mut SqliteConnection,
    uuids: &[String],
) -> Result<Vec<SearchTermSource>> {
    let mut out = Vec::new();
    for chunk in uuids.chunks(CHUNK) {
        // Audited: a placeholder per value, every value bound.
        let sql = format!(
            "SELECT {} FROM grid_rows WHERE uuid IN ({})",
            SearchTermSource::COLUMNS,
            placeholders(chunk.len(), 1)
        );
        let mut q = sqlx::query_as::<_, SearchTermSource>(sqlx::AssertSqlSafe(sql));
        for uuid in chunk {
            q = q.bind(uuid);
        }
        out.extend(
            q.fetch_all(&mut *conn)
                .await
                .context("read changed grid_rows")?,
        );
    }
    Ok(out)
}

/// Every term of `rows`: what each derives from its columns, then what
/// its render supplied.
fn every_term(rows: &[SearchTermSource]) -> Result<Vec<(&SearchTermSource, SearchTerm)>> {
    let mut out = Vec::new();
    for row in rows {
        let supplied = match row.supplied_terms.as_deref() {
            Some(json) => decode_supplied(json)
                .map_err(anyhow::Error::msg)
                .with_context(|| format!("grid row {}", row.uuid))?,
            None => Vec::new(),
        };
        out.extend(
            search_terms_of(row)
                .into_iter()
                .chain(supplied)
                .map(|t| (row, t)),
        );
    }
    Ok(out)
}

/// Values per statement, under SQLite's bound-parameter limit at four a
/// term.
const CHUNK: usize = 500;

fn placeholders(rows: usize, per_row: usize) -> String {
    let one = if per_row == 1 {
        "?".to_string()
    } else {
        format!("({})", vec!["?"; per_row].join(", "))
    };
    vec![one; rows].join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid_index::{apply_one, delete_markdowns, open_index, RenderedMarkdown, WriteLock};
    use datalib_schema::grid_rows::GridRow;
    use datalib_schema::providers::Provider;
    use datalib_schema::search_terms::SearchTermKind;

    /// The account's handles are those of the source contacts marked as
    /// the account, every handle each ties to them; no other record's.
    #[test]
    fn the_accounts_handles_are_its_marked_contacts() {
        use datalib_contact_schema::{ContactKind, NormalizedContact};
        let mut picard = NormalizedContact::new("slack", "U_PICARD", ContactKind::Person);
        picard.account = true;
        let picard = serde_json::to_string(&picard).unwrap();
        let riker = NormalizedContact::new("slack", "U_RIKER", ContactKind::Person);
        let riker = serde_json::to_string(&riker).unwrap();
        let accounts = account_handles(&[
            ("slack:T/U_PICARD".into(), picard.clone()),
            ("email:picard@e.org".into(), picard),
            ("slack:T/U_RIKER".into(), riker),
            ("email:x@e.org".into(), "{not a contact \"account\"".into()),
        ]);
        let pairs: Vec<(&str, &str)> = accounts
            .iter()
            .map(|(h, s)| (h.as_str(), s.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("email:picard@e.org", "slack"),
                ("slack:T/U_PICARD", "slack")
            ]
        );
    }

    /// A handle is known by every name it was seen under: what its rows
    /// show as their author, and each name a source's record of the person
    /// gives; a blank name, or a record this build cannot read, adds none.
    #[test]
    fn a_handle_is_known_by_every_name_it_was_seen_under() {
        use datalib_contact_schema::{ContactKind, NormalizedContact};
        let mut riker = NormalizedContact::new("slack", "U_RIKER", ContactKind::Person);
        riker.names = vec!["Will Riker".into(), "  ".into(), "Number One".into()];
        let json = serde_json::to_string(&riker).unwrap();
        let names = names_seen_under(
            vec![("email:riker@e.org".into(), "William Riker".into())],
            &[
                ("slack:T/U_RIKER".into(), json.clone()),
                ("email:riker@e.org".into(), json),
                ("email:x@e.org".into(), "{not a contact".into()),
            ],
        );
        let pairs: Vec<(&str, &str)> = names
            .iter()
            .map(|(h, n)| (h.as_str(), n.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("email:riker@e.org", "Number One"),
                ("email:riker@e.org", "Will Riker"),
                ("email:riker@e.org", "William Riker"),
                ("slack:T/U_RIKER", "Number One"),
                ("slack:T/U_RIKER", "Will Riker"),
            ]
        );
    }

    /// A document of one row: its uuid, its author's handle, its title.
    fn doc(root: &std::path::Path, uuid: &str, handle: &str, title: &str) -> RenderedMarkdown {
        let md_path = crate::grid_index::doc_md_path(&root.join("enterprise"), uuid);
        let file = md_path.strip_prefix(root).unwrap().to_string_lossy();
        let row = GridRow::builder()
            .uuid(uuid)
            .provider(Provider::Claude)
            .kind("Chat")
            .source_label("Claude")
            .is_document(true)
            .created_at(Some("2026-01-01T09:00:00+00:00".to_string()))
            .conversation_uuid(uuid)
            .conversation_name(Some(title.to_string()))
            .author_handle(Some(handle.to_string()))
            .entire_chat(format!("/chat/{uuid}"))
            .body("")
            .markdown_uuid(Some(uuid.to_string()))
            .qmd_path(file.into_owned())
            .build()
            .unwrap();
        RenderedMarkdown {
            section_layout: None,
            content_hash: None,
            markdown_uuid: uuid.to_string(),
            source_id: "enterprise".into(),
            upstream_cursor: None,
            bucket_key: None,
            md_path,
            render_version: 1,
            rows: vec![row],
            sections: Vec::new(),
            edges: Vec::new(),
            contacts: Vec::new(),
            photos: Vec::new(),
            problems: Vec::new(),
        }
    }

    struct Grid {
        _dir: tempfile::TempDir,
        root: std::path::PathBuf,
        pool: sqlx::SqlitePool,
    }

    impl Grid {
        async fn new() -> Grid {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().to_path_buf();
            let pool = open_index(&datalib_core::layout::grid_index_db(&root))
                .await
                .unwrap();
            Grid {
                _dir: dir,
                root,
                pool,
            }
        }

        async fn seal(&self, put: &[RenderedMarkdown], remove: &[&str]) {
            let lock = WriteLock::for_index(self.pool.clone());
            for md in put {
                apply_one(&lock, &self.root, md).await.unwrap();
            }
            for md in remove {
                delete_markdowns(&lock, &[md.to_string()]).await.unwrap();
            }
            crate::grid_index::seal(&lock).await.unwrap();
        }

        /// The `(uuid, kind)` of every term matching `value` exactly.
        async fn matching(&self, value: &str) -> Vec<(String, String)> {
            let coded: Vec<(String, i64)> = sqlx::query_as(
                "SELECT r.uuid, t.kind FROM vals_fts JOIN terms t ON t.val_id = vals_fts.rowid \
                 JOIN rows r ON r.row_id = t.row_id WHERE vals_fts MATCH ?",
            )
            .bind(format!("\"{value}\""))
            .fetch_all(&self.pool)
            .await
            .unwrap();
            let mut hits: Vec<(String, String)> = coded
                .into_iter()
                .map(|(uuid, code)| {
                    let kind = SearchTermKind::from_code(code).expect("a known kind");
                    (uuid, kind.as_str().to_string())
                })
                .collect();
            hits.sort();
            hits
        }

        /// Whether the dictionary still holds `value` at all.
        async fn holds(&self, value: &str) -> bool {
            let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vals WHERE value = ?")
                .bind(value)
                .fetch_one(&self.pool)
                .await
                .unwrap();
            n > 0
        }
    }

    fn hit(uuid: &str, kind: &str) -> (String, String) {
        (uuid.to_string(), kind.to_string())
    }

    #[tokio::test]
    async fn a_row_is_found_by_its_terms_once_it_is_written() {
        let g = Grid::new().await;
        let (a, b) = (
            doc(&g.root, "c-a", "email:ann@example.com", "Away team"),
            doc(&g.root, "c-b", "email:bo@example.com", "Bridge"),
        );
        g.seal(&[a, b], &[]).await;
        assert_eq!(
            g.matching("email:ann@example.com").await,
            [hit("c-a", "from")]
        );
        assert_eq!(g.matching("c-b").await, [hit("c-b", "id")]);
    }

    /// A write rewrites the terms of the rows it changed: a value no row
    /// uses any more goes, and a row written again unchanged keeps its own.
    #[tokio::test]
    async fn only_the_rows_that_changed_have_their_terms_rewritten() {
        let g = Grid::new().await;
        let kept = doc(&g.root, "c-k", "email:kit@example.com", "Kept");
        g.seal(
            &[
                doc(&g.root, "c-a", "email:ann@example.com", "Away team"),
                doc(&g.root, "c-b", "email:bo@example.com", "Bridge"),
                kept.clone(),
            ],
            &[],
        )
        .await;

        g.seal(
            &[
                doc(&g.root, "c-a", "email:ann@example.org", "Away team"),
                doc(&g.root, "c-c", "email:cy@example.com", "Cargo"),
                kept,
            ],
            &["c-b"],
        )
        .await;
        assert_eq!(g.matching("email:ann@example.com").await, Vec::new());
        assert_eq!(
            g.matching("email:ann@example.org").await,
            [hit("c-a", "from")]
        );
        assert_eq!(g.matching("c-b").await, Vec::new());
        assert_eq!(
            g.matching("email:cy@example.com").await,
            [hit("c-c", "from")]
        );
        assert_eq!(
            g.matching("email:kit@example.com").await,
            [hit("c-k", "from")]
        );
        assert!(
            !g.holds("email:ann@example.com").await,
            "a value no row uses any more stays"
        );
        assert!(!g.holds("Bridge").await, "a removed row's title stays");
        assert!(g.holds("Kept").await);
    }

    fn supplied(md: &mut RenderedMarkdown, terms: &[(SearchTermKind, &str)]) {
        md.rows[0].set_supplied_terms(terms.iter().map(|(k, v)| SearchTerm::new(*k, *v)));
    }

    /// A render's own terms (an email's To, its labels) reach the index,
    /// and a write in which only they changed rewrites them: they are part
    /// of the row, so the row changed.
    #[tokio::test]
    async fn a_renders_supplied_terms_are_written_and_follow_its_rerender() {
        use SearchTermKind::{Cc, Label, To};
        let g = Grid::new().await;
        let mut a = doc(&g.root, "c-a", "email:ann@example.com", "Away team");
        supplied(&mut a, &[(To, "email:bo@example.com"), (Label, "Inbox")]);
        g.seal(&[a.clone()], &[]).await;
        assert_eq!(g.matching("email:bo@example.com").await, [hit("c-a", "to")]);
        assert_eq!(g.matching("Inbox").await, [hit("c-a", "label")]);

        supplied(
            &mut a,
            &[(To, "email:bo@example.com"), (Cc, "email:cy@example.com")],
        );
        g.seal(&[a], &[]).await;
        assert_eq!(g.matching("Inbox").await, Vec::new());
        assert_eq!(g.matching("email:cy@example.com").await, [hit("c-a", "cc")]);
        assert_eq!(g.matching("email:bo@example.com").await, [hit("c-a", "to")]);

        g.seal(&[], &["c-a"]).await;
        assert_eq!(g.matching("email:bo@example.com").await, Vec::new());
    }
}
