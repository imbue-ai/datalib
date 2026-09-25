//! `render_all` — one `.md`, one grid row and its edges per event,
//! handed to the render driver's callback. Everything provider-specific
//! arrives in the [`CalendarRenderProfile`] and the events.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::progress::Progress;
use datalib_etl::title::Title;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::html::escape_text;
use datalib_etl_render::inputs::{Bucket, Buckets};
use datalib_etl_render::section::{join, Section};
use datalib_schema::edges::EdgeRow;
use datalib_schema::grid_rows::GridRow;
use datalib_schema::problems::{Outcome, Problem, ProblemRow, Reason, Scope, Stage};
use datalib_schema::providers::Provider;

use crate::rrule;
use crate::types::{EventShape, NormalizedEvent};
use crate::when::{display_range, EventTime, Unresolved};

/// What a provider parameterizes.
#[derive(Debug, Clone)]
pub struct CalendarRenderProfile {
    pub provider: Provider,
    /// The grid's Source column: `Google Calendar`, `Fastmail Calendar`.
    pub source_label: String,
    /// Whose calendars these are — the grid's `account` column.
    pub account: Option<String>,
    pub render_version: u32,
}

/// The grid's Kind column for each shape.
pub const KIND_EVENT: &str = "Event";
pub const KIND_SERIES: &str = "Recurring Event";
pub const KIND_OCCURRENCE: &str = "Changed Occurrence";

/// `edges.label` is `VARCHAR(64)`.
const EDGE_LABEL_CHARS: usize = 64;

#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub events_rendered: usize,
    /// Every event rendered, with what it read — for
    /// `RenderCtx::declare_bucket`.
    pub buckets: Buckets,
}

pub fn render_all(
    profile: &CalendarRenderProfile,
    events: &[NormalizedEvent],
    out_dir: &Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let mut summary = RenderSummary::default();
    progress.set_length(Some(events.len() as u64));
    for event in events {
        summary.buckets.push(Bucket {
            key: event.event_uuid.clone(),
            inputs: event.inputs.clone(),
        });
        let doc = render_one(profile, event, out_dir, source_id)
            .with_context(|| format!("render event {}", event.event_uuid))?;
        on_doc_complete(doc).with_context(|| format!("on_doc_complete {}", event.event_uuid))?;
        summary.events_rendered += 1;
        progress.inc(1);
    }
    Ok(summary)
}

fn render_one(
    profile: &CalendarRenderProfile,
    event: &NormalizedEvent,
    out_dir: &Path,
    source_id: &str,
) -> Result<RenderedMarkdown> {
    let uuid = &event.event_uuid;
    let md_path = md_path(out_dir, source_id, uuid);
    let page_dir = md_path.parent().expect("index.md has a parent");
    fs::create_dir_all(page_dir).with_context(|| format!("mkdir -p {}", page_dir.display()))?;

    let mut problems: Vec<ProblemRow> = event
        .problems
        .iter()
        .map(|p| problem_row(source_id, uuid, profile, Outcome::Nulled, p.clone()))
        .collect();
    let start = resolve_start(event, source_id, profile, &mut problems);

    let sections = render_markdown(profile, event, source_id, start.as_deref());
    fs::write(&md_path, join(&sections)).with_context(|| format!("write {}", md_path.display()))?;
    let md_rel = md_path
        .strip_prefix(out_dir)
        .unwrap_or(&md_path)
        .to_string_lossy()
        .into_owned();
    let row = build_grid_row(profile, event, source_id, &md_rel, start, &mut problems);
    Ok(RenderedMarkdown {
        markdown_uuid: uuid.clone(),
        source_id: source_id.to_string(),
        upstream_cursor: event.modified_at.clone(),
        bucket_key: Some(uuid.clone()),
        md_path,
        render_version: profile.render_version,
        rows: row.into_iter().collect(),
        sections,
        edges: edges(event),
        problems,
    })
}

/// One directory per event, named by its uuid, so a retitled or moved
/// event re-renders in place.
fn md_path(out_dir: &Path, source_id: &str, uuid: &str) -> PathBuf {
    datalib_etl::layout::render_markdown_root(out_dir, source_id)
        .join(uuid)
        .join("index.md")
}

fn problem_row(
    source_id: &str,
    uuid: &str,
    profile: &CalendarRenderProfile,
    outcome: Outcome,
    problem: Problem,
) -> ProblemRow {
    ProblemRow::new(
        source_id,
        Stage::Parse,
        Scope::Markdown(uuid),
        None,
        outcome,
        problem,
        Some(profile.render_version),
    )
}

/// The start as the grid's `created_at`: the instant the event happens,
/// so `before:`/`after:` mean "happening then". `None`, recorded, when
/// the start names a zone nobody knows; a floating time with no
/// calendar zone is read as UTC and that is recorded too.
fn resolve_start(
    event: &NormalizedEvent,
    source_id: &str,
    profile: &CalendarRenderProfile,
    problems: &mut Vec<ProblemRow>,
) -> Option<String> {
    let start = event.start.as_ref()?;
    match start.instant(event.calendar_time_zone.as_deref()) {
        Ok(i) => {
            if i.floating_as_utc {
                problems.push(problem_row(
                    source_id,
                    &event.event_uuid,
                    profile,
                    Outcome::Ok,
                    Problem::lossy(
                        "floating_time_read_as_utc",
                        Some("start".into()),
                        &start.display(),
                    ),
                ));
            }
            Some(i.at.to_rfc3339())
        }
        Err(why) => {
            let sample = match why {
                Unresolved::UnknownZone(z) => format!("unknown time zone {z:?}"),
                Unresolved::BadOffset(o) => format!("offset of {o}s is out of range"),
            };
            problems.push(problem_row(
                source_id,
                &event.event_uuid,
                profile,
                Outcome::Nulled,
                Problem::field("start", Reason::CoercionFailed, &sample),
            ));
            None
        }
    }
}

fn title_of(event: &NormalizedEvent) -> &str {
    event.title.as_deref().unwrap_or("(untitled event)")
}

fn kind_of(event: &NormalizedEvent) -> &'static str {
    match event.shape {
        EventShape::Single => KIND_EVENT,
        EventShape::Series { .. } => KIND_SERIES,
        EventShape::Occurrence { .. } => KIND_OCCURRENCE,
    }
}

fn when_line(event: &NormalizedEvent) -> Option<String> {
    event
        .start
        .as_ref()
        .map(|s| display_range(s, event.end.as_ref()))
}

fn repeats_lines(event: &NormalizedEvent) -> Vec<String> {
    let EventShape::Series { rules, .. } = &event.shape else {
        return Vec::new();
    };
    let tz = event
        .start
        .as_ref()
        .and_then(EventTime::zone_name)
        .or(event.calendar_time_zone.as_deref());
    rules.iter().map(|r| rrule::describe(r, tz)).collect()
}

fn render_markdown(
    profile: &CalendarRenderProfile,
    event: &NormalizedEvent,
    source_id: &str,
    start_instant: Option<&str>,
) -> Vec<Section> {
    let uuid = &event.event_uuid;
    let mut fm = String::with_capacity(512);
    fm.push_str("---\n");
    fm.push_str(&format!("markdown_uuid: {uuid}\n"));
    fm.push_str(&format!("source_id: {source_id}\n"));
    fm.push_str(&format!("provider: {}\n", profile.provider));
    fm.push_str(&format!("kind: {}\n", kind_of(event)));
    fm.push_str(&format!("calendar: {}\n", yaml_safe(&event.calendar_label)));
    fm.push_str(&format!("title: {}\n", yaml_safe(title_of(event))));
    fm.push_str(&format!("external_id: {}\n", yaml_safe(&event.upstream_id)));
    if let Some(s) = start_instant {
        fm.push_str(&format!("start: {s}\n"));
    }
    if let Some(ts) = &event.created {
        fm.push_str(&format!("created: {}\n", yaml_safe(ts)));
    }
    if let Some(ts) = &event.modified_at {
        fm.push_str(&format!("modified_at: {}\n", yaml_safe(ts)));
    }
    fm.push_str("---\n\n");

    let mut out = String::with_capacity(2048);
    out.push_str(
        &Title {
            suffix: None,
            text: title_of(event),
            markdown_uuid: Some(uuid),
            source_url: event.source_url.as_deref(),
        }
        .render(),
    );

    let mut facts: Vec<(&str, String)> = Vec::new();
    if let Some(w) = when_line(event) {
        facts.push(("When", w));
    }
    for r in repeats_lines(event) {
        facts.push(("Repeats", r));
    }
    if let EventShape::Occurrence {
        series,
        original_start,
    } = &event.shape
    {
        let of = match series {
            Some(s) => s.title.clone().unwrap_or_else(|| "a series".into()),
            None => "a series that is not on this calendar".into(),
        };
        facts.push((
            "Occurrence of",
            format!("{of}, originally {}", original_start.display()),
        ));
    }
    if let Some(l) = &event.location {
        facts.push(("Where", l.clone()));
    }
    facts.push(("Calendar", event.calendar_label.clone()));
    if let Some(s) = event.status.as_deref().filter(|s| *s != "confirmed") {
        facts.push(("Status", capitalize(s)));
    }
    if event.busy == Some(false) {
        facts.push(("Shows as", "Free".into()));
    }
    if let Some(o) = &event.organizer {
        if let Some(who) = person_line(o.name.as_deref(), o.email.as_deref()) {
            facts.push(("Organizer", who));
        }
    }
    if let Some(c) = &event.created {
        facts.push(("Added", c.clone()));
    }
    if let Some(m) = &event.modified_at {
        facts.push(("Last changed", m.clone()));
    }
    out.push_str("| | |\n| --- | --- |\n");
    for (k, v) in &facts {
        out.push_str(&format!("| {k} | {} |\n", cell(v)));
    }
    out.push('\n');

    if !event.attendees.is_empty() {
        out.push_str("## Attendees\n\n| Who | Response |\n| --- | --- |\n");
        for a in &event.attendees {
            let mut who = person_line(a.person.name.as_deref(), a.person.email.as_deref())
                .unwrap_or_else(|| "(unnamed)".into());
            if a.resource {
                who.push_str(" (room)");
            } else if a.optional {
                who.push_str(" (optional)");
            }
            let response = a.response.as_deref().map(response_word).unwrap_or("—");
            out.push_str(&format!("| {} | {response} |\n", cell(&who)));
        }
        out.push('\n');
    }

    if let Some(d) = &event.description {
        out.push_str("## Description\n\n");
        out.push_str(&text_block(d));
        out.push_str("\n\n");
    }

    if let EventShape::Series {
        rules,
        rdates,
        cancelled,
        changed,
    } = &event.shape
    {
        out.push_str("## Occurrences\n\n");
        for r in rules {
            out.push_str(&format!("Rule: `RRULE:{}`\n\n", r.replace('`', "")));
        }
        if !changed.is_empty() {
            out.push_str("Changed:\n\n");
            for c in changed {
                let now = c
                    .start
                    .as_ref()
                    .map(EventTime::display)
                    .unwrap_or_else(|| "no start".into());
                let title = c
                    .title
                    .as_deref()
                    .filter(|t| Some(*t) != event.title.as_deref())
                    .map(|t| format!(" — {}", escape_text(t)))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "- {} → {now}{title}\n",
                    c.original_start.display()
                ));
            }
            out.push('\n');
        }
        if !cancelled.is_empty() {
            out.push_str("Cancelled:\n\n");
            for c in cancelled {
                out.push_str(&format!("- {}\n", c.display()));
            }
            out.push('\n');
        }
        if !rdates.is_empty() {
            out.push_str("Added dates:\n\n");
            for d in rdates {
                out.push_str(&format!("- {}\n", d.display()));
            }
            out.push('\n');
        }
    }

    if !event.links.is_empty() {
        out.push_str("## Links\n\n");
        for l in &event.links {
            out.push_str(&format!(
                "- [{}](<{}>)\n",
                escape_text(&l.label).replace(['[', ']'], ""),
                l.url.replace(['<', '>'], "")
            ));
        }
        out.push('\n');
    }

    vec![Section::unkeyed(fm), Section::keyed(uuid, out)]
}

fn build_grid_row(
    profile: &CalendarRenderProfile,
    event: &NormalizedEvent,
    source_id: &str,
    md_rel: &str,
    start: Option<String>,
    problems: &mut Vec<ProblemRow>,
) -> Option<GridRow> {
    let title = title_of(event).to_string();
    let mut text: Vec<String> = vec![title.clone()];
    text.extend(when_line(event));
    text.extend(repeats_lines(event));
    text.extend(event.location.clone());
    text.push(event.calendar_label.clone());
    if let Some(o) = &event.organizer {
        text.extend(person_line(o.name.as_deref(), o.email.as_deref()));
    }
    text.extend(
        event
            .attendees
            .iter()
            .filter_map(|a| person_line(a.person.name.as_deref(), a.person.email.as_deref())),
    );
    text.extend(event.description.clone());

    GridRow::builder()
        .uuid(event.event_uuid.clone())
        .provider(profile.provider)
        .kind(kind_of(event).to_string())
        .source_label(profile.source_label.clone())
        .is_document(true)
        .created_at(start)
        // The grid orders a document's stamps created-then-modified, and
        // an event is nearly always last edited before it happens. The
        // edit stamp is on the page; the row carries only when it happens.
        .modified_at(None)
        .author(
            event
                .organizer
                .as_ref()
                .and_then(|o| o.label())
                .map(str::to_string),
        )
        .account(profile.account.clone())
        .channel(Some(event.calendar_label.clone()))
        .conversation_name(Some(title))
        .conversation_uuid(event.event_uuid.clone())
        .entire_chat(format!("/chat/{}", event.event_uuid))
        .body(text.join("\n"))
        .qmd_path(Some(md_rel.to_string()))
        .source_url(event.source_url.clone())
        .upstream_id(Some(event.upstream_id.clone()))
        .upstream_entity_kind(Some(event.upstream_entity_kind.to_string()))
        .markdown_uuid(Some(event.event_uuid.clone()))
        .build_or_record(
            source_id,
            &event.event_uuid,
            profile.render_version,
            problems,
        )
}

/// A series points at each of its changed occurrences, and an occurrence
/// back at its series, so either opens the other.
fn edges(event: &NormalizedEvent) -> Vec<EdgeRow> {
    let src = &event.event_uuid;
    let edge = |dst: &str, label: String| {
        let label: String = label.chars().take(EDGE_LABEL_CHARS).collect();
        EdgeRow {
            edge_uuid: datalib_id::edge_id(src, None, dst, None, Some(&label)),
            src_markdown_uuid: src.clone(),
            src_anchor_uuid: None,
            dst_markdown_uuid: dst.to_string(),
            dst_anchor_uuid: None,
            label: Some(label),
        }
    };
    match &event.shape {
        EventShape::Single => Vec::new(),
        EventShape::Series { changed, .. } => changed
            .iter()
            .map(|c| {
                edge(
                    &c.uuid,
                    format!("Changed: {}", c.original_start.display_date()),
                )
            })
            .collect(),
        EventShape::Occurrence { series, .. } => series
            .iter()
            .map(|s| {
                edge(
                    &s.uuid,
                    format!(
                        "Series: {}",
                        s.title.as_deref().unwrap_or("(untitled event)")
                    ),
                )
            })
            .collect(),
    }
}

fn person_line(name: Option<&str>, email: Option<&str>) -> Option<String> {
    match (name, email) {
        (Some(n), Some(e)) if n != e => Some(format!("{n} <{e}>")),
        (Some(n), _) => Some(n.to_string()),
        (None, Some(e)) => Some(e.to_string()),
        (None, None) => None,
    }
}

fn response_word(r: &str) -> &'static str {
    match r {
        "accepted" => "Accepted",
        "declined" => "Declined",
        "tentative" => "Maybe",
        "delegated" => "Delegated",
        _ => "No reply",
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

/// A table cell: HTML escaped, pipes escaped, one line.
fn cell(s: &str) -> String {
    escape_text(s).replace('|', "\\|").replace('\n', " ")
}

/// Plain text as a markdown block that reads as it was typed: HTML
/// escaped, a line that would open a heading or a quote escaped, and
/// every line break kept.
fn text_block(s: &str) -> String {
    s.trim()
        .lines()
        .map(|line| {
            let line = escape_text(line.trim_end());
            match line.chars().next() {
                Some('#') => format!("\\{line}"),
                _ => line,
            }
        })
        .collect::<Vec<_>>()
        .join("<br>\n")
}

fn yaml_safe(s: &str) -> String {
    if s.chars().any(|c| ":#[]{}&*?,|>'\"%@`\n".contains(c)) {
        format!(
            "\"{}\"",
            s.replace('\\', "\\\\")
                .replace('"', "\\\"")
                .replace('\n', " ")
        )
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Attendee, OccurrenceRef, Person, SeriesRef};

    const SERIES: &str = "11111111-1111-8111-8111-111111111111";
    const MOVED: &str = "22222222-2222-8222-8222-222222222222";

    fn la(v: &str) -> EventTime {
        EventTime::from_ical(v, Some("America/Los_Angeles")).unwrap()
    }

    fn base(uuid: &str, shape: EventShape) -> NormalizedEvent {
        NormalizedEvent {
            event_uuid: uuid.into(),
            shape,
            calendar_uuid: "33333333-3333-8333-8333-333333333333".into(),
            calendar_label: "Bridge".into(),
            calendar_time_zone: Some("America/Los_Angeles".into()),
            upstream_id: "bridge#staff".into(),
            upstream_entity_kind: "event",
            title: Some("Senior staff briefing".into()),
            start: Some(la("20260105T090000")),
            end: Some(la("20260105T100000")),
            status: Some("confirmed".into()),
            busy: Some(true),
            location: Some("Observation lounge".into()),
            description: Some("Ship's status.\n# not a heading\n<b>not bold</b>".into()),
            organizer: Some(Person {
                name: Some("Jean-Luc Picard".into()),
                email: Some("picard@enterprise.test".into()),
            }),
            attendees: vec![Attendee {
                person: Person {
                    name: Some("Deanna Troi".into()),
                    email: Some("troi@enterprise.test".into()),
                },
                response: Some("tentative".into()),
                optional: false,
                resource: false,
            }],
            links: Vec::new(),
            source_url: None,
            created: None,
            modified_at: Some("2026-09-02T16:00:00+00:00".into()),
            inputs: Vec::new(),
            problems: Vec::new(),
        }
    }

    fn series() -> NormalizedEvent {
        base(
            SERIES,
            EventShape::Series {
                rules: vec!["FREQ=WEEKLY;BYDAY=MO,TH;UNTIL=20261231T170000Z".into()],
                rdates: Vec::new(),
                cancelled: vec![la("20260209T090000")],
                changed: vec![OccurrenceRef {
                    uuid: MOVED.into(),
                    original_start: la("20260312T090000"),
                    start: Some(la("20260312T110000")),
                    title: Some("Senior staff briefing — Borg incursion".into()),
                }],
            },
        )
    }

    fn profile() -> CalendarRenderProfile {
        CalendarRenderProfile {
            provider: Provider::Test,
            source_label: "Fastmail Calendar".into(),
            account: Some("picard@enterprise.test".into()),
            render_version: 1,
        }
    }

    #[test]
    fn a_series_document_says_how_it_repeats_and_what_changed() {
        let md = join(&render_markdown(
            &profile(),
            &series(),
            "tng_calendar",
            None,
        ));
        assert!(
            md.contains("| When | Mon 5 Jan 2026, 09:00–10:00 (America/Los_Angeles) |"),
            "{md}"
        );
        assert!(
            md.contains("| Repeats | Weekly on Monday and Thursday, until Thu 31 Dec 2026 |"),
            "{md}"
        );
        assert!(md.contains("Rule: `RRULE:FREQ=WEEKLY;BYDAY=MO,TH;UNTIL=20261231T170000Z`"));
        assert!(md.contains("- Thu 12 Mar 2026, 09:00 (America/Los_Angeles) → Thu 12 Mar 2026, 11:00 (America/Los_Angeles) — Senior staff briefing — Borg incursion"), "{md}");
        assert!(md.contains("Cancelled:\n\n- Mon 9 Feb 2026, 09:00 (America/Los_Angeles)"));
        assert!(
            md.contains("| Deanna Troi &lt;troi@enterprise.test&gt; | Maybe |"),
            "{md}"
        );
        // Plain text stays plain.
        assert!(md.contains("\\# not a heading"));
        assert!(md.contains("&lt;b&gt;not bold&lt;/b&gt;"));
        assert!(!md.contains("| Status |"), "confirmed is not worth a line");
    }

    #[test]
    fn the_grid_row_sorts_by_when_the_event_happens() {
        let mut problems = Vec::new();
        let start = resolve_start(&series(), "tng_calendar", &profile(), &mut problems);
        assert_eq!(start.as_deref(), Some("2026-01-05T09:00:00-08:00"));
        let row = build_grid_row(
            &profile(),
            &series(),
            "tng_calendar",
            "x/index.md",
            start,
            &mut problems,
        )
        .expect("valid row");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(row.kind, KIND_SERIES);
        assert_eq!(row.created_at.as_deref(), Some("2026-01-05T09:00:00-08:00"));
        assert_eq!(
            row.modified_at, None,
            "an edit before the event would sort after it"
        );
        assert_eq!(row.author.as_deref(), Some("Jean-Luc Picard"));
        assert_eq!(row.channel.as_deref(), Some("Bridge"));
        assert!(row.text.contains("Weekly on Monday and Thursday"));
        assert!(row.text.contains("troi@enterprise.test"));
    }

    #[test]
    fn a_series_and_its_changed_occurrence_link_both_ways() {
        let out = edges(&series());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].dst_markdown_uuid, MOVED);
        assert_eq!(out[0].label.as_deref(), Some("Changed: Thu 12 Mar 2026"));
        let moved = base(
            MOVED,
            EventShape::Occurrence {
                series: Some(SeriesRef {
                    uuid: SERIES.into(),
                    title: Some("Senior staff briefing".into()),
                }),
                original_start: la("20260312T090000"),
            },
        );
        let back = edges(&moved);
        assert_eq!(back[0].dst_markdown_uuid, SERIES);
        assert_eq!(
            back[0].label.as_deref(),
            Some("Series: Senior staff briefing")
        );
        let md = join(&render_markdown(&profile(), &moved, "tng_calendar", None));
        assert!(md.contains("| Occurrence of | Senior staff briefing, originally Thu 12 Mar 2026, 09:00 (America/Los_Angeles) |"), "{md}");
    }

    #[test]
    fn an_unknown_zone_nulls_the_start_and_says_why() {
        let mut e = series();
        e.start = EventTime::from_ical("20260105T090000", Some("Pacific Standard Time"));
        let mut problems = Vec::new();
        assert_eq!(
            resolve_start(&e, "tng_calendar", &profile(), &mut problems),
            None
        );
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].reason, Reason::CoercionFailed);
    }

    /// The sink's answer is the run's answer.
    #[test]
    fn a_sink_that_refuses_fails_the_render() {
        let dir = tempfile::tempdir().unwrap();
        let mut refuse = |_: RenderedMarkdown| -> Result<()> { anyhow::bail!("no room") };
        let err = render_all(
            &profile(),
            &[series()],
            dir.path(),
            "tng_calendar",
            &Progress::default(),
            &mut refuse,
        )
        .expect_err("a refused document fails the render");
        assert!(format!("{err:#}").contains("no room"), "{err:#}");
    }
}
