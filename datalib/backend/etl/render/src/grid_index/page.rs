//! Writing a page of documents into a store that holds documents, a render
//! store or the grid index: every check over the whole page first, then
//! each table's rows in a few statements rather than one a row. The page is
//! written as if its documents were written one after another, in order;
//! `apply_one` writes a page of one.

use std::collections::{HashMap, HashSet};

use anyhow::{bail, Context, Result};
use datalib_etl::bulk::BulkUpsertable;
use datalib_schema::grid_rows::GridRow;
use datalib_sql::values::fetch as values;
use datalib_sql::{sql, sql_id};
use sqlx::{Row, SqliteConnection};

use super::{
    contact_records, delete_document_rows, doc_kind_for, document_row, first_repeated, misfiled,
    uuids_one_way, RenderedMarkdown, Store, WriteLock, RENDERER_VERSION,
};

/// Write `page`, replacing what the store held for each of its documents.
/// A document named twice is written twice, the later over the earlier.
/// Returns how many grid rows the page holds.
pub(super) async fn apply_page(write_lock: &WriteLock, page: &[RenderedMarkdown]) -> Result<usize> {
    let settled: Vec<_> = page.iter().map(crate::names::settled).collect();
    let mut rows = 0;
    let mut run: Vec<&RenderedMarkdown> = Vec::new();
    let mut in_run: HashSet<&str> = HashSet::new();
    for md in settled.iter().map(|md| &**md) {
        if !in_run.insert(md.markdown_uuid.as_str()) {
            rows += apply_run(write_lock, &run).await?;
            run.clear();
            in_run = HashSet::from([md.markdown_uuid.as_str()]);
        }
        run.push(md);
    }
    rows += apply_run(write_lock, &run).await?;
    Ok(rows)
}

/// What a run of documents, each named once, writes: the checks every
/// document has to pass, made before anything is written.
struct Checked<'a> {
    /// Documents that end with no rows: absent, and their rows go.
    empty: Vec<String>,
    kept: Vec<&'a RenderedMarkdown>,
}

fn check<'a>(run: &[&'a RenderedMarkdown]) -> Result<Checked<'a>> {
    let mut checked = Checked {
        empty: Vec::new(),
        kept: Vec::new(),
    };
    let mut holder: HashMap<&str, &str> = HashMap::new();
    for md in run.iter().copied() {
        if md.rows.is_empty() {
            checked.empty.push(md.markdown_uuid.clone());
            continue;
        }
        document_row(&md.rows, &md.markdown_uuid)?;
        if let Some(why) = misfiled(md) {
            bail!("{why}");
        }
        let uuids: Vec<&str> = md.rows.iter().map(|r| r.uuid.as_str()).collect();
        if let Some(twice) = first_repeated(&uuids) {
            bail!(
                "document {}: two of its rows have the uuid {twice}",
                md.markdown_uuid
            );
        }
        for uuid in uuids {
            if let Some(other) = holder.insert(uuid, &md.markdown_uuid) {
                bail!(
                    "documents {other} and {} both hold a row with the uuid {uuid}; \
                     two documents minted one id",
                    md.markdown_uuid
                );
            }
        }
        if let Some(node) = md
            .data_nodes
            .iter()
            .find(|n| n.markdown_uuid != md.markdown_uuid)
        {
            bail!(
                "document {} carries container {} under document {}",
                md.markdown_uuid,
                node.uuid,
                node.markdown_uuid
            );
        }
        if let Some(f) = md
            .filings
            .iter()
            .find(|f| f.markdown_uuid != md.markdown_uuid)
        {
            bail!(
                "document {} carries a filing of {} under document {}",
                md.markdown_uuid,
                f.node_uuid,
                f.markdown_uuid
            );
        }
        checked.kept.push(md);
    }
    Ok(checked)
}

async fn apply_run(write_lock: &WriteLock, run: &[&RenderedMarkdown]) -> Result<usize> {
    if run.is_empty() {
        return Ok(0);
    }
    let checked = check(run)?;
    // Inside `begin_transaction` every guard hands back the same
    // connection, so the page joins the batch; otherwise it auto-commits
    // statement by statement, as a single document always did.
    let mut guard = write_lock.acquire().await?;
    let pool_conn = guard.conn();
    if !checked.empty.is_empty() {
        delete_document_rows(pool_conn, write_lock.store(), &checked.empty)
            .await?
            .note(write_lock);
        for document in &checked.empty {
            tracing::info!(%document, "this document has no rows left; dropped it");
        }
    }
    let kept = &checked.kept;
    if kept.is_empty() {
        return Ok(0);
    }
    let conn: &mut SqliteConnection = pool_conn;
    let documents = serde_json::to_string(
        &kept
            .iter()
            .map(|md| md.markdown_uuid.as_str())
            .collect::<Vec<_>>(),
    )?;

    put_grid_rows(conn, write_lock, kept).await?;
    put_edges(conn, kept, &documents).await?;
    put_nodes(conn, write_lock, kept, &documents).await?;
    put_filings(conn, kept, &documents).await?;
    put_contacts(conn, write_lock.store(), kept, &documents).await?;
    put_photos(conn, kept, &documents).await?;
    put_markdowns(conn, kept, &documents).await?;
    for md in kept {
        write_lock.note_written(&md.markdown_uuid);
    }
    Ok(kept.iter().map(|md| md.rows.len()).sum())
}

/// Each document's rows: only the rows it no longer has are deleted, the
/// rest are written where they differ, and one that did not change is not
/// written at all. A uuid another document holds is decided by who wrote
/// that document: one an earlier run wrote means the row moved (a message
/// re-bucketed into another period, a chat whose document id is minted
/// from a name that changed), and this document takes it over; one this
/// run wrote means two documents minted one id, which fails.
async fn put_grid_rows(
    conn: &mut SqliteConnection,
    write_lock: &WriteLock,
    kept: &[&RenderedMarkdown],
) -> Result<()> {
    let held: Vec<(&str, &str)> = kept
        .iter()
        .flat_map(|md| {
            md.rows
                .iter()
                .map(|r| (md.markdown_uuid.as_str(), r.uuid.as_str()))
        })
        .collect();
    let gone: Vec<String> = sql!(
        "etl.render.grid.delete_rows_documents_dropped",
        "DELETE FROM grid_rows \
          WHERE markdown_uuid IN (SELECT DISTINCT value ->> 0 FROM json_each(?1)) \
            AND (markdown_uuid, uuid) NOT IN \
                (SELECT value ->> 0, value ->> 1 FROM json_each(?1)) \
         RETURNING uuid"
    )
    .query_scalar()
    .bind(serde_json::to_string(&held)?)
    .fetch_all(&mut *conn)
    .await
    .context("delete the rows these documents no longer have")?;
    write_lock.note_gone(gone);

    let rows: Vec<GridRow> = kept
        .iter()
        .flat_map(|md| {
            md.rows.iter().map(|row| GridRow {
                source_id: Some(datalib_schema::grid_rows::filed_under(
                    &row.provider,
                    &md.source_id,
                )),
                ..row.clone()
            })
        })
        .collect();
    write_grid_rows(conn, write_lock, &rows).await
}

/// Writes `rows`, each where it differs from what the index holds, and
/// decides a uuid another document holds by who wrote that document (see
/// [`put_grid_rows`]). The uuids in `rows` are distinct.
pub(super) async fn write_grid_rows(
    conn: &mut SqliteConnection,
    write_lock: &WriteLock,
    rows: &[GridRow],
) -> Result<()> {
    let rows: Vec<GridRow> = rows.iter().map(|r| uuids_one_way(r).into_owned()).collect();
    let columns = 1 + GridRow::TYPED_COLUMNS.len();
    let written: HashSet<String> = values(
        conn,
        sql_id!("etl.render.grid.upsert_grid_rows"),
        &grid_row_head(),
        columns,
        &PUT_GRID_ROWS_TAIL,
        &rows,
        |q, row| row.bind_into(q),
    )
    .await
    .context("write grid rows")?
    .iter()
    .map(|r| r.try_get::<String, _>(0))
    .collect::<Result<_, _>>()?;
    let unwritten: Vec<&GridRow> = rows.iter().filter(|r| !written.contains(&r.uuid)).collect();
    write_lock.note_changed(written);
    if unwritten.is_empty() {
        return Ok(());
    }

    // Nothing written: the row is here unchanged, or another document
    // holds its uuid.
    let owners: HashMap<String, (String, String)> = sql!(
        "etl.render.grid.find_grid_row_owners",
        "SELECT uuid, provider, IFNULL(markdown_uuid, '') FROM grid_rows \
          WHERE uuid IN (SELECT value FROM json_each(?))"
    )
    .query_as::<(String, String, String)>()
    .bind(serde_json::to_string(
        &unwritten
            .iter()
            .map(|r| r.uuid.as_str())
            .collect::<Vec<_>>(),
    )?)
    .fetch_all(&mut *conn)
    .await
    .context("find who holds the rows not written")?
    .into_iter()
    .map(|(uuid, provider, md)| (uuid, (provider, md)))
    .collect();
    let mut moved: Vec<&GridRow> = Vec::new();
    for row in unwritten {
        let incoming = row.markdown_uuid.as_deref().unwrap_or("");
        let Some((provider, md)) = owners.get(&row.uuid) else {
            bail!("grid_row {} was neither written nor held", row.uuid);
        };
        if md == incoming {
            continue;
        }
        if *provider != row.provider || write_lock.written_this_run(md) {
            bail!(
                "insert grid_row {}: an existing {provider} row already holds that \
                 uuid (markdown {md}, written this run); the incoming row is a {} \
                 from markdown {}",
                row.uuid,
                row.provider,
                row.markdown_uuid.as_deref().unwrap_or("<none>"),
            );
        }
        tracing::info!(uuid = %row.uuid, from = %md, to = incoming, "grid_row moved to another document");
        moved.push(row);
    }
    if moved.is_empty() {
        return Ok(());
    }
    let moved_uuids: Vec<String> = moved.iter().map(|r| r.uuid.clone()).collect();
    sql!(
        "etl.render.grid.delete_moved_grid_rows",
        "DELETE FROM grid_rows WHERE uuid IN (SELECT value FROM json_each(?))"
    )
    .query()
    .bind(serde_json::to_string(&moved_uuids)?)
    .execute(&mut *conn)
    .await
    .context("release the moved grid rows")?;
    values(
        conn,
        sql_id!("etl.render.grid.insert_moved_grid_rows"),
        &grid_row_head(),
        columns,
        "",
        &moved,
        |q, row| row.bind_into(q),
    )
    .await
    .context("insert the moved grid rows")?;
    write_lock.note_gone(moved_uuids);
    Ok(())
}

fn grid_row_head() -> String {
    let mut columns = vec![GridRow::ID_COLUMN];
    columns.extend(GridRow::TYPED_COLUMNS);
    format!("INSERT INTO {} ({})", GridRow::TABLE, columns.join(", "))
}

/// On a uuid its own document already holds, rewrite the row only where it
/// differs: an unchanged row then costs no index write and, in the grid
/// index, no new search terms. What it returns is what changed.
static PUT_GRID_ROWS_TAIL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    let columns = GridRow::TYPED_COLUMNS;
    let list = |prefix: &str| -> String {
        columns
            .iter()
            .map(|c| format!("{prefix}{c}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let set: Vec<String> = columns
        .iter()
        .map(|c| format!("{c} = excluded.{c}"))
        .collect();
    format!(
        "ON CONFLICT ({id}) DO UPDATE SET {} \
         WHERE {table}.markdown_uuid IS excluded.markdown_uuid AND ({}) IS NOT ({}) \
         RETURNING {id}",
        set.join(", "),
        list(&format!("{}.", GridRow::TABLE)),
        list("excluded."),
        id = GridRow::ID_COLUMN,
        table = GridRow::TABLE,
    )
});

/// Each document owns the edges whose `src_markdown_uuid` it is, so a
/// re-render replaces its outgoing set. Incoming edges belong to the other
/// document and stay.
async fn put_edges(
    conn: &mut SqliteConnection,
    kept: &[&RenderedMarkdown],
    documents: &str,
) -> Result<()> {
    sql!(
        "etl.render.grid.delete_edges_of_page",
        "DELETE FROM edges WHERE src_markdown_uuid IN (SELECT value FROM json_each(?))"
    )
    .query()
    .bind(documents)
    .execute(&mut *conn)
    .await
    .context("delete prior edges")?;
    let edges: Vec<_> = kept.iter().flat_map(|md| &md.edges).collect();
    values(
        conn,
        sql_id!("etl.render.grid.insert_edges"),
        "INSERT INTO edges (edge_uuid, src_markdown_uuid, src_anchor_uuid, dst_markdown_uuid, \
         dst_anchor_uuid, label, kind)",
        7,
        "",
        &edges,
        |q, e| {
            q.bind(&e.edge_uuid)
                .bind(&e.src_markdown_uuid)
                .bind(&e.src_anchor_uuid)
                .bind(&e.dst_markdown_uuid)
                .bind(&e.dst_anchor_uuid)
                .bind(&e.label)
                .bind(e.kind)
        },
    )
    .await
    .context("insert edges")?;
    Ok(())
}

/// The containers each document's rows live in, under the document.
async fn put_nodes(
    conn: &mut SqliteConnection,
    write_lock: &WriteLock,
    kept: &[&RenderedMarkdown],
    documents: &str,
) -> Result<()> {
    let carried: Vec<String> = sql!(
        "etl.render.grid.delete_nodes_of_page",
        "DELETE FROM data_nodes WHERE markdown_uuid IN (SELECT value FROM json_each(?)) \
         RETURNING uuid"
    )
    .query_scalar()
    .bind(documents)
    .fetch_all(&mut *conn)
    .await
    .context("delete prior containers")?;
    write_lock.touch_nodes(carried);
    let nodes: Vec<_> = kept.iter().flat_map(|md| &md.data_nodes).collect();
    values(
        conn,
        sql_id!("etl.render.grid.upsert_data_nodes"),
        "INSERT OR REPLACE INTO data_nodes \
         (markdown_uuid, uuid, source_id, kind, name, parent_uuid, sort_key, read_together, \
          is_place, read_uuid, place_uuid, upstream_entity_kind, upstream_id, upstream_account)",
        14,
        "",
        &nodes,
        |q, n| {
            q.bind(&n.markdown_uuid)
                .bind(&n.uuid)
                .bind(&n.source_id)
                .bind(&n.kind)
                .bind(&n.name)
                .bind(&n.parent_uuid)
                .bind(&n.sort_key)
                .bind(n.read_together)
                .bind(n.is_place)
                .bind(&n.read_uuid)
                .bind(&n.place_uuid)
                .bind(&n.upstream_entity_kind)
                .bind(&n.upstream_id)
                .bind(&n.upstream_account)
        },
    )
    .await
    .context("insert containers")?;
    write_lock.touch_nodes(nodes.iter().map(|n| n.uuid.clone()));
    Ok(())
}

async fn put_filings(
    conn: &mut SqliteConnection,
    kept: &[&RenderedMarkdown],
    documents: &str,
) -> Result<()> {
    sql!(
        "etl.render.grid.delete_filings_of_page",
        "DELETE FROM filings WHERE markdown_uuid IN (SELECT value FROM json_each(?))"
    )
    .query()
    .bind(documents)
    .execute(&mut *conn)
    .await
    .context("delete prior filings")?;
    let filings: Vec<_> = kept.iter().flat_map(|md| &md.filings).collect();
    values(
        conn,
        sql_id!("etl.render.grid.insert_filings"),
        "INSERT OR IGNORE INTO filings (markdown_uuid, node_uuid, under_uuid)",
        3,
        "",
        &filings,
        |q, f| {
            q.bind(&f.markdown_uuid)
                .bind(&f.node_uuid)
                .bind(&f.under_uuid)
        },
    )
    .await
    .context("insert filings")?;
    Ok(())
}

/// The people each document carries. A render store keeps each one's
/// whole record under the document, two accounts of one person in one
/// document being one row, the later winning; the grid index keeps a
/// record once (`contact_records`).
async fn put_contacts(
    conn: &mut SqliteConnection,
    store: Store,
    kept: &[&RenderedMarkdown],
    documents: &str,
) -> Result<()> {
    let carried: Vec<(&str, &datalib_contact_schema::NormalizedContact)> = kept
        .iter()
        .flat_map(|md| md.contacts.iter().map(|c| (md.markdown_uuid.as_str(), c)))
        .collect();
    replace_contacts(conn, store, documents, &carried).await
}

/// The people the documents in the JSON array `documents` carry, replaced
/// by `carried`, `(markdown_uuid, contact)`.
pub(super) async fn replace_contacts(
    conn: &mut SqliteConnection,
    store: Store,
    documents: &str,
    carried: &[(&str, &datalib_contact_schema::NormalizedContact)],
) -> Result<()> {
    for sql in store.delete_contacts() {
        sql.query()
            .bind(documents)
            .execute(&mut *conn)
            .await
            .context("delete prior source contacts")?;
    }
    if carried.is_empty() {
        return Ok(());
    }
    match store {
        Store::Index => contact_records::insert_page(conn, carried).await,
        Store::Render => put_source_contacts(conn, carried).await,
    }
}

async fn put_source_contacts(
    conn: &mut SqliteConnection,
    carried: &[(&str, &datalib_contact_schema::NormalizedContact)],
) -> Result<()> {
    let records = carried
        .iter()
        .map(|(md, c)| {
            let json = serde_json::to_string(c).context("serialize a source contact")?;
            Ok((*md, *c, json))
        })
        .collect::<Result<Vec<_>>>()?;
    values(
        conn,
        sql_id!("etl.render.grid.upsert_source_contacts"),
        "INSERT OR REPLACE INTO source_contacts \
         (markdown_uuid, contact_key, source_id, name, seen_items, last_seen_at, contact_json)",
        7,
        "",
        &records,
        |q, (md, c, json)| {
            let seen = c.seen.as_ref();
            q.bind(*md)
                .bind(&c.key)
                .bind(&c.source_id)
                .bind(c.name())
                .bind(seen.map_or(0, |s| s.items as i64))
                .bind(seen.and_then(|s| s.last_at.as_deref()))
                .bind(json)
        },
    )
    .await
    .context("insert source contacts")?;
    let handles: Vec<(&str, &str, &str)> = carried
        .iter()
        .flat_map(|(md, c)| {
            c.handles
                .iter()
                .filter_map(|h| h.handle.as_ref())
                .map(move |h| (*md, c.key.as_str(), h.as_str()))
        })
        .collect();
    values(
        conn,
        sql_id!("etl.render.grid.insert_source_contact_handles"),
        "INSERT OR IGNORE INTO source_contact_handles (markdown_uuid, contact_key, handle)",
        3,
        "",
        &handles,
        |q, (md, key, handle)| q.bind(*md).bind(*key).bind(*handle),
    )
    .await
    .context("insert source contact handles")?;
    Ok(())
}

async fn put_photos(
    conn: &mut SqliteConnection,
    kept: &[&RenderedMarkdown],
    documents: &str,
) -> Result<()> {
    sql!(
        "etl.render.grid.delete_photos_of_page",
        "DELETE FROM source_photos WHERE markdown_uuid IN (SELECT value FROM json_each(?))"
    )
    .query()
    .bind(documents)
    .execute(&mut *conn)
    .await
    .context("delete prior source photos")?;
    let photos = kept
        .iter()
        .flat_map(|md| md.photos.iter().map(|p| (md.markdown_uuid.as_str(), p)))
        .map(|(md, p)| {
            let json = serde_json::to_string(p).context("serialize a source photo")?;
            Ok((md, p, json))
        })
        .collect::<Result<Vec<_>>>()?;
    values(
        conn,
        sql_id!("etl.render.grid.upsert_source_photos"),
        "INSERT OR REPLACE INTO source_photos (markdown_uuid, photo_uuid, source_id, photo_json)",
        4,
        "",
        &photos,
        |q, (md, p, json)| q.bind(*md).bind(&p.uuid).bind(&p.source_id).bind(json),
    )
    .await
    .context("insert source photos")?;
    Ok(())
}

/// Each document's `markdowns` row, made from its document row.
async fn put_markdowns(
    conn: &mut SqliteConnection,
    kept: &[&RenderedMarkdown],
    documents: &str,
) -> Result<()> {
    sql!(
        "etl.render.grid.delete_markdowns_of_page",
        "DELETE FROM markdowns WHERE markdown_uuid IN (SELECT value FROM json_each(?))"
    )
    .query()
    .bind(documents)
    .execute(&mut *conn)
    .await
    .context("delete prior markdowns rows")?;
    let rows = kept
        .iter()
        .map(|md| {
            let canonical = document_row(&md.rows, &md.markdown_uuid)?;
            let content_hash = if md.sections.is_empty() {
                md.content_hash.clone()
            } else {
                Some(crate::section::content_hash(&md.sections))
            };
            Ok(MarkdownsRow {
                md,
                canonical,
                kind: doc_kind_for(&canonical.kind),
                version: format!("{RENDERER_VERSION}.{}", md.render_version),
                content_hash,
                front: (!md.front.is_empty()).then(|| md.front.to_json()),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    values(
        conn,
        sql_id!("etl.render.grid.insert_markdowns_rows"),
        "INSERT INTO markdowns \
         (markdown_uuid, source_id, provider, kind, title, created_at, modified_at, \
          item_count, upstream_cursor, renderer_version, render_unit_key, content_hash, front, \
          searchable)",
        14,
        "",
        &rows,
        |q, r| {
            q.bind(&r.md.markdown_uuid)
                .bind(&r.md.source_id)
                .bind(&r.canonical.provider)
                .bind(r.kind)
                .bind(&r.canonical.conversation_name)
                .bind(r.canonical.created_at.as_deref())
                .bind(r.canonical.modified_at.as_deref())
                .bind(r.canonical.item_count)
                .bind(r.md.upstream_cursor.as_deref())
                .bind(&r.version)
                .bind(&r.md.render_unit_key)
                .bind(&r.content_hash)
                .bind(&r.front)
                .bind(r.md.searchable)
        },
    )
    .await
    .context("insert markdowns rows")?;
    Ok(())
}

struct MarkdownsRow<'a> {
    md: &'a RenderedMarkdown,
    canonical: &'a GridRow,
    kind: &'static str,
    version: String,
    content_hash: Option<String>,
    front: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::Path;

    use datalib_contact_schema::{ContactHandle, ContactKind, NormalizedContact};
    use datalib_schema::data_nodes::DataNodeRow;
    use datalib_schema::edges::EdgeRow;
    use datalib_schema::filings::FilingRow;
    use datalib_schema::grid_rows::GridRow;

    use crate::grid_index::source_cursor_tests::doc;
    use crate::grid_index::{
        apply_one, apply_page, close_index, open_index, seal, RenderedMarkdown, WriteLock,
    };

    fn with_messages(source: &str, uuid: &str, messages: &[&str]) -> RenderedMarkdown {
        let mut md = doc(source, uuid, &format!("the text of {uuid}"));
        for m in messages {
            let mut row: GridRow = md.rows[0].clone();
            row.uuid = (*m).to_string();
            row.is_document = false;
            row.author = Some(format!("author of {m}"));
            md.rows.push(row);
        }
        md
    }

    fn person(key: &str, name: &str, items: u64) -> NormalizedContact {
        let mut c = NormalizedContact::new("src", key, ContactKind::Person);
        c.names = vec![name.to_string()];
        c.handles = vec![
            ContactHandle::email(None, format!("{key}@enterprise.org")),
            ContactHandle::phone(None, "(907) 555-0142"),
        ];
        c.seen = Some(datalib_contact_schema::Seen {
            items,
            last_at: Some("2369-01-01".into()),
        });
        c
    }

    fn node(md: &str, uuid: &str) -> DataNodeRow {
        DataNodeRow {
            markdown_uuid: md.into(),
            uuid: uuid.into(),
            source_id: "src".into(),
            kind: "Channel".into(),
            name: Some(format!("#{uuid}")),
            parent_uuid: None,
            sort_key: String::new(),
            read_together: true,
            is_place: true,
            read_uuid: None,
            place_uuid: None,
            upstream_entity_kind: None,
            upstream_id: None,
            upstream_account: None,
        }
    }

    /// What an earlier run left: older versions of two of the page's
    /// documents, one holding a row a page document takes over, and one
    /// the page drops.
    fn earlier() -> Vec<RenderedMarkdown> {
        let mut a = with_messages("src", "md-a", &["a-1", "a-stale"]);
        a.contacts = vec![person("riker", "Will Riker", 1)];
        vec![
            a,
            with_messages("src", "md-old", &["moving"]),
            with_messages("src", "md-gone", &["gone-1"]),
        ]
    }

    /// The page: a re-render of `md-a` that drops a row and changes one,
    /// a new `md-b` carrying the same person and a container, `md-c`
    /// taking a row over from `md-old`, and `md-gone` with no rows left.
    fn page() -> Vec<RenderedMarkdown> {
        let mut a = with_messages("src", "md-a", &["a-1", "a-2"]);
        a.rows[1].author = Some("a changed author".into());
        a.contacts = vec![person("riker", "Will Riker", 3)];
        a.edges = vec![EdgeRow {
            edge_uuid: "e-1".into(),
            src_markdown_uuid: "md-a".into(),
            src_anchor_uuid: Some("a-1".into()),
            dst_markdown_uuid: "md-b".into(),
            dst_anchor_uuid: None,
            label: Some("replies to".into()),
            kind: 1,
        }];
        let mut b = with_messages("src", "md-b", &["b-1"]);
        b.contacts = vec![
            person("riker", "Will Riker", 5),
            person("troi", "Deanna Troi", 2),
        ];
        b.data_nodes = vec![node("md-b", "n-bridge")];
        b.filings = vec![FilingRow::new("md-b", "b-1", "n-bridge")];
        let c = with_messages("src", "md-c", &["moving"]);
        let mut gone = doc("src", "md-gone", "");
        gone.rows.clear();
        vec![a, b, c, gone]
    }

    async fn written(
        db: &Path,
        by_page: bool,
    ) -> (serde_json::Value, BTreeSet<String>, BTreeSet<String>) {
        let pool = open_index(db).await.unwrap();
        let lock = WriteLock::for_index(pool.clone());
        for md in earlier() {
            apply_one(&lock, &md).await.unwrap();
        }
        seal(&lock).await.unwrap();
        // A new run: what the earlier one wrote is not "written this run".
        let lock = WriteLock::for_index(pool.clone());
        let page = page();
        if by_page {
            apply_page(&lock, &page).await.unwrap();
        } else {
            for md in &page {
                apply_one(&lock, md).await.unwrap();
            }
        }
        let changed: BTreeSet<String> = lock.take_changed().into_iter().collect();
        let touched: BTreeSet<String> = lock.take_touched_nodes().into_iter().collect();
        seal(&lock).await.unwrap();
        close_index(pool).await.unwrap();
        let dump = datalib_store_dump::dump(db, |table| match table {
            // A random id, and when each was written.
            "store_generation" | "_datalib_meta" => datalib_store_dump::Keep::Nothing,
            _ => datalib_store_dump::Keep::Rows,
        })
        .await;
        (dump, changed, touched)
    }

    /// A page writes what its documents written one at a time do, over an
    /// index holding older versions of some of them: the same rows in
    /// every table, the same rows changed and the same nodes touched,
    /// which the terms, the nodes and search follow.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_page_writes_what_its_documents_one_at_a_time_do() {
        let dir = tempfile::tempdir().unwrap();
        let (paged, paged_changed, paged_touched) =
            written(&dir.path().join("paged.sqlite"), true).await;
        let (single, single_changed, single_touched) =
            written(&dir.path().join("single.sqlite"), false).await;
        assert_eq!(paged, single);
        assert_eq!(paged_changed, single_changed);
        assert_eq!(paged_touched, single_touched);
        let rows: Vec<&str> = paged["grid_rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["uuid"].as_str().unwrap())
            .collect();
        assert_eq!(
            rows,
            ["a-1", "a-2", "md-a", "md-b", "b-1", "md-c", "md-old", "moving"]
                .iter()
                .copied()
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>(),
            "a-stale and md-gone's rows went; moving is md-c's"
        );
        let moving = paged["grid_rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["uuid"] == "moving")
            .unwrap();
        assert_eq!(moving["markdown_uuid"], "md-c");
        assert!(paged_changed.contains("a-2") && paged_changed.contains("a-stale"));
        assert!(
            !paged_changed.contains("md-old"),
            "a document the page does not name is not written"
        );
        assert_eq!(paged["contact_records"].as_array().unwrap().len(), 2);
    }

    /// Two documents of one page that hold one row uuid fail, naming both
    /// and writing nothing of the page.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_documents_of_a_page_minting_one_id_fail() {
        let dir = tempfile::tempdir().unwrap();
        let pool = open_index(&dir.path().join("db.sqlite")).await.unwrap();
        let lock = WriteLock::for_index(pool.clone());
        let page = [
            with_messages("src", "md-x", &["shared"]),
            with_messages("src", "md-y", &["shared"]),
        ];
        let e = apply_page(&lock, &page).await.unwrap_err();
        let said = format!("{e:#}");
        assert!(
            said.contains("md-x") && said.contains("md-y") && said.contains("shared"),
            "{said}"
        );
        let n: i64 = datalib_sql::test_sql!("SELECT count(*) FROM grid_rows")
            .query_scalar()
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
        close_index(pool).await.unwrap();
    }

    /// A document named twice in one page is written twice, the later
    /// over the earlier, as one after another.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_document_named_twice_is_written_twice() {
        let dir = tempfile::tempdir().unwrap();
        let pool = open_index(&dir.path().join("db.sqlite")).await.unwrap();
        let lock = WriteLock::for_index(pool.clone());
        let page = [
            with_messages("src", "md-x", &["x-1"]),
            with_messages("src", "md-x", &["x-2"]),
        ];
        apply_page(&lock, &page).await.unwrap();
        let held: Vec<String> = datalib_sql::test_sql!("SELECT uuid FROM grid_rows ORDER BY uuid")
            .query_scalar()
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(held, ["md-x", "x-2"]);
        close_index(pool).await.unwrap();
    }
}
