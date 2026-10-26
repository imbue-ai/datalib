//! Render LinkedIn `connections` as first-class contacts through the
//! shared [`datalib_etl_contact_common`] renderer.

use anyhow::{Context, Result};
use datalib_contact_schema::{ContactHandle, ContactKind, Detail, NormalizedContact};
use datalib_etl::progress::Progress;
use datalib_etl_contact_common::{render_all, ContactDoc, ContactRenderProfile};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::{changed_rows, Input, Inputs};
use datalib_etl_render::photo;
use datalib_handle::Handle;
use datalib_schema::problems::{Problem, Reason};
use serde_json::Value;
use std::collections::HashMap;

use crate::ids;
use datalib_etl_linkedin::ingest::photos::load_photo_edges;
use datalib_etl_linkedin::ingest::{db_path_for, RawDb};

use crate::processor::{FeedOutcome, Source};

use crate::render::{narrow_docs, RENDER_VERSION};
use datalib_schema::providers::Provider;

/// Human label + grouping for every LinkedIn connection.
const GROUP_LABEL: &str = "Connections";

pub fn render_connections(
    source: &Source<'_>,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<FeedOutcome> {
    let db_path = db_path_for(source.raw_dir);
    if !db_path.exists() {
        return Ok(FeedOutcome::default());
    }
    // Read at a commit: this store belongs to the download step, and
    // nothing committed means nothing to render from.
    let Some(db) = block_on(RawDb::open_reader(&db_path, source.range.pin))? else {
        return Ok(FeedOutcome::default());
    };
    let rendered = render_open(&db, source, progress, on_doc_complete);
    // Closed, not dropped: the next open of this store is a second
    // connection until this one is actually gone.
    block_on(db.close());
    rendered
}

fn render_open(
    db: &RawDb,
    source: &Source<'_>,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<FeedOutcome> {
    let Source {
        out_dir,
        name: source_id,
        account,
        account_inputs,
        range,
        ..
    } = *source;
    let pin = db.pin().expect("a reader is pinned at open").clone();
    let (rows, edges, changed) = block_on(async {
        let rows =
            datalib_etl::doltlite_raw::load_payloads_with_id_if_present(db.pool(), "connections")
                .await
                .context("load connections")?;
        // Photos, if any were fetched, keyed by the connection's URL.
        let edges = load_photo_edges(db).await?;
        let changed =
            changed_rows(db.pool(), range, &pin, &["connections", "contact_photos"]).await?;
        anyhow::Ok((rows, edges, changed))
    })?;

    // Each contact, and the hash of its photo's bytes when it has one.
    let (mut contacts, photo_of): (Vec<ContactDoc>, HashMap<String, String>) = {
        let mut photo_of = HashMap::new();
        let contacts = rows
            .iter()
            .map(|(row_id, p)| {
                let mut c = to_contact(source_id, p);
                let inputs = Inputs::default();
                inputs.read("connections", row_id);
                for input in account_inputs {
                    inputs.read(&input.table, &input.id);
                }
                if let Some(edge) = edges.get(row_id) {
                    inputs.read("contact_photos", &edge.row_id);
                    photo_of.insert(c.doc_uuid.clone(), edge.blake3.clone());
                }
                c.inputs = inputs.declared();
                c
            })
            .collect();
        (contacts, photo_of)
    };
    drop(rows);

    let mut outcome = narrow_docs(
        &mut contacts,
        contact_key,
        changed,
        range,
        pin.commit().to_string(),
    );
    let profile = ContactRenderProfile {
        provider: Provider::Linkedin,
        source_label: "LinkedIn".to_string(),
        contact_kind: "Contact".to_string(),
        contact_entity_kind: ids::KIND_CONNECTION,
        account: account.map(str::to_string),
        render_version: RENDER_VERSION,
    };
    let hashes: Vec<&str> = contacts
        .iter()
        .filter_map(|c| photo_of.get(&c.doc_uuid).map(String::as_str))
        .collect();
    let held = match db.cas() {
        Some(cas) => block_on(cas.describe_many(&hashes))?,
        None => HashMap::new(),
    };
    for c in &mut contacts {
        let Some(hash) = photo_of.get(&c.doc_uuid) else {
            continue;
        };
        match (db.cas(), held.get(hash)) {
            (Some(cas), Some(held)) => c.contact.photo = Some(photo::stored(cas.step(), held)),
            _ => c
                .problems
                .push(Problem::field("photo", Reason::BlobMissing, hash)),
        }
    }
    let summary = render_all(
        &profile,
        &contacts,
        out_dir,
        source_id,
        progress,
        on_doc_complete,
    )?;
    outcome.buckets.extend(summary.buckets);
    Ok(outcome)
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

fn contact_key(c: &ContactDoc) -> (&str, &[Input]) {
    (c.doc_uuid.as_str(), c.inputs.as_slice())
}

fn to_contact(source_id: &str, p: &Value) -> ContactDoc {
    let url = field(p, "URL");
    let name = full_name(p);

    // Identity from the profile URL (stable across re-exports). For the
    // rare row with no URL, fall back to name and company so distinct
    // people don't collapse onto one empty-URL id.
    let id = if !url.is_empty() {
        ids::connection(source_id, url)
    } else {
        ids::connection_without_url(source_id, &name, field(p, "Company"))
    };
    let nonempty = |col: &str| Some(field(p, col).to_string()).filter(|v| !v.is_empty());

    let mut person = NormalizedContact::new(source_id, id.natural_key.clone(), ContactKind::Person);
    person.names = (!name.is_empty()).then_some(name).into_iter().collect();
    person.org = nonempty("Company");
    person.title = nonempty("Position");
    person.handles = nonempty("Email Address")
        .map(|e| ContactHandle::email(None, e))
        .into_iter()
        .chain(Handle::linkedin_profile_url(url).map(ContactHandle::of))
        .collect();
    // The original `16 Jun 2026` stays readable beside the stamp the
    // grid sorts on.
    person.details = nonempty("Connected On")
        .map(|d| Detail::new("Connected On", d))
        .into_iter()
        .collect();
    person.created_at = connected_on_to_stamp(field(p, "Connected On"));
    person.source_url = (!url.is_empty()).then(|| url.to_string());
    ContactDoc {
        contact: person,
        doc_uuid: id.uuid,
        group_label: GROUP_LABEL.to_string(),
        member_handles: Vec::new(),
        upstream_account: None,
        inputs: Vec::new(),
        problems: Vec::new(),
    }
}

/// LinkedIn's `Connected On` is a bare `DD Mon YYYY` date (e.g.
/// `16 Jun 2026`) — no time, no zone. The grid's `created_at` must be RFC
/// 3339 with an explicit offset (the load step derives the sortable
/// `created_at_utc` column from it via `split_record_stamp`), so we fabricate
/// midnight UTC for that day. The fabrication is deliberate and mirrors
/// `datalib_time::parse_yyyy_mm_dd_assumed_utc`'s policy for
/// date-only inputs: the day stays correct for sorting and the invented
/// time-of-day is explicit. The original `16 Jun 2026` text still shows
/// in the contact's `Connected On` field (see `FIELD_COLUMNS`), so no
/// human-facing information is lost. Returns `None` for an empty /
/// unparseable value — a contact with no parseable date simply carries
/// no timestamp rather than a bogus one (we never fabricate the day).
fn connected_on_to_stamp(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    chrono::NaiveDate::parse_from_str(s, "%d %b %Y")
        .ok()
        .map(|d| format!("{}T00:00:00+00:00", d.format("%Y-%m-%d")))
}

fn full_name(p: &Value) -> String {
    let first = field(p, "First Name").trim();
    let last = field(p, "Last Name").trim();
    format!("{first} {last}").trim().to_string()
}

fn field<'a>(p: &'a Value, key: &str) -> &'a str {
    p.get(key).and_then(Value::as_str).unwrap_or("").trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn render(raw: &std::path::Path) -> Result<FeedOutcome> {
        let source = Source {
            raw_dir: raw,
            out_dir: raw,
            name: "linkedin",
            account: None,
            owner: None,
            account_inputs: &[],
            range: datalib_etl_render::inputs::RawRange {
                cursor: None,
                pin: None,
                stale: None,
            },
        };
        render_connections(&source, &Progress::noop(), &mut |_| Ok(()))
    }

    /// A connections table that failed to load read as an export without
    /// it, so every connection's document was rendered from nothing and
    /// swept, with the step reporting success. Only a table that does not
    /// exist is "no rows".
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_connections_table_that_will_not_load_fails_the_render() {
        let d = tempfile::tempdir().unwrap();
        let raw = d.path();
        let db = RawDb::open(&db_path_for(raw)).await.unwrap();
        datalib_etl::doltlite_raw::commit_run(db.pool(), "an export without connections")
            .await
            .unwrap();
        db.close().await;
        assert!(
            render(raw).is_ok(),
            "a table the export did not carry is no rows"
        );

        let db = RawDb::open(&db_path_for(raw)).await.unwrap();
        sqlx::query("CREATE TABLE connections (id TEXT PRIMARY KEY)")
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query("INSERT INTO connections (id) VALUES ('c1')")
            .execute(db.pool())
            .await
            .unwrap();
        datalib_etl::doltlite_raw::commit_run(db.pool(), "connections it cannot read")
            .await
            .unwrap();
        db.close().await;
        assert!(render(raw).is_err());
    }

    fn row() -> Value {
        json!({
            "First Name": "Angelica",
            "Last Name": "Lim, Ph.D.",
            "URL": "https://www.linkedin.com/in/angelicajeannelim",
            "Email Address": "",
            "Company": "Simon Fraser University",
            "Position": "Associate Professor",
            "Connected On": "16 Jun 2026",
        })
    }

    #[test]
    fn maps_connection_to_contact() {
        let c = to_contact("li", &row());
        assert_eq!(c.contact.name(), Some("Angelica Lim, Ph.D."));
        // Identity + web link both come from the profile URL.
        assert_eq!(
            c.doc_uuid,
            ids::connection("li", "https://www.linkedin.com/in/angelicajeannelim").uuid
        );
        assert_eq!(
            c.contact.source_url.as_deref(),
            Some("https://www.linkedin.com/in/angelicajeannelim")
        );
        assert_eq!(c.group_label, "Connections");
        // The bare `16 Jun 2026` date is normalized to an offset-bearing
        // RFC 3339 timestamp (midnight UTC) so the grid can sort/derive
        // `created_at_utc` from it — a raw `16 Jun 2026` would render
        // verbatim and sort wrong. The original text survives as the
        // `Connected On` field below.
        assert_eq!(
            c.contact.created_at.as_deref(),
            Some("2026-06-16T00:00:00+00:00")
        );
        assert_eq!(c.contact.org.as_deref(), Some("Simon Fraser University"));
        assert_eq!(c.contact.title.as_deref(), Some("Associate Professor"));
        // The profile is a handle; an empty Email Address is none.
        let handles: Vec<_> = c.contact.handles.iter().map(|h| h.handle.clone()).collect();
        assert_eq!(handles, [Handle::linkedin("angelicajeannelim")]);
        assert_eq!(c.contact.details[0].value, "16 Jun 2026");
    }

    #[test]
    fn connected_on_normalizes_to_offset_bearing_midnight_utc() {
        // The grid bug: a bare `DD Mon YYYY` date must become a valid
        // offset-bearing created_at, and the result must round-trip through
        // the same `split_record_stamp` the load step uses to build the
        // sortable `created_at_utc` column.
        let ts = connected_on_to_stamp("26 Oct 2020").unwrap();
        assert_eq!(ts, "2020-10-26T00:00:00+00:00");
        let (utc, offset) = datalib_time::split_record_stamp(&ts)
            .expect("normalized created_at is parseable by the load step");
        assert_eq!(utc, "2020-10-26T00:00:00.000000Z");
        assert_eq!(offset, "+00:00");

        // Empty / unparseable → no fabricated day.
        assert_eq!(connected_on_to_stamp(""), None);
        assert_eq!(connected_on_to_stamp("not a date"), None);
    }

    #[test]
    fn url_less_row_falls_back_to_name_hash() {
        let mut v = row();
        v["URL"] = json!("");
        let c = to_contact("li", &v);
        assert_eq!(c.doc_uuid.len(), 36);
        assert_eq!(c.contact.source_url, None);
        // The backpointer is what the id was minted from.
        assert_eq!(c.contact.key, "Angelica Lim, Ph.D.#Simon Fraser University");
        // Two different people don't collide on the empty URL.
        let mut other = row();
        other["URL"] = json!("");
        other["First Name"] = json!("Different");
        assert_ne!(c.doc_uuid, to_contact("li", &other).doc_uuid);
    }
}
