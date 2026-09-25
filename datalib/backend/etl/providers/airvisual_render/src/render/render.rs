//! Turn a whole AirVisual raw store into one markdown page plus its
//! plots, through the sensor page every time-series source shares.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::Result;
use datalib_etl::progress::Progress;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_timeseries_render::page::{Device, Page, PageProfile};
use datalib_etl_timeseries_render::text::{iso, thousands};
use datalib_id::IdNamespace;
use datalib_schema::providers::Provider;

pub use datalib_etl_timeseries_render::page::RenderSummary;

use super::parse::ParsedAirvisual;
use super::units::{METRICS, QUANTITIES};
use super::RENDER_VERSION;

pub const PROFILE: PageProfile = PageProfile {
    provider: Provider::Airvisual,
    tag: "airvisual",
    source_label: "AirVisual",
    id_namespace: IdNamespace::Airvisual,
    title_prefix: "AirVisual monitors",
    noun: "samples",
    intro: "Every value is as the device logged it; the sampling interval follows \
            the device's own sensor-mode schedule, so the markers are unevenly spaced \
            by design.",
    metric_word: "column",
    decimals: 1,
    metrics: METRICS,
    quantities: QUANTITIES,
    no_devices: "*(no devices)*",
    devices_preamble: None,
    orphan_key_word: "serial",
    devices_table: "airvisual_devices",
    orphan_hint: "the device was most likely given a different `serial` in the config.",
    render_version: RENDER_VERSION,
};

/// The page's `markdown_uuid`, keyed by the source id: there is no
/// AirVisual-side object behind it.
pub fn document_uuid(source_id: &str) -> String {
    PROFILE.document_uuid(source_id)
}

/// A device's row, keyed on its serial: IQAir issues those per unit.
pub fn device_uuid(source_id: &str, serial: &str) -> String {
    PROFILE.device_uuid(source_id, serial)
}

pub fn render_all(
    parsed: &ParsedAirvisual,
    root: &Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let page = Page {
        head: parsed.head.as_deref(),
        devices: parsed.devices.iter().map(device).collect(),
        series: &parsed.series,
        sample_count: parsed.sample_count,
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
    let mut facts: Vec<String> = vec![format!("serial `{}`", dev.id)];
    if let Some(m) = &dev.model {
        facts.push(format!("model {m}"));
    }
    if let (Some(a), Some(sv)) = (&dev.app_version, &dev.system_version) {
        facts.push(format!("firmware {a} / {sv}"));
    }
    if let Some(tz) = &dev.timezone {
        facts.push(format!("clock in {tz}"));
    }
    facts.push(match dev.last_ts_ms.and_then(iso) {
        Some(t) => format!("last sample {t}"),
        None => "no samples yet".to_string(),
    });
    let mut grid_text = format!("{} (serial {})", dev.name, dev.id);
    if let Some(m) = &dev.model {
        let _ = write!(grid_text, "\nAirVisual model {m}");
    }
    Device {
        key: dev.id.clone(),
        name: dev.name.clone(),
        facts: facts.join(" · "),
        grid_text,
        last_ts_ms: dev.last_ts_ms,
    }
}

/// The history files read, and nothing about the store's own history.
fn store_section(parsed: &ParsedAirvisual) -> String {
    let mut out = String::new();
    out.push_str("## Store\n\n");
    out.push_str("| | |\n| --- | --- |\n");
    let _ = writeln!(out, "| Samples | {} |", thousands(parsed.sample_count));
    let _ = writeln!(out, "| History files read | {} |", parsed.files.len());
    let _ = writeln!(
        out,
        "| History bytes read | {} |",
        thousands(parsed.files.iter().map(|f| f.size_bytes).sum())
    );
    out.push('\n');
    if !parsed.files.is_empty() {
        out.push_str("| File | Bytes |\n| --- | ---: |\n");
        for f in &parsed.files {
            let _ = writeln!(out, "| `{}` | {} |", f.rel_path, thousands(f.size_bytes));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_stable_and_source_scoped() {
        let a = document_uuid("air-cucina");
        assert_eq!(a, document_uuid("air-cucina"), "must be deterministic");
        assert_ne!(a, document_uuid("air-2"), "must be source-scoped");
        assert_ne!(
            device_uuid("air", "4133WV2JB9Z"),
            device_uuid("air", "QKAO9PC1XDJ")
        );
        assert_eq!(
            device_uuid("air", "4133WV2JB9Z"),
            device_uuid("air", "4133WV2JB9Z"),
            "a serial is the device wherever it is configured"
        );
        assert_ne!(
            document_uuid("air-cucina"),
            device_uuid("air-cucina", "air-cucina")
        );
    }

    #[test]
    fn unknown_metric_is_an_error_naming_the_fix() {
        let err = PROFILE.metric_spec("pressure_pa").unwrap_err().to_string();
        assert!(err.contains("pressure_pa"), "{err}");
        assert!(err.contains("units.rs"), "{err}");
    }
}
