//! Turn a whole YoLink raw store into one markdown page plus its plots,
//! through the sensor page every time-series source shares.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;
use datalib_etl::progress::Progress;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_timeseries_render::page::{Device, Page, PageProfile};
use datalib_etl_timeseries_render::text::{iso, pretty_json, thousands};
use datalib_id::IdNamespace;
use datalib_schema::providers::Provider;

pub use datalib_etl_timeseries_render::page::RenderSummary;

use super::parse::ParsedYolink;
use super::units::{METRICS, QUANTITIES};
use super::RENDER_VERSION;

pub const PROFILE: PageProfile = PageProfile {
    provider: Provider::Yolink,
    tag: "yolink",
    source_label: "YoLink",
    id_namespace: IdNamespace::Yolink,
    title_prefix: "YoLink sensors",
    noun: "readings",
    intro: "Values are converted to SI on the way into each plot, so devices \
            reporting in different units share one axis.",
    metric_word: "metric",
    decimals: 3,
    metrics: METRICS,
    quantities: QUANTITIES,
    no_devices: "*(no devices configured)*",
    // Device secrets stay out of the document on purpose — say so, so
    // nobody "fixes" the omission later.
    devices_preamble: Some(
        "Per-device read credentials (`family_device_id`, `device_udid`) are \
         deliberately omitted: the pair grants access to that device's entire \
         history.",
    ),
    orphan_key_word: "device",
    devices_table: "yolink_devices",
    orphan_hint: "they were most likely renamed or removed from the download config.",
    render_version: RENDER_VERSION,
};

/// The page's `markdown_uuid`, keyed by the source id: there is nothing
/// upstream behind it.
pub fn document_uuid(source_id: &str) -> String {
    PROFILE.document_uuid(source_id)
}

/// A device's row, keyed on its config name. The ids YoLink issues a
/// device (`device_udid`, `family_device_id`) are read secrets, and a
/// natural key is stored in `upstream_id` in the clear, so neither can
/// be the key.
pub fn device_uuid(source_id: &str, device: &str) -> String {
    PROFILE.device_uuid(source_id, device)
}

pub fn render_all(
    parsed: &ParsedYolink,
    root: &Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let page = Page {
        head: parsed.head.as_deref(),
        devices: parsed.devices.iter().map(device).collect(),
        series: &parsed.series,
        sample_count: parsed.reading_count,
        store_section: store_section(parsed),
    };
    datalib_etl_timeseries_render::page::render_all(
        &PROFILE,
        &page,
        root,
        source_id,
        progress,
        on_doc_complete,
    )
}

fn device(dev: &super::parse::DeviceRow) -> Device {
    let facts = format!(
        "{} · configured from {}{}",
        dev.kind,
        iso(dev.start_ms).unwrap_or_else(|| dev.start_ms.to_string()),
        match dev.last_ts_ms.and_then(iso) {
            Some(t) => format!(" · cursor at {t}"),
            None => " · no readings fetched yet".to_string(),
        },
    );
    Device {
        key: dev.name.clone(),
        name: dev.name.clone(),
        facts,
        grid_text: format!("{} ({})", dev.name, dev.kind),
        last_ts_ms: dev.last_ts_ms,
    }
}

/// The store's own provenance — the doltlite HEAD hash and the commit
/// log — is deliberately NOT rendered here, only the counts.
fn store_section(parsed: &ParsedYolink) -> String {
    let mut out = String::new();
    out.push_str("## Store\n\n");
    out.push_str("| | |\n| --- | --- |\n");
    let _ = writeln!(out, "| Readings | {} |", thousands(parsed.reading_count));
    let _ = writeln!(
        out,
        "| Readings with a recorded fetch error | {} |",
        thousands(parsed.reading_errors)
    );
    out.push('\n');

    for scope in &parsed.scope_config {
        let _ = writeln!(
            out,
            "### Configured scope — `{}`\n\n*Recorded {}.*\n\n```json\n{}\n```\n",
            scope.scope,
            scope.updated_at,
            pretty_json(&scope.config),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_stable_and_stanza_scoped() {
        let a = document_uuid("yolink");
        assert_eq!(a, document_uuid("yolink"), "must be deterministic");
        assert_ne!(a, document_uuid("yolink-2"), "must be stanza-scoped");
        assert_ne!(
            device_uuid("yolink", "fridge"),
            device_uuid("yolink", "freezer")
        );
        assert_ne!(document_uuid("yolink"), device_uuid("yolink", "fridge"));
    }

    #[test]
    fn unknown_metric_is_an_error_naming_the_fix() {
        let err = PROFILE.metric_spec("pressure_psi").unwrap_err().to_string();
        assert!(err.contains("pressure_psi"), "{err}");
        assert!(err.contains("units.rs"), "{err}");
    }
}
