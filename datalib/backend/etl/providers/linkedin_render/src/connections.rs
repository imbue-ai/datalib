//! Render LinkedIn `connections` as first-class contacts through the
//! shared [`datalib_etl_contact_common`] renderer.

use anyhow::Result;
use datalib_etl::progress::Progress;
use datalib_etl_contact_common::{
    render_all as cc_render_all, ContactField, ContactPhoto, ContactRenderProfile,
    NormalizedContact,
};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::{changed_rows, Bucket, Inputs};
use serde_json::Value;

use crate::ids;
use datalib_etl_linkedin::ingest::photos::load_photo_blobs;
use datalib_etl_linkedin::ingest::{db_path_for, RawDb};

use crate::processor::{FeedOutcome, Source};

use crate::render::RENDER_VERSION;
use datalib_schema::providers::Provider;

/// Human label + grouping for every LinkedIn connection.
const GROUP_LABEL: &str = "Connections";

/// Detail columns surfaced (in this order) as the contact's fields. The
/// name columns feed the title and `URL` feeds the web link, so they're
/// omitted here to avoid redundancy.
const FIELD_COLUMNS: &[&str] = &["Company", "Position", "Email Address", "Connected On"];

pub fn render_connections(
    source: &Source<'_>,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<FeedOutcome> {
    let Source {
        raw_dir,
        out_dir,
        name: source_id,
        account,
        account_inputs,
        range,
    } = *source;
    let db_path = db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(FeedOutcome::default());
    }
    let Some((rows, photos, changed, new_head)) = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            // Read at a commit: this store belongs to the download step, and
            // nothing committed means nothing to render from.
            let Some(db) = RawDb::open_reader(&db_path, range.pin).await? else {
                return Ok(None);
            };
            let pin = db.pin().expect("a reader is pinned at open").clone();
            // A user who excluded connections has no table; treat a load
            // error as "absent" rather than failing the whole render.
            let rows = datalib_etl::doltlite_raw::load_payloads_with_id(
                db.pool(),
                datalib_etl::pin::Reads::At(&pin),
                "connections",
            )
            .await
            .unwrap_or_default();
            // Photos, if any were fetched, keyed by the connection's URL.
            let photos = load_photo_blobs(&db, datalib_etl::pin::Reads::At(&pin))
                .await
                .unwrap_or_default();
            let changed =
                changed_rows(db.pool(), range, &pin, &["connections", "contact_photos"]).await?;
            // Closed, not dropped: the next open of this store is a
            // second connection until this one is actually gone.
            db.close().await;
            Ok::<_, anyhow::Error>(Some((rows, photos, changed, pin.commit().to_string())))
        })
    })?
    else {
        return Ok(FeedOutcome::default());
    };

    let mut contacts: Vec<NormalizedContact> = rows
        .iter()
        .map(|(row_id, p)| {
            let mut c = to_contact(source_id, p);
            let inputs = Inputs::default();
            inputs.read("connections", row_id);
            for input in account_inputs {
                inputs.read(&input.table, &input.id);
            }
            if let Some(photo) = photos.get(row_id) {
                inputs.read("contact_photos", &photo.row_id);
                c.photo = Some(ContactPhoto {
                    bytes: photo.bytes.clone(),
                    content_type: photo
                        .content_type
                        .clone()
                        .unwrap_or_else(|| "application/octet-stream".to_string()),
                });
            }
            c.inputs = inputs.declared();
            c
        })
        .collect();

    // What to render: the contacts the driver found stale, plus the ones
    // a new or changed row maps to through the rows just loaded. A
    // removed row's contact reaches here through the driver, having
    // declared the row.
    let forward = changed.map(|changed| {
        contacts
            .iter()
            .filter(|c| {
                c.inputs
                    .iter()
                    .any(|i| changed.get(&i.table).is_some_and(|ids| ids.contains(&i.id)))
            })
            .map(|c| c.contact_uuid.clone())
            .collect::<std::collections::HashSet<String>>()
    });
    let render = range.narrow(forward.as_ref());
    let mut outcome = FeedOutcome {
        new_head: Some(new_head),
        buckets: render
            .iter()
            .flatten()
            .map(|key| Bucket {
                key: key.clone(),
                inputs: Vec::new(),
            })
            .collect(),
    };
    if let Some(render) = &render {
        contacts.retain(|c| render.contains(&c.contact_uuid));
    }
    let profile = ContactRenderProfile {
        provider: Provider::Linkedin,
        source_label: "LinkedIn".to_string(),
        contact_kind: "Contact".to_string(),
        contact_entity_kind: ids::KIND_CONNECTION,
        account: account.map(str::to_string),
        render_version: RENDER_VERSION,
    };
    let s = cc_render_all(
        &profile,
        &contacts,
        out_dir,
        source_id,
        progress,
        on_doc_complete,
    )?;
    outcome.buckets.extend(s.buckets);
    Ok(outcome)
}

fn to_contact(source_id: &str, p: &Value) -> NormalizedContact {
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

    let fields: Vec<ContactField> = FIELD_COLUMNS
        .iter()
        .filter_map(|col| {
            let v = field(p, col);
            (!v.is_empty()).then(|| ContactField::new(*col, v.to_string()))
        })
        .collect();

    NormalizedContact {
        inputs: Vec::new(),
        contact_uuid: id.uuid,
        group_uuid: ids::connections_group(source_id).uuid,
        group_label: GROUP_LABEL.to_string(),
        display_name: (!name.is_empty()).then_some(name),
        external_id: Some(id.natural_key),
        upstream_account: None,
        created_at: connected_on_to_stamp(field(p, "Connected On")),
        modified_at: None,
        source_url: (!url.is_empty()).then(|| url.to_string()),
        fields,
        photo: None,
        photo_url: None,
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
        assert_eq!(c.display_name.as_deref(), Some("Angelica Lim, Ph.D."));
        // Identity + web link both come from the profile URL.
        assert_eq!(
            c.contact_uuid,
            ids::connection("li", "https://www.linkedin.com/in/angelicajeannelim").uuid
        );
        assert_eq!(
            c.source_url.as_deref(),
            Some("https://www.linkedin.com/in/angelicajeannelim")
        );
        assert_eq!(c.group_label, "Connections");
        // The bare `16 Jun 2026` date is normalized to an offset-bearing
        // RFC 3339 timestamp (midnight UTC) so the grid can sort/derive
        // `created_at_utc` from it — a raw `16 Jun 2026` would render
        // verbatim and sort wrong. The original text survives as the
        // `Connected On` field below.
        assert_eq!(c.created_at.as_deref(), Some("2026-06-16T00:00:00+00:00"));
        // Empty Email Address is dropped; the rest are fields in order.
        let labels: Vec<&str> = c.fields.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(labels, vec!["Company", "Position", "Connected On"]);
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
        assert_eq!(c.contact_uuid.len(), 36);
        assert_eq!(c.source_url, None);
        // The backpointer is what the id was minted from.
        assert_eq!(
            c.external_id.as_deref(),
            Some("Angelica Lim, Ph.D.#Simon Fraser University")
        );
        // Two different people don't collide on the empty URL.
        let mut other = row();
        other["URL"] = json!("");
        other["First Name"] = json!("Different");
        assert_ne!(c.contact_uuid, to_contact("li", &other).contact_uuid);
    }
}
