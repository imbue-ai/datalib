//! Render LinkedIn `connections` as first-class contacts through the
//! shared [`datalib_etl_contact_common`] renderer.

use datalib_etl_render::processor::RenderPass;
use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use datalib_etl::progress::Progress;
use datalib_etl_contact_common::{
    render_all as cc_render_all, ContactField, ContactPhoto, ContactRenderProfile,
    NormalizedContact,
};
use datalib_etl_render::grid_index::RenderedMarkdown;
use serde_json::Value;

use datalib_etl_linkedin::download::photos::load_photo_blobs;
use datalib_etl_linkedin::download::schema_raw::{connection_uuid, ns_id};
use datalib_etl_linkedin::download::{db_path_for, RawDb};

use crate::render::RENDER_VERSION;
use datalib_schema::providers::Provider;

/// Human label + grouping for every LinkedIn connection.
const GROUP_LABEL: &str = "Connections";

/// Detail columns surfaced (in this order) as the contact's fields. The
/// name columns feed the title and `URL` feeds the web link, so they're
/// omitted here to avoid redundancy.
const FIELD_COLUMNS: &[&str] = &["Company", "Position", "Email Address", "Connected On"];

pub fn render_connections(
    raw_dir: &Path,
    out_dir: &Path,
    source_name: &str,
    progress: &Progress,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
    // Every document this render considered, skipped ones included — the
    // caller hands it to `RunCtx::retain_documents`, which drops whatever
    // the store holds and this does not name.
    seen: &mut std::collections::HashSet<String>,
) -> Result<RenderPass> {
    let db_path = db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(RenderPass::Skipped);
    }
    let Some((payloads, photos)) = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let db = RawDb::open_reader(&db_path).await?;
            // Read at a commit: this store belongs to the download step, and
            // nothing committed means nothing to render from.
            let Some(pin) = datalib_etl::pin::head(db.pool()).await? else {
                db.close().await;
                // `None` all the way out, not an empty value: an empty load is
                // indistinguishable from a source with nothing in it, and the
                // caller sweeps every document this pass did not name.
                return Ok(None);
            };
            datalib_etl::pin::install_views(db.pool(), &pin).await?;
            // A user who excluded connections has no table; treat a load
            // error as "absent" rather than failing the whole render.
            let payloads = db
                .load_payloads(datalib_etl::pin::Reads::At(&pin), "connections")
                .await
                .unwrap_or_default();
            // Photos, if any were fetched, keyed by connection_uuid.
            let photos = load_photo_blobs(&db, datalib_etl::pin::Reads::At(&pin))
                .await
                .unwrap_or_default();
            // Closed, not dropped: the next open of this store is a
            // second connection until this one is actually gone.
            db.close().await;
            Ok::<_, anyhow::Error>(Some((payloads, photos)))
        })
    })?
    else {
        // Nothing committed to read: this pass did not walk, so it must not
        // reach the retain sweep.
        return Ok(RenderPass::Skipped);
    };

    let contacts: Vec<NormalizedContact> = payloads
        .iter()
        .map(|p| {
            let mut c = to_contact(p);
            if let Some((bytes, content_type)) = photos.get(&c.contact_uuid) {
                c.photo = Some(ContactPhoto {
                    bytes: bytes.clone(),
                    content_type: content_type
                        .clone()
                        .unwrap_or_else(|| "application/octet-stream".to_string()),
                });
            }
            c
        })
        .collect();
    let profile = ContactRenderProfile {
        provider: Provider::Linkedin,
        source_label: "LinkedIn".to_string(),
        contact_kind: "Contact".to_string(),
        render_version: RENDER_VERSION,
    };
    let s = cc_render_all(
        &profile,
        &contacts,
        out_dir,
        source_name,
        progress,
        prior_fingerprints,
        on_doc_complete,
    )?;
    seen.extend(s.documents);
    Ok(RenderPass::Walked)
}

fn to_contact(p: &Value) -> NormalizedContact {
    let url = field(p, "URL");
    let name = full_name(p);

    // Identity from the profile URL (stable across re-exports). For the
    // rare row with no URL, fall back to a name+company hash so distinct
    // people don't collapse onto one empty-URL id.
    let contact_uuid = if !url.is_empty() {
        connection_uuid(url)
    } else {
        ns_id(&format!(
            "connection-nourl:{}:{}",
            name,
            field(p, "Company")
        ))
    };

    let fields: Vec<ContactField> = FIELD_COLUMNS
        .iter()
        .filter_map(|col| {
            let v = field(p, col);
            (!v.is_empty()).then(|| ContactField::new(*col, v.to_string()))
        })
        .collect();

    NormalizedContact {
        contact_uuid,
        group_uuid: ns_id("group:connections"),
        group_label: GROUP_LABEL.to_string(),
        display_name: (!name.is_empty()).then_some(name),
        external_id: (!url.is_empty()).then(|| url.to_string()),
        when_ts: connected_on_to_when_ts(field(p, "Connected On")),
        source_url: (!url.is_empty()).then(|| url.to_string()),
        fields,
        photo: None,
        photo_url: None,
    }
}

/// LinkedIn's `Connected On` is a bare `DD Mon YYYY` date (e.g.
/// `16 Jun 2026`) — no time, no zone. The grid's `when_ts` must be RFC
/// 3339 with an explicit offset (the load step derives the sortable
/// `when_ts_utc` column from it via `split_when_ts`), so we fabricate
/// midnight UTC for that day. The fabrication is deliberate and mirrors
/// `datalib_time::parse_yyyy_mm_dd_assumed_utc`'s policy for
/// date-only inputs: the day stays correct for sorting and the invented
/// time-of-day is explicit. The original `16 Jun 2026` text still shows
/// in the contact's `Connected On` field (see `FIELD_COLUMNS`), so no
/// human-facing information is lost. Returns `None` for an empty /
/// unparseable value — a contact with no parseable date simply carries
/// no timestamp rather than a bogus one (we never fabricate the day).
fn connected_on_to_when_ts(s: &str) -> Option<String> {
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
        let c = to_contact(&row());
        assert_eq!(c.display_name.as_deref(), Some("Angelica Lim, Ph.D."));
        // Identity + web link both come from the profile URL.
        assert_eq!(
            c.contact_uuid,
            connection_uuid("https://www.linkedin.com/in/angelicajeannelim")
        );
        assert_eq!(
            c.source_url.as_deref(),
            Some("https://www.linkedin.com/in/angelicajeannelim")
        );
        assert_eq!(c.group_label, "Connections");
        // The bare `16 Jun 2026` date is normalized to an offset-bearing
        // RFC 3339 timestamp (midnight UTC) so the grid can sort/derive
        // `when_ts_utc` from it — a raw `16 Jun 2026` would render
        // verbatim and sort wrong. The original text survives as the
        // `Connected On` field below.
        assert_eq!(c.when_ts.as_deref(), Some("2026-06-16T00:00:00+00:00"));
        // Empty Email Address is dropped; the rest are fields in order.
        let labels: Vec<&str> = c.fields.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(labels, vec!["Company", "Position", "Connected On"]);
    }

    #[test]
    fn connected_on_normalizes_to_offset_bearing_midnight_utc() {
        // The grid bug: a bare `DD Mon YYYY` date must become a valid
        // offset-bearing when_ts, and the result must round-trip through
        // the same `split_when_ts` the load step uses to build the
        // sortable `when_ts_utc` column.
        let ts = connected_on_to_when_ts("26 Oct 2020").unwrap();
        assert_eq!(ts, "2020-10-26T00:00:00+00:00");
        let (utc, offset) = datalib_time::split_when_ts(&ts)
            .expect("normalized when_ts is parseable by the load step");
        assert_eq!(utc, "2020-10-26T00:00:00.000000Z");
        assert_eq!(offset, "+00:00");

        // Empty / unparseable → no fabricated day.
        assert_eq!(connected_on_to_when_ts(""), None);
        assert_eq!(connected_on_to_when_ts("not a date"), None);
    }

    #[test]
    fn url_less_row_falls_back_to_name_hash() {
        let mut v = row();
        v["URL"] = json!("");
        let c = to_contact(&v);
        assert_eq!(c.contact_uuid.len(), 36);
        assert_eq!(c.source_url, None);
        assert_eq!(c.external_id, None);
        // Two different people don't collide on the empty URL.
        let mut other = row();
        other["URL"] = json!("");
        other["First Name"] = json!("Different");
        assert_ne!(c.contact_uuid, to_contact(&other).contact_uuid);
    }
}
