//! Turn a Garmin raw store into its documents: the overview with the
//! weight plot, a page per family of daily metrics, the daily log, one
//! page per activity, and the account's records and badges. Every one
//! is in the bucket keyed by the overview's uuid, since each run reads
//! the store whole and emits them all.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::progress::Progress;
use datalib_etl_render::front_matter::yaml_scalar;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::html::escape_md_inline;
use datalib_etl_render::message::stamp_text_html;
use datalib_etl_render::title::Title;
use datalib_etl_timeseries_render::text::{iso, short_ts, stamp};
use datalib_id::{entity_id_str, IdNamespace};
use datalib_schema::grid_rows::GridRow;
use datalib_schema::problems::ProblemRow;
use datalib_schema::providers::Provider;

use super::activities;
use super::daily::{self, Days, Family};
use super::parse::{Activity, ParsedGarmin};
use super::records;
use super::stamps::day_start_iso;
use super::time_series;
use super::RENDER_VERSION;
use datalib_etl_timeseries_render::time_series::time_series_chart;

const ID_NAMESPACE: IdNamespace = IdNamespace::Garmin;
const KIND_PAGE: &str = "weight";
const KIND_DEVICE: &str = "device";
const KIND_METRIC_PAGE: &str = "metric_page";
const KIND_DAILY_LOG: &str = "daily_log";
const KIND_DAY: &str = "day";
const KIND_ACTIVITY: &str = "activity";
const KIND_LISTINGS: &str = "listings";
const KIND_ITEM: &str = "item";

/// The page's `markdown_uuid`: one page per source and no Garmin-side
/// object behind it, so its key is the source id. No stamp: the page's
/// `created_at` is its first weigh-in, not the page's own.
pub fn document_uuid(source_id: &str) -> String {
    entity_id_str(ID_NAMESPACE, source_id, None, KIND_PAGE, source_id, None)
}

/// A device's row, keyed on the id Garmin issues it. No stamp: the
/// row's `created_at` is its last sync, which moves.
pub fn device_uuid(source_id: &str, device_id: &str) -> String {
    entity_id_str(ID_NAMESPACE, source_id, None, KIND_DEVICE, device_id, None)
}

pub fn metric_page_uuid(source_id: &str, family: Family) -> String {
    entity_id_str(
        ID_NAMESPACE,
        source_id,
        None,
        KIND_METRIC_PAGE,
        family.slug(),
        None,
    )
}

pub fn daily_log_uuid(source_id: &str) -> String {
    entity_id_str(
        ID_NAMESPACE,
        source_id,
        None,
        KIND_DAILY_LOG,
        source_id,
        None,
    )
}

/// A day's row, keyed on its calendar date. No stamp: the offset its
/// `created_at` needs can arrive after the day does.
pub fn day_uuid(source_id: &str, date: &str) -> String {
    entity_id_str(ID_NAMESPACE, source_id, None, KIND_DAY, date, None)
}

/// An activity's page, keyed on Garmin's `activityId` and stamped with
/// its start.
pub fn activity_uuid(source_id: &str, activity_id: &str, start_ms: Option<i64>) -> String {
    entity_id_str(
        ID_NAMESPACE,
        source_id,
        None,
        KIND_ACTIVITY,
        activity_id,
        start_ms,
    )
}

pub fn listings_uuid(source_id: &str) -> String {
    entity_id_str(
        ID_NAMESPACE,
        source_id,
        None,
        KIND_LISTINGS,
        source_id,
        None,
    )
}

/// A listing row, keyed on its `garmin_items` id (`<kind>#<id>`).
pub fn item_uuid(source_id: &str, item_id: &str) -> String {
    entity_id_str(ID_NAMESPACE, source_id, None, KIND_ITEM, item_id, None)
}

/// How many of the newest weigh-ins the table shows. The plot has all
/// of them; the table is for reading the recent ones off.
const TABLE_ROWS: usize = 30;

#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub weigh_ins: usize,
    pub devices: usize,
    pub plots: usize,
    pub metric_pages: usize,
    pub days: usize,
    pub activities: usize,
    pub items: usize,
}

/// Where documents are written, and the one bucket they all go in.
struct Out<'a> {
    root: &'a Path,
    source_id: &'a str,
    dir: PathBuf,
    bucket: String,
    on_doc: &'a mut dyn FnMut(RenderedMarkdown) -> Result<()>,
}

impl Out<'_> {
    fn emit(
        &mut self,
        rel: &str,
        m_uuid: &str,
        body: String,
        build_rows: impl FnOnce(&str, &mut Vec<ProblemRow>) -> Vec<GridRow>,
    ) -> Result<()> {
        let md_path = self.dir.join(rel);
        if let Some(parent) = md_path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("mkdir -p {}", parent.display()))?;
        }
        fs::write(&md_path, body).with_context(|| format!("write {}", md_path.display()))?;
        let md_rel = md_path
            .strip_prefix(self.root)
            .unwrap_or(&md_path)
            .to_string_lossy()
            .into_owned();
        let mut problems: Vec<ProblemRow> = Vec::new();
        let rows = build_rows(&md_rel, &mut problems);
        (self.on_doc)(RenderedMarkdown {
            markdown_uuid: m_uuid.to_string(),
            source_id: self.source_id.to_string(),
            // Not the raw HEAD: it moves on every ingest, and a row whose
            // content did not change may carry nothing per-run.
            upstream_cursor: None,
            bucket_key: Some(self.bucket.clone()),
            md_path,
            render_version: RENDER_VERSION,
            rows,
            sections: Vec::new(),
            edges: Vec::new(),
            contacts: Vec::new(),
            problems,
        })
        .with_context(|| format!("on_doc_complete {m_uuid}"))
    }
}

/// Front matter and the title: what every document opens with.
fn head(m_uuid: &str, source_id: &str, title: &str, created_at: Option<&str>) -> String {
    let mut out = String::with_capacity(8 * 1024);
    out.push_str("---\n");
    let _ = writeln!(out, "markdown_uuid: {m_uuid}");
    let _ = writeln!(out, "source_id: {source_id}");
    out.push_str("provider: garmin\n");
    let _ = writeln!(out, "title: {}", yaml_scalar(title));
    if let Some(ts) = created_at {
        let _ = writeln!(out, "created_at: {}", yaml_scalar(ts));
    }
    out.push_str("---\n\n");
    out.push_str(
        &Title {
            suffix: None,
            text: title,
            markdown_uuid: Some(m_uuid),
            source_url: None,
        }
        .render(),
    );
    out
}

fn section_open(out: &mut String, uuid: &str) {
    let _ = writeln!(
        out,
        "<div id=\"m-{uuid}\" data-section-uuid=\"{uuid}\" class=\"msg msg--garmin\">\n"
    );
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
    fs::create_dir_all(&page_dir).with_context(|| format!("mkdir -p {}", page_dir.display()))?;
    // Where an earlier build wrote its Plotly pages; a chart is now drawn
    // from the time-series store.
    match fs::remove_dir_all(page_dir.join("plots")) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).context("remove the retired plots/");
        }
        _ => {}
    }
    let families: Vec<Family> = Family::ALL
        .into_iter()
        .filter(|f| daily::has_any(&parsed.days, *f))
        .collect();
    let docs = 2 + families.len() + 1 + parsed.activities.len() + 1;
    progress.set_length(Some(docs as u64));

    let mut summary = RenderSummary {
        weigh_ins: parsed.weigh_ins.len(),
        devices: parsed.devices.len(),
        ..Default::default()
    };
    if !parsed.weigh_ins.is_empty() {
        summary.plots = 1;
    }
    progress.inc(1);

    let m_uuid = document_uuid(source_id);
    let mut out = Out {
        root,
        source_id,
        dir: page_dir,
        bucket: m_uuid.clone(),
        on_doc: on_doc_complete,
    };
    let body = render_markdown(parsed, source_id, &m_uuid);
    out.emit("index.md", &m_uuid, body, |md_rel, problems| {
        build_grid_rows(parsed, source_id, &m_uuid, md_rel, problems)
    })?;
    progress.inc(1);

    let who = who(parsed, source_id);
    for family in families {
        if render_family(&mut out, parsed, &who, family)? {
            summary.plots += 1;
        }
        summary.metric_pages += 1;
        progress.inc(1);
    }
    summary.days = render_daily_log(&mut out, parsed, &who)?;
    progress.inc(1);
    let device_names: HashMap<&str, &str> = parsed
        .devices
        .iter()
        .map(|d| (d.id.as_str(), d.name.as_str()))
        .collect();
    for a in &parsed.activities {
        render_activity(&mut out, parsed, a, &device_names)?;
        summary.activities += 1;
        progress.inc(1);
    }
    summary.items = render_listings(&mut out, parsed, &who)?;
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

/// The account holder's name, else the source's id: what every title
/// after a dash says.
fn who(parsed: &ParsedGarmin, source_id: &str) -> String {
    parsed
        .full_name
        .as_deref()
        .or(parsed.display_name.as_deref())
        .unwrap_or(source_id)
        .to_string()
}

fn day_iso(days: &Days, date: &str) -> Option<String> {
    day_start_iso(date, days.get(date)?.offset_secs?)
}

/// One family's page and its charts; `true` when it has one.
fn render_family(
    out: &mut Out<'_>,
    parsed: &ParsedGarmin,
    who: &str,
    family: Family,
) -> Result<bool> {
    let source_id = out.source_id;
    let days = &parsed.days;
    let with_data: Vec<&String> = days
        .iter()
        .filter(|(_, d)| daily::family_line(family, d).is_some())
        .map(|(date, _)| date)
        .collect();
    let (Some(first), Some(last)) = (with_data.first(), with_data.last()) else {
        return Ok(false);
    };
    let latest = daily::family_line(family, &days[*last]).unwrap_or_default();
    let title = format!("{} — {who}", family.title());
    let m_uuid = metric_page_uuid(source_id, family);
    let created_at = day_iso(days, first);
    let modified_at = day_iso(days, last);

    let charts = time_series::family_charts(source_id, family, days);
    let mut body = head(&m_uuid, source_id, &title, created_at.as_deref());
    let _ = writeln!(body, "**{}:** {latest}\n", stamp_text_html(last));
    for ids in &charts {
        let _ = writeln!(body, "{}\n", time_series_chart(ids));
    }
    let _ = writeln!(
        body,
        "### Latest {} days\n",
        daily::TABLE_DAYS.min(with_data.len())
    );
    daily::family_table(&mut body, family, days);

    let text = format!(
        "{title}\n{last}: {latest}\n{} days since {first}",
        with_data.len()
    );
    out.emit(
        &format!("{}.md", family.slug()),
        &m_uuid,
        body,
        |md_rel, problems| {
            GridRow::builder()
                .uuid(m_uuid.clone())
                .provider(Provider::Garmin)
                .kind("Garmin Metrics")
                .source_label("Garmin")
                .is_document(true)
                .item_count(Some(with_data.len() as i64))
                .created_at(created_at.clone())
                .modified_at(modified_at.clone())
                .author(parsed.full_name.clone())
                .conversation_name(Some(title.clone()))
                .conversation_uuid(m_uuid.clone())
                .entire_chat(format!("/chat/{m_uuid}"))
                .body(text)
                .qmd_path(Some(md_rel.to_string()))
                .markdown_uuid(Some(m_uuid.clone()))
                .upstream_id(Some(family.slug().to_string()))
                .upstream_entity_kind(Some(KIND_METRIC_PAGE.to_string()))
                .build_or_record(source_id, &m_uuid, RENDER_VERSION, problems)
                .into_iter()
                .collect()
        },
    )?;
    Ok(!charts.is_empty())
}

/// One section and one grid row per day with any value, newest first.
/// Returns how many days.
fn render_daily_log(out: &mut Out<'_>, parsed: &ParsedGarmin, who: &str) -> Result<usize> {
    let source_id = out.source_id;
    let days = &parsed.days;
    let title = format!("Daily log — {who}");
    let m_uuid = daily_log_uuid(source_id);
    let first = days.keys().next().cloned();
    let last = days.keys().next_back().cloned();
    let mut body = head(
        &m_uuid,
        source_id,
        &title,
        first.as_deref().and_then(|d| day_iso(days, d)).as_deref(),
    );
    if days.is_empty() {
        body.push_str("*(no per-day metrics mirrored yet)*\n\n");
    }
    let mut lines: Vec<(&String, Vec<(Family, String)>)> = Vec::new();
    for (date, day) in days.iter().rev() {
        let by_family: Vec<(Family, String)> = Family::ALL
            .into_iter()
            .filter_map(|f| daily::family_line(f, day).map(|l| (f, l)))
            .collect();
        section_open(&mut body, &day_uuid(source_id, date));
        let _ = writeln!(body, "### {}\n", stamp_text_html(date));
        for (family, line) in &by_family {
            let _ = writeln!(body, "- **{}:** {line}", family.title());
        }
        body.push_str("\n</div>\n\n");
        lines.push((date, by_family));
    }
    let count = lines.len();
    out.emit("days.md", &m_uuid, body, |md_rel, problems| {
        let mut rows: Vec<GridRow> = GridRow::builder()
            .uuid(m_uuid.clone())
            .provider(Provider::Garmin)
            .kind("Garmin Daily Log")
            .source_label("Garmin")
            .is_document(true)
            .item_count(Some(count as i64))
            .created_at(first.as_deref().and_then(|d| day_iso(days, d)))
            .modified_at(last.as_deref().and_then(|d| day_iso(days, d)))
            .author(parsed.full_name.clone())
            .conversation_name(Some(title.clone()))
            .conversation_uuid(m_uuid.clone())
            .entire_chat(format!("/chat/{m_uuid}"))
            .body(format!("{title}\n{count} days"))
            .qmd_path(Some(md_rel.to_string()))
            .markdown_uuid(Some(m_uuid.clone()))
            .upstream_id(Some(source_id.to_string()))
            .upstream_entity_kind(Some(KIND_DAILY_LOG.to_string()))
            .build_or_record(source_id, &m_uuid, RENDER_VERSION, problems)
            .into_iter()
            .collect();
        for (idx, (date, by_family)) in lines.iter().enumerate() {
            let mut text: Vec<&str> = Vec::new();
            let lead = daily::headline(&days[*date]);
            text.extend(lead.as_deref());
            text.extend(by_family.iter().map(|(_, l)| l.as_str()));
            rows.extend(
                GridRow::builder()
                    .uuid(day_uuid(source_id, date))
                    .provider(Provider::Garmin)
                    .kind("Garmin Day")
                    .source_label("Garmin")
                    .created_at(day_iso(days, date))
                    .author(parsed.full_name.clone())
                    .conversation_name(Some(title.clone()))
                    .conversation_uuid(m_uuid.clone())
                    .message_index(Some(idx as i64))
                    .entire_chat(format!("/chat/{m_uuid}"))
                    .body(format!("{date}\n{}", text.join("\n")))
                    .qmd_path(Some(md_rel.to_string()))
                    .upstream_id(Some((*date).clone()))
                    .upstream_entity_kind(Some(KIND_DAY.to_string()))
                    .markdown_uuid(Some(m_uuid.clone()))
                    .build_or_record(source_id, &m_uuid, RENDER_VERSION, problems),
            );
        }
        rows
    })?;
    Ok(count)
}

fn render_activity(
    out: &mut Out<'_>,
    parsed: &ParsedGarmin,
    a: &Activity,
    device_names: &HashMap<&str, &str>,
) -> Result<()> {
    let source_id = out.source_id;
    let m_uuid = activity_uuid(source_id, &a.id, a.start_gmt_ms());
    let name = a.name();
    let created_at = a.start_iso();
    let device = a
        .device_id()
        .and_then(|id| device_names.get(id.as_str()).map(|n| n.to_string()));
    let mut body = head(&m_uuid, source_id, &name, created_at.as_deref());
    if a.fit_blake3.is_some() {
        let ids = time_series::activity_chart(source_id, &a.id);
        let _ = writeln!(body, "{}\n", time_series_chart(&ids));
    }
    activities::body(&mut body, a, device.as_deref());
    out.emit(
        &format!("activities/{m_uuid}.md"),
        &m_uuid,
        body,
        |md_rel, problems| {
            GridRow::builder()
                .uuid(m_uuid.clone())
                .provider(Provider::Garmin)
                .kind("Garmin Activity")
                .source_label("Garmin")
                .is_document(true)
                .item_count(Some(1))
                .created_at(created_at.clone())
                .author(parsed.full_name.clone())
                .channel(Some(a.type_label()))
                .conversation_name(Some(name.clone()))
                .conversation_uuid(m_uuid.clone())
                .entire_chat(format!("/chat/{m_uuid}"))
                .body(activities::row_text(a))
                .qmd_path(Some(md_rel.to_string()))
                .markdown_uuid(Some(m_uuid.clone()))
                .upstream_id(Some(a.id.clone()))
                .upstream_entity_kind(Some(KIND_ACTIVITY.to_string()))
                .build_or_record(source_id, &m_uuid, RENDER_VERSION, problems)
                .into_iter()
                .collect()
        },
    )
}

/// Personal records, badges, gear, workouts and goals on one page, a
/// section and a row each. Returns how many.
fn render_listings(out: &mut Out<'_>, parsed: &ParsedGarmin, who: &str) -> Result<usize> {
    let source_id = out.source_id;
    let title = format!("Records and badges — {who}");
    let m_uuid = listings_uuid(source_id);
    let names: HashMap<&str, String> = parsed
        .activities
        .iter()
        .map(|a| (a.id.as_str(), a.name()))
        .collect();
    let activity_name = |id: &str| names.get(id).cloned();
    let mut body = head(&m_uuid, source_id, &title, None);
    let mut lines = Vec::new();
    for (kind, heading, row_kind) in records::KINDS {
        let items: Vec<_> = parsed.items.iter().filter(|i| i.kind == *kind).collect();
        if items.is_empty() {
            continue;
        }
        let _ = writeln!(body, "## {heading}\n");
        for item in items {
            let (name, said) = item.line(activity_name);
            section_open(&mut body, &item_uuid(source_id, &item.id));
            let _ = write!(body, "**{}**", escape_md_inline(&name));
            if !said.is_empty() {
                let _ = write!(body, " · {}", escape_md_inline(&said));
            }
            body.push('\n');
            body.push_str("\n</div>\n\n");
            lines.push((item, *row_kind, name, said));
        }
    }
    if lines.is_empty() {
        body.push_str("*(no records, badges, gear, workouts or goals mirrored yet)*\n\n");
    }
    let count = lines.len();
    out.emit("records.md", &m_uuid, body, |md_rel, problems| {
        let mut rows: Vec<GridRow> = GridRow::builder()
            .uuid(m_uuid.clone())
            .provider(Provider::Garmin)
            .kind("Garmin Records")
            .source_label("Garmin")
            .is_document(true)
            .item_count(Some(count as i64))
            .author(parsed.full_name.clone())
            .conversation_name(Some(title.clone()))
            .conversation_uuid(m_uuid.clone())
            .entire_chat(format!("/chat/{m_uuid}"))
            .body(format!("{title}\n{count} entries"))
            .qmd_path(Some(md_rel.to_string()))
            .markdown_uuid(Some(m_uuid.clone()))
            .upstream_id(Some(source_id.to_string()))
            .upstream_entity_kind(Some(KIND_LISTINGS.to_string()))
            .build_or_record(source_id, &m_uuid, RENDER_VERSION, problems)
            .into_iter()
            .collect();
        for (idx, (item, row_kind, name, said)) in lines.iter().enumerate() {
            rows.extend(
                GridRow::builder()
                    .uuid(item_uuid(source_id, &item.id))
                    .provider(Provider::Garmin)
                    .kind(*row_kind)
                    .source_label("Garmin")
                    .created_at(item.created_at())
                    .author(parsed.full_name.clone())
                    .conversation_name(Some(title.clone()))
                    .conversation_uuid(m_uuid.clone())
                    .message_index(Some(idx as i64))
                    .entire_chat(format!("/chat/{m_uuid}"))
                    .body(format!("{name}\n{said}"))
                    .qmd_path(Some(md_rel.to_string()))
                    .upstream_id(Some(item.id.clone()))
                    .upstream_entity_kind(Some(KIND_ITEM.to_string()))
                    .markdown_uuid(Some(m_uuid.clone()))
                    .build_or_record(source_id, &m_uuid, RENDER_VERSION, problems),
            );
        }
        rows
    })?;
    Ok(count)
}

fn render_markdown(parsed: &ParsedGarmin, source_id: &str, m_uuid: &str) -> String {
    let title = page_title(parsed, source_id);
    let created_at = parsed
        .latest_weigh_in()
        .and_then(|w| iso(w.timestamp_gmt_ms));
    let mut out = head(m_uuid, source_id, &title, created_at.as_deref());
    render_weight_section(&mut out, parsed, source_id);
    render_device_section(&mut out, parsed, source_id);
    render_store_section(&mut out, parsed);
    out
}

fn render_weight_section(out: &mut String, parsed: &ParsedGarmin, source_id: &str) {
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
        stamp(latest.timestamp_gmt_ms),
        parsed.weigh_ins.len(),
        stamp(first.timestamp_gmt_ms),
        match latest.body_fat_pct {
            Some(f) => format!(" · {f:.1} % body fat"),
            None => String::new(),
        }
    );
    let _ = writeln!(
        out,
        "{}\n",
        time_series_chart(&time_series::weight_chart(source_id))
    );
    let _ = writeln!(
        out,
        "### Latest {} weigh-ins\n",
        TABLE_ROWS.min(parsed.weigh_ins.len())
    );
    out.push_str("| When | kg | BMI | Body fat | Source |\n");
    out.push_str("| --- | ---: | ---: | ---: | --- |\n");
    for w in parsed.weigh_ins.iter().rev().take(TABLE_ROWS) {
        let _ = writeln!(
            out,
            "| {} | {:.1} | {} | {} | {} |",
            stamp(w.timestamp_gmt_ms),
            w.weight_kg,
            w.bmi.map(|b| format!("{b:.1}")).unwrap_or_default(),
            w.body_fat_pct
                .map(|f| format!("{f:.1} %"))
                .unwrap_or_default(),
            escape_md_inline(w.source_type.as_deref().unwrap_or("")),
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
        section_open(out, &uuid);
        let _ = writeln!(out, "### {}\n", escape_md_inline(&d.name));
        let _ = writeln!(
            out,
            "*device {}{}*\n",
            escape_md_inline(&d.id),
            d.last_sync
                .as_deref()
                .map(|s| format!(" · last synced {}", escape_md_inline(s)))
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
    let _ = writeln!(out, "| Activities | {} |", parsed.activities.len());
    let _ = writeln!(out, "| Activity FIT files | {} |", parsed.activity_files);
    let _ = writeln!(
        out,
        "| Account listings (records, gear, badges, workouts, goals) | {} |",
        parsed.items.len()
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
    problems: &mut Vec<ProblemRow>,
) -> Vec<GridRow> {
    let title = page_title(parsed, source_id);
    let mut text = format!("{title}\n{} weigh-ins", parsed.weigh_ins.len());
    if let Some(w) = parsed.latest_weigh_in() {
        let _ = write!(
            text,
            "\nlatest {:.1} kg on {}",
            w.weight_kg,
            short_ts(w.timestamp_gmt_ms)
        );
    }
    let mut rows: Vec<GridRow> = GridRow::builder()
        .uuid(m_uuid.to_string())
        .provider(Provider::Garmin)
        .kind("Garmin Weight")
        .source_label("Garmin")
        .is_document(true)
        .item_count(Some(parsed.measurements()))
        .created_at(
            parsed
                .weigh_ins
                .first()
                .and_then(|w| iso(w.timestamp_gmt_ms)),
        )
        .modified_at(
            parsed
                .latest_weigh_in()
                .and_then(|w| iso(w.timestamp_gmt_ms)),
        )
        .author(parsed.full_name.clone())
        .conversation_name(Some(title.clone()))
        .conversation_uuid(m_uuid.to_string())
        .entire_chat(format!("/chat/{m_uuid}"))
        .body(text)
        .qmd_path(Some(md_rel.to_string()))
        .markdown_uuid(Some(m_uuid.to_string()))
        .upstream_id(Some(source_id.to_string()))
        .upstream_entity_kind(Some(KIND_PAGE.to_string()))
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
                .body(format!("{} (device {})", d.name, d.id))
                .qmd_path(Some(md_rel.to_string()))
                .upstream_id(Some(d.id.clone()))
                .upstream_entity_kind(Some(KIND_DEVICE.to_string()))
                .markdown_uuid(Some(m_uuid.to_string()))
                .build_or_record(source_id, m_uuid, RENDER_VERSION, problems),
        );
    }
    rows
}

fn page_title(parsed: &ParsedGarmin, source_id: &str) -> String {
    format!("Garmin — {}", who(parsed, source_id))
}

/// Garmin writes device stamps as `2026-09-14T05:12:44.0` with no
/// offset, and the field name says GMT — the one place assuming UTC is
/// backed by the source itself.
fn garmin_stamp_to_iso(s: &str) -> Option<String> {
    datalib_time::parse_with_assumed_utc(s)
        .ok()
        .map(|t| t.to_rfc3339())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A device's name and a weigh-in's source come from Garmin's API;
    /// on the page they are text.
    #[test]
    fn a_device_and_a_weigh_in_in_markup_render_escaped() {
        let parsed = ParsedGarmin {
            head: None,
            display_name: None,
            full_name: None,
            weigh_ins: vec![super::super::parse::WeighIn {
                id: "w1".into(),
                calendar_date: "2364-04-11".into(),
                timestamp_gmt_ms: 12442118400000,
                weight_kg: 80.0,
                bmi: None,
                body_fat_pct: None,
                source_type: Some("<b>scale</b> | x".into()),
            }],
            devices: vec![super::super::parse::Device {
                id: "<i>1</i>".into(),
                name: "<script>x</script> & co".into(),
                last_sync: None,
            }],
            metrics: Vec::new(),
            activities: Vec::new(),
            activity_files: 0,
            items: Vec::new(),
            days: Default::default(),
        };
        let mut out = String::new();
        render_device_section(&mut out, &parsed, "garmin");
        render_weight_section(&mut out, &parsed, "garmin");
        assert!(!out.contains("<script>") && !out.contains("<b>"), "{out}");
        assert!(
            out.contains("### &lt;script&gt;x&lt;/script&gt; &amp; co\n"),
            "{out}"
        );
        assert!(out.contains("*device &lt;i&gt;1&lt;/i&gt;*"), "{out}");
        assert!(out.contains("| &lt;b&gt;scale&lt;/b&gt; \\| x |"), "{out}");
    }

    #[test]
    fn page_ids_are_source_scoped_and_device_ids_are_garmins() {
        assert_eq!(document_uuid("garmin"), document_uuid("garmin"));
        assert_ne!(document_uuid("garmin"), document_uuid("garmin-2"));
        assert_ne!(device_uuid("g", "1"), device_uuid("g", "2"));
        assert_ne!(document_uuid("1"), device_uuid("1", "1"));
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
