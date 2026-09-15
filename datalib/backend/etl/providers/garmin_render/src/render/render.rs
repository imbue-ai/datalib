//! Turn a Garmin raw store into one markdown page plus its weight plot.

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::progress::Progress;
use datalib_etl::title::Title;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_schema::grid_rows::GridRow;
use datalib_schema::providers::Provider;
use datalib_schema::render_problems::RenderProblemRow;
use once_cell::sync::Lazy;
use uuid::Uuid;

use super::parse::ParsedGarmin;
use super::plot::weight_html;
use super::RENDER_VERSION;

/// Namespace for every UUIDv5 this renderer mints. Fixed and arbitrary.
pub static GARMIN_UUID_NS: Lazy<Uuid> = Lazy::new(|| {
    Uuid::parse_str("2f8c5b1e-3a4d-5e6f-8a9b-7c6d5e4f0002").expect("valid garmin ns uuid")
});

/// The page's `markdown_uuid`: one page per source, stable across every
/// re-render.
pub fn document_uuid(source_id: &str) -> String {
    Uuid::new_v5(
        &GARMIN_UUID_NS,
        format!("garmin:{source_id}:weight").as_bytes(),
    )
    .to_string()
}

pub fn device_uuid(source_id: &str, device_id: &str) -> String {
    Uuid::new_v5(
        &GARMIN_UUID_NS,
        format!("garmin:{source_id}:device:{device_id}").as_bytes(),
    )
    .to_string()
}

/// How many of the newest weigh-ins the table shows. The plot has all
/// of them; the table is for reading the recent ones off.
const TABLE_ROWS: usize = 30;

#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub weigh_ins: usize,
    pub devices: usize,
    pub plots: usize,
}

/// The raw store's HEAD moves on every ingest (the bookkeeping stamps
/// alone see to that), so the page is rendered on every run the ingest
/// ran; an unchanged page writes identical rows, which the render store
/// records as no change.
pub fn render_all(
    parsed: &ParsedGarmin,
    root: &Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let page_dir = datalib_etl::layout::render_markdown_root(root, source_id);
    let plots_dir = page_dir.join("plots");
    fs::create_dir_all(&plots_dir).with_context(|| format!("mkdir -p {}", plots_dir.display()))?;
    progress.set_length(Some(2));

    let mut summary = RenderSummary {
        weigh_ins: parsed.weigh_ins.len(),
        devices: parsed.devices.len(),
        ..Default::default()
    };
    let plot_file = if parsed.weigh_ins.is_empty() {
        None
    } else {
        let subtitle = format!(
            "{} weigh-ins · {} — {}",
            parsed.weigh_ins.len(),
            short(parsed.weigh_ins[0].timestamp_gmt_ms),
            short(parsed.weigh_ins[parsed.weigh_ins.len() - 1].timestamp_gmt_ms),
        );
        let html = weight_html("Weight", &subtitle, &parsed.weigh_ins)?;
        let path = plots_dir.join("weight.html");
        fs::write(&path, html).with_context(|| format!("write {}", path.display()))?;
        summary.plots = 1;
        Some("weight.html")
    };
    progress.inc(1);

    let m_uuid = document_uuid(source_id);
    let body = render_markdown(parsed, source_id, &m_uuid, plot_file);
    let md_path = page_dir.join("index.md");
    fs::write(&md_path, body).with_context(|| format!("write {}", md_path.display()))?;
    let md_rel = md_path
        .strip_prefix(root)
        .unwrap_or(&md_path)
        .to_string_lossy()
        .into_owned();

    let mut problems: Vec<RenderProblemRow> = Vec::new();
    let rows = build_grid_rows(parsed, source_id, &m_uuid, &md_rel, &mut problems);
    on_doc_complete(RenderedMarkdown {
        markdown_uuid: m_uuid.clone(),
        source_id: source_id.to_string(),
        // Not the raw HEAD: it moves on every ingest, and a row whose
        // content did not change may carry nothing per-run.
        upstream_cursor: None,
        bucket_key: Some(m_uuid.clone()),
        md_path,
        render_version: RENDER_VERSION,
        rows,
        edges: Vec::new(),
        problems,
    })
    .with_context(|| format!("on_doc_complete {m_uuid}"))?;
    progress.inc(1);
    if parsed.head.is_none() {
        tracing::warn!(
            event = "garmin_render_no_head",
            source = source_id,
            "dolt_log() returned no HEAD; the render cursor stays put and the next run re-renders"
        );
    }
    Ok(summary)
}

fn render_markdown(
    parsed: &ParsedGarmin,
    source_id: &str,
    m_uuid: &str,
    plot_file: Option<&str>,
) -> String {
    let mut out = String::with_capacity(8 * 1024);
    let title = page_title(parsed, source_id);
    let created_at = parsed
        .latest_weigh_in()
        .and_then(|w| iso(w.timestamp_gmt_ms));

    out.push_str("---\n");
    let _ = writeln!(out, "markdown_uuid: {m_uuid}");
    let _ = writeln!(out, "source_id: {source_id}");
    out.push_str("provider: garmin\n");
    let _ = writeln!(out, "title: {}", yaml_safe(&title));
    if let Some(ts) = &created_at {
        let _ = writeln!(out, "created_at: {}", yaml_safe(ts));
    }
    out.push_str("---\n\n");
    out.push_str(
        &Title {
            suffix: None,
            text: &title,
            markdown_uuid: Some(m_uuid),
            source_url: None,
        }
        .render(),
    );

    render_weight_section(&mut out, parsed, plot_file);
    render_device_section(&mut out, parsed, source_id);
    render_store_section(&mut out, parsed);
    out
}

fn render_weight_section(out: &mut String, parsed: &ParsedGarmin, plot_file: Option<&str>) {
    out.push_str("## Weight\n\n");
    let Some(latest) = parsed.latest_weigh_in() else {
        out.push_str("*(no weigh-ins mirrored yet)*\n\n");
        return;
    };
    let first = &parsed.weigh_ins[0];
    let _ = writeln!(
        out,
        "**{:.1} kg** on {} · {} weigh-ins since {}{}.\n",
        latest.weight_kg,
        short(latest.timestamp_gmt_ms),
        parsed.weigh_ins.len(),
        short(first.timestamp_gmt_ms),
        match latest.body_fat_pct {
            Some(f) => format!(" · {f:.1} % body fat"),
            None => String::new(),
        }
    );
    if let Some(file) = plot_file {
        // Relative `src`, so the page works opened straight off disk and
        // under a static file server; the UI rewrites it to the asset
        // route when it renders the body.
        let _ = writeln!(
            out,
            "<iframe src=\"plots/{file}\" title=\"Weight\" width=\"100%\" height=\"520\" \
             loading=\"lazy\" sandbox=\"allow-scripts allow-downloads\" \
             style=\"border:1px solid rgba(128,128,128,.35);border-radius:6px\">\
             </iframe>\n"
        );
        let _ = writeln!(out, "[Open the weight plot on its own](plots/{file})\n");
    }
    let _ = writeln!(
        out,
        "### Latest {} weigh-ins\n",
        TABLE_ROWS.min(parsed.weigh_ins.len())
    );
    out.push_str("| When (UTC) | kg | BMI | Body fat | Source |\n");
    out.push_str("| --- | ---: | ---: | ---: | --- |\n");
    for w in parsed.weigh_ins.iter().rev().take(TABLE_ROWS) {
        let _ = writeln!(
            out,
            "| {} | {:.1} | {} | {} | {} |",
            short(w.timestamp_gmt_ms),
            w.weight_kg,
            w.bmi.map(|b| format!("{b:.1}")).unwrap_or_default(),
            w.body_fat_pct
                .map(|f| format!("{f:.1} %"))
                .unwrap_or_default(),
            w.source_type.as_deref().unwrap_or(""),
        );
    }
    out.push('\n');
}

fn render_device_section(out: &mut String, parsed: &ParsedGarmin, source_id: &str) {
    out.push_str("## Devices\n\n");
    if parsed.devices.is_empty() {
        out.push_str("*(no devices registered)*\n\n");
        return;
    }
    for d in &parsed.devices {
        let uuid = device_uuid(source_id, &d.id);
        let _ = writeln!(
            out,
            "<div id=\"m-{uuid}\" data-section-uuid=\"{uuid}\" class=\"msg msg--garmin\">\n"
        );
        let _ = writeln!(out, "### {}\n", d.name);
        let _ = writeln!(
            out,
            "*device {}{}*\n",
            d.id,
            d.last_sync
                .as_deref()
                .map(|s| format!(" · last synced {s}"))
                .unwrap_or_default()
        );
        out.push_str("</div>\n\n");
    }
}

/// Counts only — nothing here should change when the store is merely
/// re-committed, so the page stays byte-identical across no-op runs.
fn render_store_section(out: &mut String, parsed: &ParsedGarmin) {
    out.push_str("## Store\n\n");
    out.push_str("| | |\n| --- | --- |\n");
    let _ = writeln!(out, "| Weigh-ins | {} |", parsed.weigh_ins.len());
    let _ = writeln!(out, "| Activities | {} |", parsed.activities);
    let _ = writeln!(out, "| Activity FIT files | {} |", parsed.activity_files);
    let _ = writeln!(
        out,
        "| Account listings (records, gear, badges, workouts, goals) | {} |",
        parsed.items
    );
    out.push('\n');
    if !parsed.metrics.is_empty() {
        out.push_str("### Per-day metrics\n\n| Metric | Days walked | Days with data |\n| --- | ---: | ---: |\n");
        for m in &parsed.metrics {
            let _ = writeln!(
                out,
                "| `{}` | {} | {} |",
                m.metric, m.days, m.days_with_data
            );
        }
        out.push('\n');
    }
}

/// One row for the page plus one per device.
fn build_grid_rows(
    parsed: &ParsedGarmin,
    source_id: &str,
    m_uuid: &str,
    md_rel: &str,
    problems: &mut Vec<RenderProblemRow>,
) -> Vec<GridRow> {
    let title = page_title(parsed, source_id);
    let mut text = format!("{title}\n{} weigh-ins", parsed.weigh_ins.len());
    if let Some(w) = parsed.latest_weigh_in() {
        let _ = write!(
            text,
            "\nlatest {:.1} kg on {}",
            w.weight_kg,
            short(w.timestamp_gmt_ms)
        );
    }
    let mut rows: Vec<GridRow> = GridRow::builder()
        .uuid(m_uuid.to_string())
        .provider(Provider::Garmin)
        .kind("Garmin Weight")
        .source_label("Garmin")
        .created_at(
            parsed
                .latest_weigh_in()
                .and_then(|w| iso(w.timestamp_gmt_ms)),
        )
        .author(parsed.full_name.clone())
        .conversation_name(Some(title.clone()))
        .conversation_uuid(m_uuid.to_string())
        .entire_chat(format!("/chat/{m_uuid}"))
        .text(text)
        .qmd_path(Some(md_rel.to_string()))
        .markdown_uuid(Some(m_uuid.to_string()))
        .build_or_record(source_id, m_uuid, RENDER_VERSION, problems)
        .into_iter()
        .collect();
    for (idx, d) in parsed.devices.iter().enumerate() {
        rows.extend(
            GridRow::builder()
                .uuid(device_uuid(source_id, &d.id))
                .provider(Provider::Garmin)
                .kind("Garmin Device")
                .source_label("Garmin")
                .created_at(d.last_sync.as_deref().and_then(garmin_stamp_to_iso))
                .channel(Some(d.name.clone()))
                .conversation_name(Some(title.clone()))
                .conversation_uuid(m_uuid.to_string())
                .message_index(Some(idx as i64))
                .entire_chat(format!("/chat/{m_uuid}"))
                .text(format!("{} (device {})", d.name, d.id))
                .qmd_path(Some(md_rel.to_string()))
                .upstream_id(Some(d.id.clone()))
                .upstream_entity_kind(Some("device".to_string()))
                .markdown_uuid(Some(m_uuid.to_string()))
                .build_or_record(source_id, m_uuid, RENDER_VERSION, problems),
        );
    }
    rows
}

fn page_title(parsed: &ParsedGarmin, source_id: &str) -> String {
    match parsed
        .full_name
        .as_deref()
        .or(parsed.display_name.as_deref())
    {
        Some(name) => format!("Garmin — {name}"),
        None => format!("Garmin — {source_id}"),
    }
}

fn iso(ms: i64) -> Option<String> {
    datalib_time::IsoOffsetTimestamp::from_unix_millis(ms).map(|t| t.to_rfc3339())
}

fn short(ms: i64) -> String {
    datalib_time::IsoOffsetTimestamp::from_unix_millis(ms)
        .map(|t| t.inner().format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ms.to_string())
}

/// Garmin writes device stamps as `2026-09-14T05:12:44.0` with no
/// offset, and the field name says GMT — the one place assuming UTC is
/// backed by the source itself.
fn garmin_stamp_to_iso(s: &str) -> Option<String> {
    datalib_time::parse_with_assumed_utc(s)
        .ok()
        .map(|t| t.to_rfc3339())
}

fn yaml_safe(s: &str) -> String {
    if s.chars().any(|c| ":#[]{}&*?,|>'\"%@`\n".contains(c)) {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_stable_and_source_scoped() {
        assert_eq!(document_uuid("garmin"), document_uuid("garmin"));
        assert_ne!(document_uuid("garmin"), document_uuid("garmin-2"));
        assert_ne!(device_uuid("garmin", "1"), device_uuid("garmin", "2"));
        assert_ne!(document_uuid("garmin"), device_uuid("garmin", "1"));
    }

    #[test]
    fn a_garmin_device_stamp_is_read_as_utc() {
        assert_eq!(
            garmin_stamp_to_iso("2369-04-15T05:12:44.0").as_deref(),
            Some("2369-04-15T05:12:44+00:00")
        );
        assert_eq!(garmin_stamp_to_iso("yesterday"), None);
    }
}
