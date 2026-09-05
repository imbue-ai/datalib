//! Turn a whole YoLink raw store into one markdown page plus its plots.
//!
//! The document has three parts, in order:
//!
//! 1. **Plots** — one `<iframe>` per physical quantity, each frame a
//!    standalone Plotly page under `plots/`. Every device is a series.
//! 2. **Devices** — the non-timeseries half: what each device is, what
//!    it has reported, and the per-metric extent/statistics. Each device
//!    gets an `id="m-<uuid>" data-section-uuid="<uuid>"` wrapper, which
//!    is what the UI's per-section feedback and copy-id affordances hang
//!    off, and what the device's grid row addresses.
//! 3. **Store** — provenance: the configured fetch scope, plus counts of
//!    commits, readings and recorded fetch errors. Deliberately no
//!    doltlite HEAD and no commit log; see `render_store_section` for
//!    why those two stay out of the rendered page.
//!
//! Secrets never reach the page: `family_device_id` and the device UDID
//! are a per-device read credential for the device's entire history (see
//! `download/schema_raw.rs`), so the device table names the device and
//! its kind and stops there.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::grid_index::RenderedMarkdown;
use datalib_etl::progress::Progress;
use datalib_etl::render_cursor;
use datalib_etl::title::Title;
use datalib_schema::grid_rows::GridRow;
use datalib_schema::render_problems::RenderProblemRow;
use once_cell::sync::Lazy;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::parse::{ParsedYolink, Series};
use super::plot::{standalone_html, Trace};
use super::units::{self, series_label, spec_for, Quantity, QUANTITIES};
use super::RENDER_VERSION;

/// Namespace for every UUIDv5 this renderer mints. A fixed, arbitrary
/// UUID — the same role `GITHUB_UUID_NS` plays for that provider.
pub static YOLINK_UUID_NS: Lazy<Uuid> = Lazy::new(|| {
    Uuid::parse_str("6b1d6f2c-9c1a-5f7e-b0d4-2f9a7c4e0001").expect("valid yolink ns uuid")
});

/// The page's `markdown_uuid`. Derived from the stanza name, not from
/// anything upstream: there is exactly one page per stanza, and it must
/// keep its identity across every re-render.
pub fn document_uuid(source_name: &str) -> String {
    Uuid::new_v5(
        &YOLINK_UUID_NS,
        format!("yolink:{source_name}:timeseries").as_bytes(),
    )
    .to_string()
}

/// A device's section/grid-row uuid within the page.
pub fn device_uuid(source_name: &str, device: &str) -> String {
    Uuid::new_v5(
        &YOLINK_UUID_NS,
        format!("yolink:{source_name}:device:{device}").as_bytes(),
    )
    .to_string()
}

/// What goes into the render cursor's `params` slot.
///
/// YoLink has no render knobs, so the natural value is
/// `render_cursor::no_params()`. We record `RENDER_VERSION` instead,
/// because the cursor here is the *entire* skip decision: with a bare
/// `{}`, bumping `RENDER_VERSION` would change nothing for any mirror
/// whose store hadn't moved, and the new layout would reach it only
/// whenever the next reading happened to land. `read_for_params` treats
/// any difference as "re-render everything", which for a one-page
/// provider is precisely right.
pub fn cursor_params() -> serde_json::Value {
    serde_json::json!({ "render_version": RENDER_VERSION })
}

/// Counts for the step's one-line run summary.
#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub devices: usize,
    pub series: usize,
    pub points: usize,
    pub plots: usize,
}

/// Render the page, its plots, and its sidecar; advance the cursor.
pub fn render_all(
    parsed: &ParsedYolink,
    root: &Path,
    source_name: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let page_dir = datalib_etl::layout::rendered_md_root(root, source_name);
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

    let m_uuid = document_uuid(source_name);
    let fingerprint = compute_fingerprint(parsed);
    let body = render_markdown(parsed, source_name, &m_uuid, &fingerprint, &rendered_plots);

    let md_path = page_dir.join("index.md");
    fs::write(&md_path, body).with_context(|| format!("write {}", md_path.display()))?;

    let md_rel = md_path
        .strip_prefix(root)
        .unwrap_or(&md_path)
        .to_string_lossy()
        .into_owned();
    let mut problems: Vec<RenderProblemRow> = Vec::new();
    let rows = build_grid_rows(parsed, source_name, &m_uuid, &md_rel, &mut problems);

    on_doc_complete(RenderedMarkdown {
        markdown_uuid: m_uuid.clone(),
        source_name: source_name.to_string(),
        source_fingerprint: fingerprint,
        upstream_cursor: parsed.head.clone(),
        md_path,
        render_version: RENDER_VERSION,
        rows,
        edges: Vec::new(),
        problems,
    })
    .with_context(|| format!("on_doc_complete {m_uuid}"))?;
    progress.inc(1);

    // Cursor last, and only with a real HEAD: an unwritten cursor makes
    // the next run re-render, which is the harmless direction. Writing a
    // placeholder would make it skip forever.
    if let Some(head) = parsed.head.as_deref() {
        let cursor_path = render_cursor::cursor_path(root, source_name);
        render_cursor::write(&cursor_path, head, parsed.scan_elapsed, &cursor_params())
            .with_context(|| format!("write yolink render cursor {}", cursor_path.display()))?;
    } else {
        tracing::warn!(
            event = "yolink_render_no_head",
            source = source_name,
            "dolt_log() returned no HEAD; leaving the render cursor unwritten \
             (next run will re-render)"
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

/// Write one quantity's plot. `Ok(None)` when no series in the store
/// belongs to this quantity — a store with only THSensors has no volume
/// plot, and an empty frame is worse than no frame.
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

/// The sidecar's `source_fingerprint` — a hash of the readings this
/// document was built from, plus the render version.
///
/// Deliberately **not** the store's HEAD, though HEAD is right there and
/// we only get here because it moved. Two reasons:
///
/// 1. The cross-provider contract (the `markdowns` row the store keeps)
///    is that this hashes *the upstream payload that produced the
///    document*. A commit hash is a property of the store, not of the
///    content: two stores holding identical readings would disagree, and
///    a commit that changed nothing this page renders would look like a
///    change.
/// 2. It is the difference between a reproducible `markdowns` row and
///    one that moves every time the store is rebuilt from scratch. The
///    doltlite *file* can't be byte-stable — doltlite's own bootstrap
///    commit and `doltlite_raw::open`'s "schema: apply DDL" both take
///    the wall clock, and hashes chain — but the table contents can, and
///    this was the only field standing in the way.
///
/// Hashing every sample rather than just the per-series shape is
/// deliberate: yolink re-fetches overlapping windows, and a corrected
/// historical value changes no count and no timestamp. A shape-only
/// hash would let the Load step skip a document that genuinely changed.
fn compute_fingerprint(parsed: &ParsedYolink) -> String {
    let mut h = Sha256::new();
    h.update(RENDER_VERSION.to_be_bytes());
    h.update(b"|readings:");
    h.update(parsed.reading_count.to_be_bytes());
    for s in &parsed.series {
        h.update(b"\n");
        h.update(s.device.as_bytes());
        h.update(b"/");
        h.update(s.metric.as_bytes());
        h.update(b"=");
        h.update((s.len() as u64).to_be_bytes());
        for (ts, v) in s.ts_ms.iter().zip(&s.values) {
            h.update(ts.to_be_bytes());
            h.update(v.to_be_bytes());
        }
    }
    h.finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

// ---------------------------------------------------------------- markdown

fn render_markdown(
    parsed: &ParsedYolink,
    source_name: &str,
    m_uuid: &str,
    fingerprint: &str,
    plots: &[(&Quantity, PlotFacts)],
) -> String {
    let mut out = String::with_capacity(8 * 1024);
    let when_ts = parsed.latest_ts_ms().and_then(iso);

    out.push_str("---\n");
    let _ = writeln!(out, "markdown_uuid: {m_uuid}");
    let _ = writeln!(out, "source_fingerprint: {fingerprint}");
    let _ = writeln!(out, "source_name: {source_name}");
    out.push_str("provider: yolink\n");
    let _ = writeln!(out, "title: {}", yaml_safe(&page_title(source_name)));
    if let Some(ts) = &when_ts {
        let _ = writeln!(out, "when_ts: {}", yaml_safe(ts));
    }
    out.push_str("---\n\n");

    out.push_str(
        &Title {
            text: &page_title(source_name),
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
    render_device_sections(&mut out, parsed, source_name);
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
        //
        // `sandbox` without `allow-same-origin` gives the frame an opaque
        // origin: in the app the plot is served from the same origin as
        // the UI, and there is no reason a chart should be able to reach
        // `parent.document`. `allow-scripts` is what Plotly needs;
        // `allow-downloads` keeps its "save as PNG" toolbar button
        // working.
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

fn render_device_sections(out: &mut String, parsed: &ParsedYolink, source_name: &str) {
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
        let uuid = device_uuid(source_name, &dev.name);
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

/// The store's own provenance — the doltlite HEAD hash and the per-commit
/// hashes and wall-clock dates — is deliberately NOT rendered here, only
/// the counts.
///
/// Two reasons, and the second is the one that bites. It is storage-layer
/// bookkeeping rather than anything the user's sensors recorded, so
/// putting it in a vector index buys noise; and it changes on every
/// single run, because doltlite stamps its bootstrap commits with the
/// wall clock and the hashes follow from those timestamps. That made
/// this one file the reason a whole rendered markdown tree was never
/// byte-identical to its previous self, which in turn re-ran the ~90s
/// CPU-only embed on CI for changes that altered nothing it reads (see
/// `tests/fixtures/tar_qmd.py`). `dolt_log` still has all of it — read
/// it with the doltlite CLI, per docs/dev/doltlite.md.
fn render_store_section(out: &mut String, parsed: &ParsedYolink) {
    out.push_str("## Store\n\n");
    out.push_str("| | |\n| --- | --- |\n");
    let _ = writeln!(out, "| Commits | {} |", parsed.commits.len());
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
    source_name: &str,
    m_uuid: &str,
    md_rel: &str,
    problems: &mut Vec<RenderProblemRow>,
) -> Vec<GridRow> {
    let title = page_title(source_name);
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
        .provider("yolink")
        .kind("Sensor Timeseries")
        .source_label("YoLink")
        .when_ts(parsed.latest_ts_ms().and_then(iso))
        .account(Some(source_name.to_string()))
        .conversation_name(Some(title.clone()))
        .conversation_uuid(m_uuid.to_string())
        .entire_chat(format!("/chat/{m_uuid}"))
        .text(doc_text)
        .qmd_path(Some(md_rel.to_string()))
        .markdown_uuid(Some(m_uuid.to_string()))
        .build_or_record(source_name, m_uuid, RENDER_VERSION, problems)
        .into_iter()
        .collect();

    for (idx, dev) in parsed.devices.iter().enumerate() {
        let uuid = device_uuid(source_name, &dev.name);
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
                .provider("yolink")
                .kind("Sensor Device")
                .source_label("YoLink")
                .when_ts(when)
                .author(Some(dev.name.clone()))
                .account(Some(source_name.to_string()))
                .channel(Some(dev.name.clone()))
                .conversation_name(Some(title.clone()))
                .conversation_uuid(m_uuid.to_string())
                .message_index(Some(idx as i64))
                .entire_chat(format!("/chat/{m_uuid}"))
                .text(text)
                .qmd_path(Some(md_rel.to_string()))
                .upstream_id(Some(dev.kind.clone()))
                .upstream_entity_kind(Some("device".to_string()))
                .markdown_uuid(Some(m_uuid.to_string()))
                .build_or_record(source_name, m_uuid, RENDER_VERSION, problems),
        );
    }
    rows
}

// ---------------------------------------------------------------- helpers

fn page_title(source_name: &str) -> String {
    format!("YoLink sensors — {source_name}")
}

/// Unix ms → the repo's ISO-8601-with-offset convention. The stored
/// value came from a unix-epoch number, so per the convention in
/// AGENTS.md it renders as UTC with an explicit `+00:00`.
fn iso(ms: i64) -> Option<String> {
    datalib_time::IsoOffsetTimestamp::from_unix_millis(ms).map(|t| t.to_rfc3339())
}

/// `YYYY-MM-DD HH:MM` — the table/subtitle form. Same instant as
/// [`iso`], just short enough to read in a cell.
fn short(ms: i64) -> Option<String> {
    datalib_time::IsoOffsetTimestamp::from_unix_millis(ms)
        .map(|t| t.inner().format("%Y-%m-%d %H:%M").to_string())
}

fn short_ts(ms: i64) -> String {
    short(ms).unwrap_or_else(|| ms.to_string())
}

/// Median inter-sample gap, in ms. Median rather than mean because a
/// single multi-day outage would otherwise swamp a sensor that reports
/// every few minutes.
fn median_gap(ts: &[i64]) -> Option<i64> {
    if ts.len() < 2 {
        return None;
    }
    let mut gaps: Vec<i64> = ts.windows(2).map(|w| w[1] - w[0]).collect();
    gaps.sort_unstable();
    Some(gaps[gaps.len() / 2])
}

fn human_gap(ms: i64) -> String {
    let s = ms as f64 / 1000.0;
    if s < 90.0 {
        format!("{s:.0}s")
    } else if s < 5400.0 {
        format!("{:.1}m", s / 60.0)
    } else if s < 129_600.0 {
        format!("{:.1}h", s / 3600.0)
    } else {
        format!("{:.1}d", s / 86_400.0)
    }
}

fn thousands(n: i64) -> String {
    let neg = n < 0;
    let digits = n.unsigned_abs().to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if neg {
        format!("-{out}")
    } else {
        out
    }
}

/// Re-indent a stored JSON blob for display; pass it through unchanged
/// if it isn't JSON after all (the column is TEXT, and a display helper
/// is no place to start failing renders).
fn pretty_json(raw: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_else(|_| raw.to_string()),
        Err(_) => raw.to_string(),
    }
}

fn yaml_safe(s: &str) -> String {
    if s.chars().any(|c| ":#[]{}&*?,|>'\"%@`\n".contains(c)) {
        format!("\"{}\"", s.replace('"', "\\\""))
    } else {
        s.to_string()
    }
}

/// Where the page and its plots land, relative to a data root. Exposed
/// for tests and for anything that needs to find the page without
/// re-deriving the layout.
pub fn output_paths(root: &Path, source_name: &str) -> (PathBuf, PathBuf) {
    let dir = datalib_etl::layout::rendered_md_root(root, source_name);
    (dir.join("index.md"), dir.join("plots"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thousands_groups_digits() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_000), "1,000");
        assert_eq!(thousands(149_564), "149,564");
        assert_eq!(thousands(-1_234_567), "-1,234,567");
    }

    #[test]
    fn median_gap_ignores_a_single_long_outage() {
        // Five 60s gaps and one 10-day gap: the median must stay 60s.
        let mut ts = vec![0i64];
        for _ in 0..5 {
            ts.push(ts.last().unwrap() + 60_000);
        }
        ts.push(ts.last().unwrap() + 864_000_000);
        assert_eq!(median_gap(&ts), Some(60_000));
        assert_eq!(median_gap(&[1]), None);
    }

    #[test]
    fn human_gap_picks_a_readable_unit() {
        assert_eq!(human_gap(30_000), "30s");
        assert_eq!(human_gap(300_000), "5.0m");
        assert_eq!(human_gap(7_200_000), "2.0h");
        assert_eq!(human_gap(432_000_000), "5.0d");
    }

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
    fn cursor_params_carry_the_render_version() {
        // The point of not using `no_params()`: bumping RENDER_VERSION
        // must invalidate a cursor written by the previous version.
        let stored = serde_json::json!({"render_version": RENDER_VERSION - 1});
        assert_ne!(stored, cursor_params());
    }

    #[test]
    fn unknown_metric_is_an_error_naming_the_fix() {
        let err = metric_spec("pressure_psi").unwrap_err().to_string();
        assert!(err.contains("pressure_psi"), "{err}");
        assert!(err.contains("units.rs"), "{err}");
    }

    #[test]
    fn iso_stamps_carry_an_explicit_offset() {
        // AGENTS.md: an epoch-derived timestamp renders as UTC with an
        // explicit `+00:00`, never a bare `Z`-less or offset-less form.
        let s = iso(1_781_481_609_000).unwrap();
        assert!(s.starts_with("2026-"), "{s}");
        assert!(s.ends_with("+00:00"), "{s}");
    }
}
