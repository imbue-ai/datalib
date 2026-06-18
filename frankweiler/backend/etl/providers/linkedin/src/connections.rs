//! Render LinkedIn `connections` as first-class contacts through the
//! shared [`frankweiler_etl_contact_common`] renderer.
//!
//! Each row of the `connections` raw table (one per 1st-degree
//! connection; see `Connections.csv`) becomes one
//! [`NormalizedContact`]: identity is a UUID derived from the member's
//! profile URL ([`schema_raw::connection_uuid`]), the URL is also the
//! contact's canonical web link, and the remaining columns (Company,
//! Position, Email, Connected On) become the detail fields. They all
//! share a single "Connections" group.
//!
//! Future enhancement: fetch each connection's profile picture and store
//! it in blob_cas, then set [`NormalizedContact::photo`] — the renderer
//! already materializes photos for vCard contacts the same way. For now
//! we have no picture bytes, so the photo stays unset.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl_contact_common::{
    render_all as cc_render_all, ContactField, ContactPhoto, ContactRenderProfile,
    NormalizedContact,
};
use serde_json::Value;

use crate::extract::photos::load_photo_blobs;
use crate::extract::schema_raw::{connection_uuid, ns_id};
use crate::extract::{db_path_for, RawDb};

/// Bump when the connection → contact mapping changes meaningfully.
const RENDER_VERSION: u32 = 1;

/// Human label + grouping for every LinkedIn connection.
const GROUP_LABEL: &str = "Connections";

/// Detail columns surfaced (in this order) as the contact's fields. The
/// name columns feed the title and `URL` feeds the web link, so they're
/// omitted here to avoid redundancy.
const FIELD_COLUMNS: &[&str] = &["Company", "Position", "Email Address", "Connected On"];

/// Render the `connections` table under `raw_dir` into `out_dir`. No-op
/// when the raw store (or the `connections` table) is absent / empty.
pub fn render_connections(
    raw_dir: &Path,
    out_dir: &Path,
    source_name: &str,
    progress: &Progress,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<()> {
    let db_path = db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(());
    }
    let (payloads, photos) = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let db = RawDb::open(&db_path).await?;
            // A user who excluded connections has no table; treat a load
            // error as "absent" rather than failing the whole render.
            let payloads = db.load_payloads("connections").await.unwrap_or_default();
            // Photos, if any were fetched, keyed by connection_uuid.
            let photos = load_photo_blobs(&db, &db_path).await.unwrap_or_default();
            Ok::<_, anyhow::Error>((payloads, photos))
        })
    })?;

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
        provider: "linkedin",
        source_label: "LinkedIn".to_string(),
        contact_kind: "Contact".to_string(),
        render_version: RENDER_VERSION,
    };
    cc_render_all(
        &profile,
        &contacts,
        out_dir,
        source_name,
        progress,
        prior_fingerprints,
        on_doc_complete,
    )?;
    Ok(())
}

/// One `connections` payload row → one [`NormalizedContact`].
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
        when_ts: nonempty(field(p, "Connected On")).map(str::to_string),
        source_url: (!url.is_empty()).then(|| url.to_string()),
        fields,
        photo: None,
        photo_url: None,
    }
}

fn full_name(p: &Value) -> String {
    let first = field(p, "First Name").trim();
    let last = field(p, "Last Name").trim();
    format!("{first} {last}").trim().to_string()
}

fn field<'a>(p: &'a Value, key: &str) -> &'a str {
    p.get(key).and_then(Value::as_str).unwrap_or("").trim()
}

fn nonempty(s: &str) -> Option<&str> {
    (!s.is_empty()).then_some(s)
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
        assert_eq!(c.when_ts.as_deref(), Some("16 Jun 2026"));
        // Empty Email Address is dropped; the rest are fields in order.
        let labels: Vec<&str> = c.fields.iter().map(|f| f.label.as_str()).collect();
        assert_eq!(labels, vec!["Company", "Position", "Connected On"]);
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
