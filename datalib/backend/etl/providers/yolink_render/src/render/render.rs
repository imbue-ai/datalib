//! Turn a whole YoLink raw store into one markdown page plus its plots.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::progress::Progress;
use datalib_etl::title::Title;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_id::{entity_id_str, IdNamespace, Scope};
use datalib_schema::grid_rows::GridRow;
use datalib_schema::problems::ProblemRow;
use datalib_schema::providers::Provider;

use super::parse::{ParsedYolink, Series};
use datalib_etl_timeseries_render::plot::{standalone_html, Trace};
use datalib_etl_timeseries_render::text::{
    human_gap, iso, median_gap, pretty_json, short, short_ts, thousands, yaml_safe,
};

use super::units::{self, series_label, spec_for, Quantity, QUANTITIES};
use super::RENDER_VERSION;

const ID_NAMESPACE: IdNamespace = IdNamespace::Yolink;
const KIND_PAGE: &str = "timeseries";
const KIND_DEVICE: &str = "device";

/// The page's `markdown_uuid`. Scoped to the source id, not to anything
/// upstream: there is exactly one page per source, and it must keep its
/// identity across every re-render. No stamp: the page's `created_at`
/// is its earliest reading, not the page's own.
pub fn document_uuid(source_id: &str) -> String {
    entity_id_str(
        ID_NAMESPACE,
        Scope::SourceInstance(source_id),
        KIND_PAGE,
        source_id,
        None,
    )
}

/// A device's row, keyed on its config name under the source. The ids
/// YoLink issues a device (`device_udid`, `family_device_id`) are read
/// secrets, and a natural key is stored in `upstream_id` in the clear,
/// so neither can be the key. No stamp: the row's `created_at` is its
/// latest reading, which moves every sync.
pub fn device_uuid(source_id: &str, device: &str) -> String {
    entity_id_str(
        ID_NAMESPACE,
        Scope::SourceInstance(source_id),
        KIND_DEVICE,
        device,
        None,
    )
}

/// Counts for the step's one-line run summary.
#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub devices: usize,
    pub series: usize,
    pub points: usize,
    pub plots: usize,
}

pub fn render_all(
    parsed: &ParsedYolink,
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
        bucket_key: Some(m_uuid.clone()),
        md_path,
        render_version: RENDER_VERSION,
        rows,
        sections: Vec::new(),
        edges: Vec::new(),
        problems,
    })
    .with_context(|| format!("on_doc_complete {m_uuid}"))?;
    progress.inc(1);
    if parsed.head.is_none() {
        tracing::warn!(
            event = "yolink_render_no_head",
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
    parsed: &ParsedYolink,
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
            name: series_label(&s.device, spec),
            axis: spec.axis,
            x_ms: s.ts_ms.clone(),
            y: s.values.iter().map(|v| (spec.to_si)(*v)).collect(),
            unit: spec.si_unit.to_string(),
        });
    }

    if traces.is_empty() {
        return Ok(None);
    }
    // Stable legend order regardless of how the rows came back.
    traces.sort_by(|a, b| a.name.cmp(&b.name));

    let subtitle = format!(
        "{} series · {} points · {}",
        traces.len(),
        thousands(points as i64),
        span.map(|(a, b)| format!("{} — {}", short_ts(a), short_ts(b)))
            .unwrap_or_else(|| "no readings".into()),
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

/// [`spec_for`] with the failure spelled out. A metric with no entry in
/// [`units::METRICS`] is a hard error, not a dropped series: silently
/// omitting it would mean a new sensor kind renders a page that looks
/// complete and isn't.
fn metric_spec(metric: &str) -> Result<&'static units::MetricSpec> {
    spec_for(metric).with_context(|| {
        format!(
            "yolink metric {metric:?} has no unit mapping — add it to \
             `render/units.rs::METRICS` (which quantity it plots on, its \
             axis, and its conversion to SI)"
        )
    })
}

// ---------------------------------------------------------------- markdown

fn render_markdown(
    parsed: &ParsedYolink,
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
    out.push_str("provider: yolink\n");
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
        "{} device{} · {} readings across {} series{}.\n",
        parsed.devices.len(),
        if parsed.devices.len() == 1 { "" } else { "s" },
        thousands(parsed.reading_count),
        parsed.series.len(),
        match (parsed.earliest_ts_ms(), parsed.latest_ts_ms()) {
            (Some(a), Some(b)) => format!(", {} — {}", short_ts(a), short_ts(b)),
            _ => String::new(),
        }
    );
    out.push_str(
        "Values are converted to SI on the way into each plot, so devices \
         reporting in different units share one axis.\n\n",
    );

    render_plot_sections(&mut out, plots);
    render_device_sections(&mut out, parsed, source_id);
    render_store_section(&mut out, parsed);
    out
}

fn render_plot_sections(out: &mut String, plots: &[(&Quantity, PlotFacts)]) {
    if plots.is_empty() {
        out.push_str("## Plots\n\n*(no readings yet — nothing to plot)*\n\n");
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

fn render_device_sections(out: &mut String, parsed: &ParsedYolink, source_id: &str) {
    out.push_str("## Devices\n\n");
    if parsed.devices.is_empty() {
        out.push_str("*(no devices configured)*\n\n");
        return;
    }
    // Device secrets stay out of the document on purpose — say so, so
    // nobody \"fixes\" the omission later.
    out.push_str(
        "Per-device read credentials (`family_device_id`, `device_udid`) are \
         deliberately omitted: the pair grants access to that device's entire \
         history.\n\n",
    );

    let by_device = parsed.series_by_device();
    for (idx, dev) in parsed.devices.iter().enumerate() {
        let uuid = device_uuid(source_id, &dev.name);
        let _ = writeln!(
            out,
            "<div id=\"m-{uuid}\" data-section-uuid=\"{uuid}\" class=\"msg msg--yolink\">\n"
        );
        let _ = writeln!(out, "### {}\n", dev.name);
        let _ = writeln!(
            out,
            "*{} · configured from {}{}*\n",
            dev.kind,
            iso(dev.start_ms).unwrap_or_else(|| dev.start_ms.to_string()),
            match dev.last_ts_ms.and_then(iso) {
                Some(t) => format!(" · cursor at {t}"),
                None => " · no readings fetched yet".to_string(),
            },
        );
        let series = by_device.get(dev.name.as_str());
        match series {
            Some(list) if !list.is_empty() => render_metric_table(out, list),
            _ => out.push_str("*(no readings)*\n\n"),
        }
        out.push_str("</div>\n\n");
        let _ = idx;
    }

    // A device row can exist with no readings; readings can also exist
    // for a device the config no longer lists. Surface the second case
    // rather than dropping it silently — those series still plot.
    let orphans: Vec<&str> = by_device
        .keys()
        .copied()
        .filter(|d| !parsed.devices.iter().any(|dev| dev.name == *d))
        .collect();
    if !orphans.is_empty() {
        let _ = writeln!(
            out,
            "> **{} device{} with readings but no `yolink_devices` row:** {}. \
             Their series still plot; they were most likely renamed or removed \
             from the download config.\n",
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
        "| Metric | Unit | Samples | Min | Max | Mean | Latest | First | Last | Median gap |\n",
    );
    out.push_str("| --- | --- | ---: | ---: | ---: | ---: | ---: | --- | --- | ---: |\n");
    for s in series {
        // A metric with no unit mapping already failed the render in
        // `render_plot`; if that ever changes, show the raw tag rather
        // than panicking here.
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
            "| `{}` | {} | {} | {:.3} | {:.3} | {:.3} | {:.3} | {} | {} | {} |",
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

/// The store's own provenance — the doltlite HEAD hash and the commit
/// log — is deliberately NOT rendered here, only the counts.
fn render_store_section(out: &mut String, parsed: &ParsedYolink) {
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
}

// ------------------------------------------------------------- grid rows

/// One row for the page plus one per device. The device rows are what
/// make a sensor findable in the grid at all — searching `main_fridge`
/// should land on something, and the page row's text is a summary, not
/// an index of every device.
/// A row that will not validate is dropped and recorded on `problems`
/// rather than failing the source's render — see
/// `GridRowBuilder::build_or_record`.
fn build_grid_rows(
    parsed: &ParsedYolink,
    source_id: &str,
    m_uuid: &str,
    md_rel: &str,
    problems: &mut Vec<ProblemRow>,
) -> Vec<GridRow> {
    let title = page_title(source_id);
    let by_device = parsed.series_by_device();

    let mut doc_text = format!(
        "{title}\n{} devices, {} readings",
        parsed.devices.len(),
        parsed.reading_count
    );
    for q in QUANTITIES {
        doc_text.push('\n');
        doc_text.push_str(q.title);
    }

    let mut rows: Vec<GridRow> = GridRow::builder()
        .uuid(m_uuid.to_string())
        .provider(Provider::Yolink)
        .kind("Sensor Timeseries")
        .source_label("YoLink")
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
        .upstream_entity_kind(Some(KIND_PAGE.to_string()))
        .upstream_scope(Some(source_id.to_string()))
        .build_or_record(source_id, m_uuid, RENDER_VERSION, problems)
        .into_iter()
        .collect();

    for (idx, dev) in parsed.devices.iter().enumerate() {
        let uuid = device_uuid(source_id, &dev.name);
        let series = by_device.get(dev.name.as_str());
        let mut text = format!("{} ({})", dev.name, dev.kind);
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
                .provider(Provider::Yolink)
                .kind("Sensor Device")
                .source_label("YoLink")
                .created_at(when)
                .author(Some(dev.name.clone()))
                .channel(Some(dev.name.clone()))
                .conversation_name(Some(title.clone()))
                .conversation_uuid(m_uuid.to_string())
                .message_index(Some(idx as i64))
                .entire_chat(format!("/chat/{m_uuid}"))
                .text(text)
                .qmd_path(Some(md_rel.to_string()))
                .upstream_id(Some(dev.name.clone()))
                .upstream_entity_kind(Some(KIND_DEVICE.to_string()))
                .upstream_scope(Some(source_id.to_string()))
                .markdown_uuid(Some(m_uuid.to_string()))
                .build_or_record(source_id, m_uuid, RENDER_VERSION, problems),
        );
    }
    rows
}

// ---------------------------------------------------------------- helpers

fn page_title(source_id: &str) -> String {
    format!("YoLink sensors — {source_id}")
}

pub fn output_paths(root: &Path, source_id: &str) -> (PathBuf, PathBuf) {
    let dir = datalib_etl::layout::render_markdown_root(root, source_id);
    (dir.join("index.md"), dir.join("plots"))
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
        let err = metric_spec("pressure_psi").unwrap_err().to_string();
        assert!(err.contains("pressure_psi"), "{err}");
        assert!(err.contains("units.rs"), "{err}");
    }
}
