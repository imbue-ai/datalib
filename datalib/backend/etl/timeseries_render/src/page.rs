//! The one page a sensor source renders to: a summary, one Plotly plot
//! per physical quantity, a section per device with its metric table,
//! and the store's counts. A provider's parse fills in a [`Page`]; what
//! differs between providers — names, wording, the unit table — is a
//! [`PageProfile`].

use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::progress::Progress;
use datalib_etl::title::Title;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::processor::RenderCtx;
use datalib_id::{entity_id_str, IdNamespace};
use datalib_schema::grid_rows::GridRow;
use datalib_schema::problems::ProblemRow;
use datalib_schema::providers::Provider;

use crate::plot::{standalone_html, Trace};
use crate::series::{by_device, earliest_ts_ms, latest_ts_ms, Series};
use crate::text::{human_gap, iso, median_gap, short, short_ts, thousands, yaml_safe};
use crate::units::{series_label, spec_in, MetricSpec, Quantity};

const KIND_PAGE: &str = "timeseries";
const KIND_DEVICE: &str = "device";

/// How one sensor provider names and words its page.
pub struct PageProfile {
    pub provider: Provider,
    /// The front matter's `provider:`, and the device sections' CSS class.
    pub tag: &'static str,
    pub source_label: &'static str,
    pub id_namespace: IdNamespace,
    /// The page is titled `<title_prefix> — <source id>`.
    pub title_prefix: &'static str,
    /// What one stored value is called, plural: `samples`, `readings`.
    pub noun: &'static str,
    /// The paragraph under the summary line.
    pub intro: &'static str,
    /// What the raw store calls a metric — `column`, `metric` — in the
    /// metric table's header and the unmapped-metric error.
    pub metric_word: &'static str,
    /// Decimal places in the metric table.
    pub decimals: usize,
    pub metrics: &'static [MetricSpec],
    pub quantities: &'static [Quantity],
    /// Shown when the store names no device.
    pub no_devices: &'static str,
    /// A paragraph above the devices, when the provider has one to say.
    pub devices_preamble: Option<&'static str>,
    /// For series whose device has no row: what a device key is called
    /// (`serial`), the device table, and the likely reason.
    pub orphan_key_word: &'static str,
    pub devices_table: &'static str,
    pub orphan_hint: &'static str,
    pub render_version: u32,
}

/// One device, as its section and grid row show it.
pub struct Device {
    /// What its series are keyed by. Its id and `upstream_id` are made
    /// from this.
    pub key: String,
    pub name: String,
    /// The line under the device's heading.
    pub facts: String,
    /// The first line(s) of its grid row's text; one line per series
    /// follows.
    pub grid_text: String,
    pub last_ts_ms: Option<i64>,
}

/// Everything the page is built from.
pub struct Page<'a> {
    /// The commit everything was read at; `None` when nothing is
    /// committed yet.
    pub head: Option<&'a str>,
    pub devices: Vec<Device>,
    /// Sorted by (device, metric), so the document and the plot legends
    /// are stable run to run.
    pub series: &'a [Series],
    pub sample_count: i64,
    /// The `## Store` section: counts of what was read — nothing about
    /// the store's own history, which changes on every run and would
    /// make an unchanged page differ.
    pub store_section: String,
}

/// Counts for the step's one-line run summary.
#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub devices: usize,
    pub series: usize,
    pub points: usize,
    pub plots: usize,
}

impl PageProfile {
    /// The page's `markdown_uuid`. There is exactly one page per source
    /// and nothing upstream behind it, so its key is the source id. No
    /// stamp: the page's `created_at` is its earliest value, not the
    /// page's own.
    pub fn document_uuid(&self, source_id: &str) -> String {
        entity_id_str(
            self.id_namespace,
            source_id,
            None,
            KIND_PAGE,
            source_id,
            None,
        )
    }

    /// A device's row. No stamp: the row's `created_at` is its latest
    /// value, which moves every sync.
    pub fn device_uuid(&self, source_id: &str, device_key: &str) -> String {
        entity_id_str(
            self.id_namespace,
            source_id,
            None,
            KIND_DEVICE,
            device_key,
            None,
        )
    }

    fn page_title(&self, source_id: &str) -> String {
        format!("{} — {source_id}", self.title_prefix)
    }

    /// The metric's row in the unit table. One with none is a hard
    /// error, not a dropped series: silently omitting it would mean a
    /// new sensor kind renders a page that looks complete and isn't.
    pub fn metric_spec(&self, metric: &str) -> Result<&'static MetricSpec> {
        spec_in(self.metrics, metric).with_context(|| {
            format!(
                "{} {} {metric:?} has no plot mapping — add it to \
                 `render/units.rs::METRICS` (which quantity it plots on, its \
                 axis, and its unit)",
                self.tag, self.metric_word,
            )
        })
    }
}

impl Page<'_> {
    fn device_name<'s>(&'s self, key: &'s str) -> &'s str {
        self.devices
            .iter()
            .find(|d| d.key == key)
            .map(|d| d.name.as_str())
            .unwrap_or(key)
    }
}

/// Whether a one-page source can skip this run: the driver pinned a
/// commit and found nothing the page reads changed since the last
/// render, which costs one `dolt_log()` query. When it can, the pin is
/// recorded as read and the run's summary line comes back.
pub fn skip_if_current(ctx: &RenderCtx<'_>, provider: &str, page_uuid: &str) -> Option<String> {
    let range = ctx.raw_range();
    let (Some(pin), false) = (range.pin, range.is_stale(page_uuid)) else {
        return None;
    };
    tracing::info!(
        event = "timeseries_render_skipped",
        provider,
        source = %ctx.name,
        head = %pin,
        "nothing the page reads changed since the last render",
    );
    ctx.consumed(pin);
    Some(format!("up to date at {pin}"))
}

pub fn render_all(
    profile: &PageProfile,
    page: &Page<'_>,
    root: &Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let page_dir = datalib_etl::layout::render_markdown_root(root, source_id);
    let plots_dir = page_dir.join("plots");
    fs::create_dir_all(&plots_dir).with_context(|| format!("mkdir -p {}", plots_dir.display()))?;

    let mut summary = RenderSummary {
        devices: page.devices.len(),
        series: page.series.len(),
        points: page.series.iter().map(Series::len).sum(),
        ..Default::default()
    };
    progress.set_length(Some((profile.quantities.len() + 1) as u64));

    // Plots first: the markdown links to whatever actually got written,
    // so a quantity with no data yields no iframe rather than a broken
    // one.
    let mut rendered_plots: Vec<(&Quantity, PlotFacts)> = Vec::new();
    for quantity in profile.quantities {
        progress.set_message(&format!("plot {}", quantity.key));
        if let Some(facts) = render_plot(profile, page, quantity, &plots_dir)? {
            summary.plots += 1;
            rendered_plots.push((quantity, facts));
        }
        progress.inc(1);
    }

    let m_uuid = profile.document_uuid(source_id);
    let body = render_markdown(profile, page, source_id, &m_uuid, &rendered_plots);

    let md_path = page_dir.join("index.md");
    fs::write(&md_path, body).with_context(|| format!("write {}", md_path.display()))?;

    let md_rel = md_path
        .strip_prefix(root)
        .unwrap_or(&md_path)
        .to_string_lossy()
        .into_owned();
    let mut problems: Vec<ProblemRow> = Vec::new();
    let rows = build_grid_rows(profile, page, source_id, &m_uuid, &md_rel, &mut problems);

    on_doc_complete(RenderedMarkdown {
        markdown_uuid: m_uuid.clone(),
        source_id: source_id.to_string(),
        // Not the raw HEAD: it moves on every ingest, and a row whose
        // content did not change may carry nothing per-run.
        upstream_cursor: None,
        bucket_key: Some(m_uuid.clone()),
        md_path,
        render_version: profile.render_version,
        rows,
        sections: Vec::new(),
        edges: Vec::new(),
        problems,
    })
    .with_context(|| format!("on_doc_complete {m_uuid}"))?;
    progress.inc(1);
    if page.head.is_none() {
        tracing::warn!(
            event = "timeseries_render_no_head",
            provider = profile.tag,
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
    profile: &PageProfile,
    page: &Page<'_>,
    quantity: &Quantity,
    plots_dir: &Path,
) -> Result<Option<PlotFacts>> {
    let mut traces: Vec<Trace> = Vec::new();
    let mut points = 0usize;
    let mut span: Option<(i64, i64)> = None;

    for s in page.series {
        let spec = profile.metric_spec(&s.metric)?;
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
            name: series_label(page.device_name(&s.device), spec),
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
            .unwrap_or_else(|| format!("no {}", profile.noun)),
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

// ---------------------------------------------------------------- markdown

fn render_markdown(
    profile: &PageProfile,
    page: &Page<'_>,
    source_id: &str,
    m_uuid: &str,
    plots: &[(&Quantity, PlotFacts)],
) -> String {
    let mut out = String::with_capacity(8 * 1024);
    let earliest = earliest_ts_ms(page.series);
    let latest = latest_ts_ms(page.series);
    let title = profile.page_title(source_id);

    out.push_str("---\n");
    let _ = writeln!(out, "markdown_uuid: {m_uuid}");
    let _ = writeln!(out, "source_id: {source_id}");
    let _ = writeln!(out, "provider: {}", profile.tag);
    let _ = writeln!(out, "title: {}", yaml_safe(&title));
    if let Some(ts) = earliest.and_then(iso) {
        let _ = writeln!(out, "created_at: {}", yaml_safe(&ts));
    }
    if let Some(ts) = latest.and_then(iso) {
        let _ = writeln!(out, "modified_at: {}", yaml_safe(&ts));
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

    let _ = writeln!(
        out,
        "{} device{} · {} {} across {} series{}.\n",
        page.devices.len(),
        if page.devices.len() == 1 { "" } else { "s" },
        thousands(page.sample_count),
        profile.noun,
        page.series.len(),
        match (earliest, latest) {
            (Some(a), Some(b)) => format!(", {} — {}", short_ts(a), short_ts(b)),
            _ => String::new(),
        }
    );
    let _ = write!(out, "{}\n\n", profile.intro);

    render_plot_sections(&mut out, profile, plots);
    render_device_sections(&mut out, profile, page, source_id);
    out.push_str(&page.store_section);
    out
}

fn render_plot_sections(out: &mut String, profile: &PageProfile, plots: &[(&Quantity, PlotFacts)]) {
    if plots.is_empty() {
        let _ = write!(
            out,
            "## Plots\n\n*(no {} yet — nothing to plot)*\n\n",
            profile.noun
        );
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

fn render_device_sections(
    out: &mut String,
    profile: &PageProfile,
    page: &Page<'_>,
    source_id: &str,
) {
    out.push_str("## Devices\n\n");
    if page.devices.is_empty() {
        let _ = write!(out, "{}\n\n", profile.no_devices);
        return;
    }
    if let Some(preamble) = profile.devices_preamble {
        let _ = write!(out, "{preamble}\n\n");
    }

    let by_device = by_device(page.series);
    for dev in &page.devices {
        let uuid = profile.device_uuid(source_id, &dev.key);
        let _ = writeln!(
            out,
            "<div id=\"m-{uuid}\" data-section-uuid=\"{uuid}\" class=\"msg msg--{}\">\n",
            profile.tag
        );
        let _ = writeln!(out, "### {}\n", dev.name);
        let _ = writeln!(out, "*{}*\n", dev.facts);
        match by_device.get(dev.key.as_str()) {
            Some(list) if !list.is_empty() => render_metric_table(out, profile, list),
            _ => {
                let _ = write!(out, "*(no {})*\n\n", profile.noun);
            }
        }
        out.push_str("</div>\n\n");
    }

    // Values can exist for a device the store no longer names. Say so
    // rather than dropping them — those series still plot.
    let orphans: Vec<&str> = by_device
        .keys()
        .copied()
        .filter(|d| !page.devices.iter().any(|dev| dev.key == *d))
        .collect();
    if !orphans.is_empty() {
        let _ = writeln!(
            out,
            "> **{} {}{} with {} but no `{}` row:** {}. Their series still plot; {}\n",
            orphans.len(),
            profile.orphan_key_word,
            if orphans.len() == 1 { "" } else { "s" },
            profile.noun,
            profile.devices_table,
            orphans
                .iter()
                .map(|d| format!("`{d}`"))
                .collect::<Vec<_>>()
                .join(", "),
            profile.orphan_hint,
        );
    }
}

fn render_metric_table(out: &mut String, profile: &PageProfile, series: &[&Series]) {
    let _ = writeln!(
        out,
        "| {} | Unit | Samples | Min | Max | Mean | Latest | First | Last | Median gap |",
        capitalize(profile.metric_word),
    );
    out.push_str("| --- | --- | ---: | ---: | ---: | ---: | ---: | --- | --- | ---: |\n");
    let p = profile.decimals;
    for s in series {
        // A metric with no unit mapping already failed the render in
        // `render_plot`; if that ever changes, show the raw name rather
        // than panicking here.
        let (unit, si): (&str, Box<dyn Fn(f64) -> f64>) = match spec_in(profile.metrics, &s.metric)
        {
            Some(spec) => (spec.si_unit, Box::new(|v| (spec.to_si)(v))),
            None => ("?", Box::new(|v| v)),
        };
        let vals: Vec<f64> = s.values.iter().map(|v| si(*v)).collect();
        let min = vals.iter().copied().fold(f64::INFINITY, f64::min);
        let max = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        let _ = writeln!(
            out,
            "| `{}` | {} | {} | {:.p$} | {:.p$} | {:.p$} | {:.p$} | {} | {} | {} |",
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

fn capitalize(word: &str) -> String {
    let mut chars = word.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

// ------------------------------------------------------------- grid rows

/// One row for the page plus one per device. The device rows are what
/// make a sensor findable in the grid at all — searching a device's
/// name should land on something, and the page row's text is a
/// summary, not an index of every device. A row that will not validate
/// is dropped and recorded on `problems` rather than failing the
/// source's render.
fn build_grid_rows(
    profile: &PageProfile,
    page: &Page<'_>,
    source_id: &str,
    m_uuid: &str,
    md_rel: &str,
    problems: &mut Vec<ProblemRow>,
) -> Vec<GridRow> {
    let title = profile.page_title(source_id);
    let by_device = by_device(page.series);
    let version = profile.render_version;

    let mut doc_text = format!(
        "{title}\n{} devices, {} {}",
        page.devices.len(),
        page.sample_count,
        profile.noun,
    );
    for q in profile.quantities {
        doc_text.push('\n');
        doc_text.push_str(q.title);
    }

    let mut rows: Vec<GridRow> = GridRow::builder()
        .uuid(m_uuid.to_string())
        .provider(profile.provider)
        .kind("Sensor Timeseries")
        .source_label(profile.source_label)
        .is_document(true)
        .created_at(earliest_ts_ms(page.series).and_then(iso))
        .modified_at(latest_ts_ms(page.series).and_then(iso))
        .conversation_name(Some(title.clone()))
        .conversation_uuid(m_uuid.to_string())
        .entire_chat(format!("/chat/{m_uuid}"))
        .body(doc_text)
        .qmd_path(Some(md_rel.to_string()))
        .markdown_uuid(Some(m_uuid.to_string()))
        .upstream_id(Some(source_id.to_string()))
        .upstream_entity_kind(Some(KIND_PAGE.to_string()))
        .build_or_record(source_id, m_uuid, version, problems)
        .into_iter()
        .collect();

    for (idx, dev) in page.devices.iter().enumerate() {
        let series = by_device.get(dev.key.as_str());
        let mut text = dev.grid_text.clone();
        if let Some(list) = series {
            for s in list {
                let unit = spec_in(profile.metrics, &s.metric)
                    .map(|x| x.si_unit)
                    .unwrap_or("?");
                let _ = write!(text, "\n{} — {} samples ({unit})", s.metric, s.len());
            }
        }
        let when = series
            .and_then(|l| l.iter().filter_map(|s| s.ts_ms.last()).max().copied())
            .or(dev.last_ts_ms)
            .and_then(iso);
        rows.extend(
            GridRow::builder()
                .uuid(profile.device_uuid(source_id, &dev.key))
                .provider(profile.provider)
                .kind("Sensor Device")
                .source_label(profile.source_label)
                .created_at(when)
                .author(Some(dev.name.clone()))
                .channel(Some(dev.name.clone()))
                .conversation_name(Some(title.clone()))
                .conversation_uuid(m_uuid.to_string())
                .message_index(Some(idx as i64))
                .entire_chat(format!("/chat/{m_uuid}"))
                .body(text)
                .qmd_path(Some(md_rel.to_string()))
                .upstream_id(Some(dev.key.clone()))
                .upstream_entity_kind(Some(KIND_DEVICE.to_string()))
                .markdown_uuid(Some(m_uuid.to_string()))
                .build_or_record(source_id, m_uuid, version, problems),
        );
    }
    rows
}
