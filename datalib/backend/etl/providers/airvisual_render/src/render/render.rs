//! Turn a whole AirVisual raw store into one markdown page plus its plots.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::progress::Progress;
use datalib_etl::title::Title;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_timeseries_render::plot::{standalone_html, Trace};
use datalib_etl_timeseries_render::text::{
    human_gap, iso, median_gap, short, short_ts, thousands, yaml_safe,
};
use datalib_id::{entity_id_str, IdNamespace, Scope};
use datalib_schema::grid_rows::GridRow;
use datalib_schema::problems::ProblemRow;
use datalib_schema::providers::Provider;

use super::parse::{ParsedAirvisual, Series};
use super::units::{self, series_label, spec_for, Quantity, QUANTITIES};
use super::RENDER_VERSION;

const ID_NAMESPACE: IdNamespace = IdNamespace::Airvisual;
const SOURCE_LABEL: &str = "AirVisual";

/// The page's `markdown_uuid`. There is exactly one page per source and
/// no AirVisual-side object behind it, so the scope is the source id —
/// the `SourceInstance` case `entity_ids.md` reserves for exactly this.
pub fn document_uuid(source_id: &str) -> String {
    entity_id_str(
        ID_NAMESPACE,
        Scope::SourceInstance(source_id),
        "timeseries",
        source_id,
    )
}

/// A device's row, keyed on its serial: IQAir issues those per unit, so
/// the same Pro configured in two sources is one device — and
/// `IdClaims` will say so rather than let one source's row erase the
/// other's.
pub fn device_uuid(serial: &str) -> String {
    entity_id_str(ID_NAMESPACE, Scope::ProviderGlobal, "device", serial)
}

#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub devices: usize,
    pub series: usize,
    pub points: usize,
    pub plots: usize,
}

pub fn render_all(
    parsed: &ParsedAirvisual,
    root: &Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let page_dir = datalib_etl::layout::render_markdown_root(root, source_id);
    let plots_dir = page_dir.join("plots");
    fs::create_dir_all(&plots_dir).with_context(|| format!("mkdir -p {}", plots_dir.display()))?;

    let mut summary = RenderSummary {
        devices: parsed.devices.len(),
        series: parsed.series.len(),
        points: parsed.series.iter().map(Series::len).sum(),
        ..Default::default()
    };
    progress.set_length(Some((QUANTITIES.len() + 1) as u64));

    // Plots first: the markdown links to whatever actually got written,
    // so a quantity with no data yields no iframe rather than a broken
    // one.
    let mut rendered_plots: Vec<(&Quantity, PlotFacts)> = Vec::new();
    for quantity in QUANTITIES {
        progress.set_message(&format!("plot {}", quantity.key));
        if let Some(facts) = render_plot(parsed, quantity, &plots_dir)? {
            summary.plots += 1;
            rendered_plots.push((quantity, facts));
        }
        progress.inc(1);
    }

    let m_uuid = document_uuid(source_id);
    let body = render_markdown(parsed, source_id, &m_uuid, &rendered_plots);

    let md_path = page_dir.join("index.md");
    fs::write(&md_path, body).with_context(|| format!("write {}", md_path.display()))?;

    let md_rel = md_path
        .strip_prefix(root)
        .unwrap_or(&md_path)
        .to_string_lossy()
        .into_owned();
    let mut problems: Vec<ProblemRow> = Vec::new();
    let rows = build_grid_rows(parsed, source_id, &m_uuid, &md_rel, &mut problems);

    on_doc_complete(RenderedMarkdown {
        markdown_uuid: m_uuid.clone(),
        source_id: source_id.to_string(),
        // Not the raw HEAD: it moves on every ingest, and a row whose
        // content did not change may carry nothing per-run.
        upstream_cursor: None,
        md_path,
        render_version: RENDER_VERSION,
        rows,
        edges: Vec::new(),
        problems,
        bucket_key: Some(m_uuid.clone()),
    })
    .with_context(|| format!("on_doc_complete {m_uuid}"))?;
    progress.inc(1);
    if parsed.head.is_none() {
        tracing::warn!(
            event = "airvisual_render_no_head",
            source = source_id,
            "dolt_log() returned no HEAD; the render cursor stays put and the next run re-renders"
        );
    }

    Ok(summary)
}

/// What the markdown needs to know about a plot that got written.
struct PlotFacts {
    file: String,
    series: usize,
    points: usize,
    span: Option<(i64, i64)>,
}

fn render_plot(
    parsed: &ParsedAirvisual,
    quantity: &Quantity,
    plots_dir: &Path,
) -> Result<Option<PlotFacts>> {
    let mut traces: Vec<Trace> = Vec::new();
    let mut points = 0usize;
    let mut span: Option<(i64, i64)> = None;

    for s in &parsed.series {
        let spec = metric_spec(&s.metric)?;
        if spec.quantity.key != quantity.key || s.is_empty() {
            continue;
        }
        points += s.len();
        let (lo, hi) = (s.ts_ms[0], s.ts_ms[s.len() - 1]);
        span = Some(match span {
            Some((a, b)) => (a.min(lo), b.max(hi)),
            None => (lo, hi),
        });
        traces.push(Trace {
            name: series_label(parsed.device_label(&s.device), spec),
            axis: spec.axis,
            x_ms: s.ts_ms.clone(),
            y: s.values.iter().map(|v| (spec.to_si)(*v)).collect(),
            unit: spec.si_unit.to_string(),
        });
    }

    if traces.is_empty() {
        return Ok(None);
    }
    traces.sort_by(|a, b| a.name.cmp(&b.name));

    let subtitle = format!(
        "{} series · {} points · {}",
        traces.len(),
        thousands(points as i64),
        span.map(|(a, b)| format!("{} — {}", short_ts(a), short_ts(b)))
            .unwrap_or_else(|| "no samples".into()),
    );
    let html = standalone_html(quantity, &subtitle, &traces)?;
    let file = format!("{}.html", quantity.key);
    let path = plots_dir.join(&file);
    fs::write(&path, html).with_context(|| format!("write {}", path.display()))?;

    Ok(Some(PlotFacts {
        file,
        series: traces.len(),
        points,
        span,
    }))
}

/// [`spec_for`] with the failure spelled out. A column with no entry in
/// [`units::METRICS`] is a hard error, not a dropped series.
fn metric_spec(metric: &str) -> Result<&'static units::MetricSpec> {
    spec_for(metric).with_context(|| {
        format!(
            "airvisual column {metric:?} has no plot mapping — add it to \
             `render/units.rs::METRICS` (which quantity it plots on, its \
             axis, and its unit)"
        )
    })
}

// ---------------------------------------------------------------- markdown

fn render_markdown(
    parsed: &ParsedAirvisual,
    source_id: &str,
    m_uuid: &str,
    plots: &[(&Quantity, PlotFacts)],
) -> String {
    let mut out = String::with_capacity(8 * 1024);
    let created_at = parsed.earliest_ts_ms().and_then(iso);
    let modified_at = parsed.latest_ts_ms().and_then(iso);

    out.push_str("---\n");
    let _ = writeln!(out, "markdown_uuid: {m_uuid}");
    let _ = writeln!(out, "source_id: {source_id}");
    out.push_str("provider: airvisual\n");
    let _ = writeln!(out, "title: {}", yaml_safe(&page_title(source_id)));
    if let Some(ts) = &created_at {
        let _ = writeln!(out, "created_at: {}", yaml_safe(ts));
    }
    if let Some(ts) = &modified_at {
        let _ = writeln!(out, "modified_at: {}", yaml_safe(ts));
    }
    out.push_str("---\n\n");

    out.push_str(
        &Title {
            suffix: None,
            text: &page_title(source_id),
            markdown_uuid: Some(m_uuid),
            source_url: None,
        }
        .render(),
    );

    let _ = writeln!(
        out,
        "{} device{} · {} samples across {} series{}.\n",
        parsed.devices.len(),
        if parsed.devices.len() == 1 { "" } else { "s" },
        thousands(parsed.sample_count),
        parsed.series.len(),
        match (parsed.earliest_ts_ms(), parsed.latest_ts_ms()) {
            (Some(a), Some(b)) => format!(", {} — {}", short_ts(a), short_ts(b)),
            _ => String::new(),
        }
    );
    out.push_str(
        "Every value is as the device logged it; the sampling interval follows \
         the device's own sensor-mode schedule, so the markers are unevenly spaced \
         by design.\n\n",
    );

    render_plot_sections(&mut out, plots);
    render_device_sections(&mut out, parsed);
    render_store_section(&mut out, parsed);
    out
}

fn render_plot_sections(out: &mut String, plots: &[(&Quantity, PlotFacts)]) {
    if plots.is_empty() {
        out.push_str("## Plots\n\n*(no samples yet — nothing to plot)*\n\n");
        return;
    }
    out.push_str("## Plots\n\n");
    for (quantity, facts) in plots {
        let _ = writeln!(out, "### {}\n", quantity.title);
        let _ = writeln!(out, "{}\n", quantity.blurb);
        let _ = writeln!(
            out,
            "{} series · {} points{}\n",
            facts.series,
            thousands(facts.points as i64),
            facts
                .span
                .map(|(a, b)| format!(" · {} — {}", short_ts(a), short_ts(b)))
                .unwrap_or_default()
        );
        // Relative `src`, so the page works opened straight off disk and
        // under a static file server; the UI rewrites it to
        // `/api/asset/<markdown_uuid>/plots/<file>` when it renders the
        // body (see ChatBody.ce.vue).
        let _ = writeln!(
            out,
            "<iframe src=\"plots/{}\" title=\"{}\" width=\"100%\" height=\"520\" \
             loading=\"lazy\" sandbox=\"allow-scripts allow-downloads\" \
             style=\"border:1px solid rgba(128,128,128,.35);border-radius:6px\">\
             </iframe>\n",
            facts.file, quantity.title,
        );
        let _ = writeln!(
            out,
            "[Open the {} plot on its own]({})\n",
            quantity.title.to_lowercase(),
            format_args!("plots/{}", facts.file)
        );
    }
}

fn render_device_sections(out: &mut String, parsed: &ParsedAirvisual) {
    out.push_str("## Devices\n\n");
    if parsed.devices.is_empty() {
        out.push_str("*(no devices)*\n\n");
        return;
    }

    let by_device = parsed.series_by_device();
    for dev in &parsed.devices {
        let uuid = device_uuid(&dev.id);
        let _ = writeln!(
            out,
            "<div id=\"m-{uuid}\" data-section-uuid=\"{uuid}\" class=\"msg msg--airvisual\">\n"
        );
        let _ = writeln!(out, "### {}\n", dev.name);
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
        let _ = writeln!(out, "*{}*\n", facts.join(" · "));
        match by_device.get(dev.id.as_str()) {
            Some(list) if !list.is_empty() => render_metric_table(out, list),
            _ => out.push_str("*(no samples)*\n\n"),
        }
        out.push_str("</div>\n\n");
    }

    // Samples can exist for a device the config no longer names. Say so
    // rather than dropping them — those series still plot.
    let orphans: Vec<&str> = by_device
        .keys()
        .copied()
        .filter(|d| !parsed.devices.iter().any(|dev| dev.id == *d))
        .collect();
    if !orphans.is_empty() {
        let _ = writeln!(
            out,
            "> **{} serial{} with samples but no `airvisual_devices` row:** {}. \
             Their series still plot; the device was most likely given a different \
             `serial` in the config.\n",
            orphans.len(),
            if orphans.len() == 1 { "" } else { "s" },
            orphans
                .iter()
                .map(|d| format!("`{d}`"))
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
}

fn render_metric_table(out: &mut String, series: &[&Series]) {
    out.push_str(
        "| Column | Unit | Samples | Min | Max | Mean | Latest | First | Last | Median gap |\n",
    );
    out.push_str("| --- | --- | ---: | ---: | ---: | ---: | ---: | --- | --- | ---: |\n");
    for s in series {
        let (unit, si): (&str, Box<dyn Fn(f64) -> f64>) = match spec_for(&s.metric) {
            Some(spec) => (spec.si_unit, Box::new(|v| (spec.to_si)(v))),
            None => ("?", Box::new(|v| v)),
        };
        let vals: Vec<f64> = s.values.iter().map(|v| si(*v)).collect();
        let min = vals.iter().copied().fold(f64::INFINITY, f64::min);
        let max = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {} | {} | {} |",
            s.metric,
            unit,
            thousands(s.len() as i64),
            min,
            max,
            mean,
            vals.last().copied().unwrap_or(f64::NAN),
            s.ts_ms.first().copied().and_then(short).unwrap_or_default(),
            s.ts_ms.last().copied().and_then(short).unwrap_or_default(),
            median_gap(&s.ts_ms)
                .map(human_gap)
                .unwrap_or_else(|| "—".into()),
        );
    }
    out.push('\n');
}

/// Counts of what was read — nothing about the store's own history,
/// which changes on every run and would make an unchanged page differ.
fn render_store_section(out: &mut String, parsed: &ParsedAirvisual) {
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
}

// ------------------------------------------------------------- grid rows

/// One row for the page plus one per device, so a device is findable in
/// the grid by name. A row that will not validate is dropped and
/// recorded on `problems` rather than failing the source's render.
fn build_grid_rows(
    parsed: &ParsedAirvisual,
    source_id: &str,
    m_uuid: &str,
    md_rel: &str,
    problems: &mut Vec<ProblemRow>,
) -> Vec<GridRow> {
    let title = page_title(source_id);
    let by_device = parsed.series_by_device();

    let mut doc_text = format!(
        "{title}\n{} devices, {} samples",
        parsed.devices.len(),
        parsed.sample_count
    );
    for q in QUANTITIES {
        doc_text.push('\n');
        doc_text.push_str(q.title);
    }

    let mut rows: Vec<GridRow> = GridRow::builder()
        .uuid(m_uuid.to_string())
        .provider(Provider::Airvisual)
        .kind("Sensor Timeseries")
        .source_label(SOURCE_LABEL)
        .is_document(true)
        .created_at(parsed.earliest_ts_ms().and_then(iso))
        .modified_at(parsed.latest_ts_ms().and_then(iso))
        .conversation_name(Some(title.clone()))
        .conversation_uuid(m_uuid.to_string())
        .entire_chat(format!("/chat/{m_uuid}"))
        .text(doc_text)
        .qmd_path(Some(md_rel.to_string()))
        .markdown_uuid(Some(m_uuid.to_string()))
        .upstream_id(Some(source_id.to_string()))
        .upstream_entity_kind(Some("timeseries".to_string()))
        .upstream_scope(Some(source_id.to_string()))
        .build_or_record(source_id, m_uuid, RENDER_VERSION, problems)
        .into_iter()
        .collect();

    for (idx, dev) in parsed.devices.iter().enumerate() {
        let uuid = device_uuid(&dev.id);
        let series = by_device.get(dev.id.as_str());
        let mut text = format!("{} (serial {})", dev.name, dev.id);
        if let Some(m) = &dev.model {
            let _ = write!(text, "\nAirVisual model {m}");
        }
        if let Some(list) = series {
            for s in list {
                let unit = spec_for(&s.metric).map(|x| x.si_unit).unwrap_or("?");
                let _ = write!(text, "\n{} — {} samples ({unit})", s.metric, s.len());
            }
        }
        let when = series
            .and_then(|l| l.iter().filter_map(|s| s.ts_ms.last()).max().copied())
            .or(dev.last_ts_ms)
            .and_then(iso);
        rows.extend(
            GridRow::builder()
                .uuid(uuid)
                .provider(Provider::Airvisual)
                .kind("Sensor Device")
                .source_label(SOURCE_LABEL)
                .created_at(when)
                .author(Some(dev.name.clone()))
                .channel(Some(dev.name.clone()))
                .conversation_name(Some(title.clone()))
                .conversation_uuid(m_uuid.to_string())
                .message_index(Some(idx as i64))
                .entire_chat(format!("/chat/{m_uuid}"))
                .text(text)
                .qmd_path(Some(md_rel.to_string()))
                .upstream_id(Some(dev.id.clone()))
                .upstream_entity_kind(Some("device".to_string()))
                .markdown_uuid(Some(m_uuid.to_string()))
                .build_or_record(source_id, m_uuid, RENDER_VERSION, problems),
        );
    }
    rows
}

fn page_title(source_id: &str) -> String {
    format!("AirVisual monitors — {source_id}")
}

pub fn output_paths(root: &Path, source_id: &str) -> (PathBuf, PathBuf) {
    let dir = datalib_etl::layout::render_markdown_root(root, source_id);
    (dir.join("index.md"), dir.join("plots"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuids_are_stable_and_source_scoped() {
        let a = document_uuid("air-cucina");
        assert_eq!(a, document_uuid("air-cucina"), "must be deterministic");
        assert_ne!(a, document_uuid("air-2"), "must be source-scoped");
        assert_ne!(device_uuid("4133WV2JB9Z"), device_uuid("QKAO9PC1XDJ"));
        assert_eq!(
            device_uuid("4133WV2JB9Z"),
            device_uuid("4133WV2JB9Z"),
            "a serial is the device wherever it is configured"
        );
        assert_ne!(document_uuid("air-cucina"), device_uuid("air-cucina"));
    }

    #[test]
    fn unknown_metric_is_an_error_naming_the_fix() {
        let err = metric_spec("pressure_pa").unwrap_err().to_string();
        assert!(err.contains("pressure_pa"), "{err}");
        assert!(err.contains("units.rs"), "{err}");
    }
}
