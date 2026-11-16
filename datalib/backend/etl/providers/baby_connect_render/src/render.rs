//! One document per child per month: the month's days in order, every
//! entry logged for the child under its day, with who logged it and the
//! note typed with it. A grid row for the month and for each entry.
//! TRANSLATE.md says what becomes what.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{Duration, LocalResult, NaiveDate, NaiveDateTime, NaiveTime};
use datalib_etl::layout::render_markdown_root;
use datalib_etl_render::front_matter::{yaml_name, yaml_scalar};
use datalib_etl_render::grid_index::{doc_md_path, RenderedMarkdown};
use datalib_etl_render::html::{escape_md_block, escape_md_inline, escape_name_inline};
use datalib_etl_render::inputs::Inputs;
use datalib_etl_render::message::{stamp_html, stamp_text_html, timestamp_html};
use datalib_etl_render::section::{join, msg_div_open_with, Section, MSG_DIV_CLOSE};
use datalib_etl_render::title::Title;
use datalib_handle::Handle;
use datalib_id::Identity;
use datalib_schema::data_nodes::{DataNodeRow, Lineage};
use datalib_schema::grid_rows::GridRow;
use datalib_schema::problems::{Outcome, Problem, ProblemRow, Scope, Severity, Stage};
use datalib_schema::providers::Provider;
use datalib_time::{IsoOffsetTimestamp, NamedZone};
use serde_json::Value;

use crate::categories;
use crate::ids;
use crate::store::{id_of, month_of, str_of, Store};

/// v1: one document per child per month.
pub const RENDER_VERSION: u32 = 1;

const SOURCE_LABEL: &str = "Baby Connect";
const KIND_MONTH: &str = "Baby Connect Month";
const KIND_KID: &str = "Child";

/// When an entry was logged for: its day (`Pdt`) and its clock time
/// (`Utm`, `HHMM`), a reading with no zone.
pub fn wall_clock(entry: &Value) -> Option<NaiveDateTime> {
    let pdt = entry.get("Pdt")?.as_u64()?;
    let utm = entry.get("Utm").and_then(Value::as_u64).unwrap_or(0);
    let day = NaiveDate::from_ymd_opt(
        2000 + (pdt / 10000) as i32,
        (pdt / 100 % 100) as u32,
        (pdt % 100) as u32,
    )?;
    let time = NaiveTime::from_hms_opt((utm / 100) as u32, (utm % 100) as u32, 0)?;
    Some(day.and_time(time))
}

/// The moment a clock reading names in `zone`. In the hour the clocks
/// go back it is the first of the two; in the hour they skip, the reading
/// an hour later, which is what the clock on the wall showed.
pub fn in_zone(wall: &NaiveDateTime, zone: &NamedZone) -> Option<IsoOffsetTimestamp> {
    match zone.from_local(wall) {
        LocalResult::Single(t) | LocalResult::Ambiguous(t, _) => Some(t.into()),
        LocalResult::None => match zone.from_local(&(*wall + Duration::hours(1))) {
            LocalResult::Single(t) | LocalResult::Ambiguous(t, _) => Some(t.into()),
            LocalResult::None => None,
        },
    }
}

/// An entry's time: as the grid stores it, and as the page shows it.
/// With no zone the reading is taken as UTC and written `Z`, the clock
/// unknown.
fn entry_time(entry: &Value, zone: Option<&NamedZone>) -> Option<(String, String)> {
    let wall = wall_clock(entry)?;
    match zone {
        Some(zone) => {
            let t = in_zone(&wall, zone)?;
            Some((t.to_rfc3339_secs(), stamp_html(&t)))
        }
        None => {
            let ms = wall.and_utc().timestamp_millis();
            let t = IsoOffsetTimestamp::from_unix_millis(ms)?;
            Some((
                datalib_time::with_unknown_offset(t.to_rfc3339_secs()),
                timestamp_html(Some(ms), None),
            ))
        }
    }
}

/// Whoever logged an entry, as the day's answers named them.
struct Person {
    name: String,
    handle: Option<Handle>,
}

fn person(store: &Store, inputs: &Inputs, by: Option<String>) -> Option<Person> {
    let by = by?;
    inputs.read("users", &by);
    let Some(u) = store.users.get(&by) else {
        return Some(Person {
            name: format!("Baby Connect user {by}"),
            handle: None,
        });
    };
    let handle = str_of(u, "Email").and_then(Handle::email);
    let name = str_of(u, "Name")
        .map(String::from)
        .or_else(|| handle.as_ref().map(|h| h.value().to_string()))
        .unwrap_or_else(|| format!("Baby Connect user {by}"));
    Some(Person { name, handle })
}

/// `2068-10` as "October 2068".
fn month_title(month: &str) -> String {
    NaiveDate::parse_from_str(&format!("{month}-01"), "%Y-%m-%d")
        .map(|d| d.format("%B %Y").to_string())
        .unwrap_or_else(|_| month.to_string())
}

/// One child's month: what the store holds of it, written to its `.md`
/// under `root`. `inputs` gets every raw row it read.
pub fn render_month(
    root: &Path,
    source_id: &str,
    store: &Store,
    kid: &str,
    month: &str,
    zone: Option<&NamedZone>,
    inputs: &Inputs,
) -> Result<RenderedMarkdown> {
    let doc = build(source_id, store, kid, month, zone, inputs);
    let md_path = root.join(&doc.md_path);
    let dir = md_path.parent().expect("a document's file has a folder");
    fs::create_dir_all(dir).with_context(|| format!("mkdir -p {}", dir.display()))?;
    fs::write(&md_path, join(&doc.sections))
        .with_context(|| format!("write {}", md_path.display()))?;
    Ok(RenderedMarkdown { md_path, ..doc })
}

/// [`render_month`] without the file: the document as it would be
/// stored, its `md_path` relative to the data root.
pub fn build(
    source_id: &str,
    store: &Store,
    kid: &str,
    month: &str,
    zone: Option<&NamedZone>,
    inputs: &Inputs,
) -> RenderedMarkdown {
    inputs.read("kids", kid);
    let kid_row = store.kids.get(kid);
    let kid_name = kid_row
        .and_then(|k| str_of(k, "Name"))
        .map(String::from)
        .unwrap_or_else(|| format!("Child {kid}"));
    let id = ids::month(source_id, kid, month);
    let rel = doc_md_path(&render_markdown_root(Path::new(""), source_id), &id.uuid)
        .to_string_lossy()
        .into_owned();
    let title = format!("{kid_name} — {}", month_title(month));

    let mut entries: Vec<(&String, &Value)> = store
        .entries
        .iter()
        .filter(|(_, e)| {
            id_of(e, "Kid").as_deref() == Some(kid) && month_of(e).as_deref() == Some(month)
        })
        .collect();
    entries.sort_by_key(|(id, e)| (wall_clock(e), id.to_string()));

    let kid_id = ids::kid(source_id, kid);
    let kid_node = DataNodeRow::new(&id.uuid, kid_id.uuid.clone(), source_id, KIND_KID)
        .named(Some(kid_name.clone()))
        .upstream(ids::KIND_KID, kid, None)
        .place()
        .under(&Lineage::top());
    let in_kid = Lineage::top().below(&kid_node);
    let month_node = DataNodeRow::new(&id.uuid, id.uuid.clone(), source_id, KIND_MONTH)
        .named(Some(title.clone()))
        .upstream(ids::KIND_MONTH, id.natural_key.clone(), None)
        .sort_key(Some(month.to_string()))
        .read_together()
        .under(&in_kid);
    let in_month = in_kid.below(&month_node);

    let mut out = Out {
        source_id,
        doc: &id,
        md_rel: &rel,
        title: &title,
        kid_name: &kid_name,
        in_month,
        sections: vec![Section::unkeyed(head(&id, &title, kid, month, zone))],
        rows: Vec::new(),
        problems: Vec::new(),
        stamps: Vec::new(),
    };
    if zone.is_none() {
        out.problems.push(assumed_utc(source_id, &id.uuid));
    }
    let mut by_day: BTreeMap<Option<NaiveDate>, Vec<(&String, &Value)>> = BTreeMap::new();
    for (eid, e) in entries {
        inputs.read("entries", eid);
        by_day
            .entry(wall_clock(e).map(|w| w.date()))
            .or_default()
            .push((eid, e));
    }
    for (day, list) in by_day {
        let heading = match day {
            Some(d) => stamp_text_html(&d.format("%Y-%m-%d").to_string()),
            None => "Undated".to_string(),
        };
        out.sections
            .push(Section::unkeyed(format!("## {heading}\n\n")));
        for (eid, e) in list {
            let who = person(store, inputs, id_of(e, "By"));
            out.entry(eid, e, who, zone);
        }
    }

    let Out {
        sections,
        mut rows,
        mut problems,
        stamps,
        ..
    } = out;
    let items = rows.len() as i64;
    let created = stamps.iter().min().cloned();
    let modified = stamps.iter().max().cloned();
    let doc_row = GridRow::builder()
        .uuid(id.uuid.clone())
        .provider(Provider::BabyConnect)
        .kind(KIND_MONTH)
        .source_label(SOURCE_LABEL)
        .is_document(true)
        .item_count(Some(items))
        .created_at(created)
        .modified_at(modified)
        .channel(Some(kid_name.clone()))
        .conversation_name(Some(title.clone()))
        .conversation_uuid(id.uuid.clone())
        .entire_chat(format!("/chat/{}", id.uuid))
        .body(title.clone())
        .qmd_path(Some(rel.clone()))
        .upstream_id(Some(id.natural_key.clone()))
        .upstream_entity_kind(Some(id.entity_kind.to_string()))
        .markdown_uuid(Some(id.uuid.clone()))
        .lineage(&in_kid)
        .build_or_record(source_id, &id.natural_key, RENDER_VERSION, &mut problems);
    rows.splice(0..0, doc_row);
    RenderedMarkdown {
        section_layout: None,
        content_hash: None,
        markdown_uuid: id.uuid.clone(),
        source_id: source_id.to_string(),
        upstream_cursor: None,
        bucket_key: Some(crate::store::bucket(kid, month)),
        md_path: PathBuf::from(&rel),
        render_version: RENDER_VERSION,
        rows,
        sections,
        edges: Vec::new(),
        contacts: Vec::new(),
        data_nodes: vec![kid_node, month_node],
        filings: Vec::new(),
        photos: Vec::new(),
        problems,
    }
}

fn head(id: &Identity, title: &str, kid: &str, month: &str, zone: Option<&NamedZone>) -> String {
    let mut s = String::from("---\n");
    s.push_str(&format!("title: {}\n", yaml_name(title)));
    s.push_str(&format!("provider: {}\n", Provider::BabyConnect));
    s.push_str(&format!("source_label: {}\n", yaml_scalar(SOURCE_LABEL)));
    s.push_str(&format!("markdown_uuid: {}\n", id.uuid));
    s.push_str(&format!("kid_id: {}\n", yaml_scalar(kid)));
    s.push_str(&format!("month: {}\n", yaml_scalar(month)));
    s.push_str("---\n\n");
    s.push_str(
        &Title {
            text: title,
            suffix: None,
            markdown_uuid: Some(&id.uuid),
            source_url: None,
        }
        .render(),
    );
    if zone.is_none() {
        s.push_str(
            "*Baby Connect keeps no time zone and this source names none (`time_zone`), so \
             these times are read as UTC.*\n\n",
        );
    }
    s
}

fn assumed_utc(source_id: &str, uuid: &str) -> ProblemRow {
    ProblemRow::new(
        source_id,
        Stage::Render,
        Scope::Markdown(uuid),
        Some(uuid),
        Outcome::Nulled,
        Problem::lossy_explained(
            "entry_time_assumed_utc",
            Some("created_at".to_string()),
            "Baby Connect keeps an entry's time with no zone and the source names none \
             (`time_zone`), so its times are read as UTC",
        )
        .severity(Severity::Warning),
        Some(RENDER_VERSION),
    )
}

struct Out<'a> {
    source_id: &'a str,
    doc: &'a Identity,
    md_rel: &'a str,
    title: &'a str,
    kid_name: &'a str,
    in_month: Lineage,
    sections: Vec<Section>,
    rows: Vec<GridRow>,
    problems: Vec<ProblemRow>,
    /// Every entry's stamp, for the document row's span.
    stamps: Vec<String>,
}

impl Out<'_> {
    fn entry(&mut self, entry_id: &str, e: &Value, who: Option<Person>, zone: Option<&NamedZone>) {
        let time = entry_time(e, zone);
        let at = time.as_ref().map(|(at, _)| at.clone());
        let id = ids::entry(self.source_id, entry_id, at.as_deref());
        if let Some(at) = &at {
            self.stamps.push(at.clone());
        }
        let cat = e.get("Cat").and_then(Value::as_i64);
        let category = categories::name(cat);
        let text = str_of(e, "Txt");
        let note = str_of(e, "n");

        let mut s = msg_div_open_with(&id.uuid, Provider::BabyConnect, &["baby-connect-entry"]);
        s.push_str("\n\n");
        let mut line = Vec::new();
        if let Some((_, html)) = &time {
            line.push(html.clone());
        }
        line.push(format!("**{}**", escape_name_inline(&category)));
        if let Some(p) = &who {
            line.push(escape_name_inline(&p.name));
        }
        s.push_str(&line.join(" · "));
        s.push_str("\n\n");
        if let Some(t) = text {
            s.push_str(&escape_md_inline(t));
            s.push_str("\n\n");
        }
        if let Some(n) = note {
            for l in escape_md_block(n).lines() {
                s.push_str("> ");
                s.push_str(l);
                s.push('\n');
            }
            s.push('\n');
        }
        s.push_str(MSG_DIV_CLOSE);
        s.push_str("\n\n");
        self.sections.push(Section::keyed(&id.uuid, s));

        let body = [text, note]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("\n");
        let index = self.rows.len() as i64;
        let built = GridRow::builder()
            .uuid(id.uuid.clone())
            .provider(Provider::BabyConnect)
            .kind(category)
            .source_label(SOURCE_LABEL)
            .item_count(Some(1))
            .created_at(at)
            .author(who.as_ref().map(|p| p.name.clone()))
            .author_handle(who.and_then(|p| p.handle).map(|h| h.as_str().to_string()))
            .channel(Some(self.kid_name.to_string()))
            .conversation_name(Some(self.title.to_string()))
            .conversation_uuid(self.doc.uuid.clone())
            .message_index(Some(index))
            .entire_chat(format!("/chat/{}", self.doc.uuid))
            .body(body)
            .qmd_path(Some(self.md_rel.to_string()))
            .upstream_id(Some(id.natural_key.clone()))
            .upstream_entity_kind(Some(id.entity_kind.to_string()))
            .markdown_uuid(Some(self.doc.uuid.clone()))
            .lineage(&self.in_month)
            .build_or_record(self.source_id, entry_id, RENDER_VERSION, &mut self.problems);
        self.rows.extend(built);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn la() -> NamedZone {
        NamedZone::get("America/Los_Angeles").unwrap()
    }

    #[test]
    fn a_clock_reading_takes_the_zones_offset_on_its_date() {
        let winter = wall_clock(&json!({"Pdt": 140119, "Utm": 210})).unwrap();
        assert_eq!(
            in_zone(&winter, &la()).unwrap().to_rfc3339_secs(),
            "2014-01-19T02:10:00-08:00"
        );
        let summer = wall_clock(&json!({"Pdt": 140719, "Utm": 1430})).unwrap();
        assert_eq!(
            in_zone(&summer, &la()).unwrap().to_rfc3339_secs(),
            "2014-07-19T14:30:00-07:00"
        );
    }

    /// 2:30 on the night the clocks spring forward never happened; a
    /// parent logging then read 3:30 on the wall.
    #[test]
    fn a_reading_in_the_skipped_hour_moves_past_it() {
        let skipped = wall_clock(&json!({"Pdt": 140309, "Utm": 230})).unwrap();
        assert_eq!(
            in_zone(&skipped, &la()).unwrap().to_rfc3339_secs(),
            "2014-03-09T03:30:00-07:00"
        );
    }

    #[test]
    fn with_no_zone_a_time_says_its_clock_is_unknown() {
        let (at, _) = entry_time(&json!({"Pdt": 140119, "Utm": 210}), None).unwrap();
        assert_eq!(at, "2014-01-19T02:10:00Z");
        assert_eq!(wall_clock(&json!({"Pdt": 141319})), None);
    }

    fn store() -> Store {
        let mut s = Store::default();
        s.kids.insert(
            "11".into(),
            json!({"Id": 11, "Name": "Wesley", "BDay": "10/12/2048"}),
        );
        s.users.insert(
            "7".into(),
            json!({"Id": 7, "Name": "Beverly Crusher", "Email": "beverly@enterprise.test"}),
        );
        s.entries.insert(
            "2".into(),
            json!({"Id": 2, "Kid": 11, "Pdt": 481013, "Utm": 905, "Cat": 2600, "By": 7,
                   "Txt": "Diary", "n": "He grabbed Jack's finger.\nJack says he has an engineer's grip."}),
        );
        s.entries.insert(
            "1".into(),
            json!({"Id": 1, "Kid": 11, "Pdt": 481012, "Utm": 210, "Cat": 352, "By": 7,
                   "Txt": "Wesley nursed (left) for 15 min"}),
        );
        s.entries.insert(
            "3".into(),
            json!({"Id": 3, "Kid": 12, "Pdt": 481012, "Utm": 300, "Cat": 402, "By": 7,
                   "Txt": "Another child's diaper"}),
        );
        s
    }

    #[test]
    fn a_month_holds_one_childs_entries_in_time_order_with_their_notes() {
        let doc = build(
            "bc",
            &store(),
            "11",
            "2048-10",
            Some(&la()),
            &Inputs::default(),
        );
        let md = join(&doc.sections);
        assert!(md.contains("Wesley — October 2048"), "{md}");
        let nursed = md.find("nursed").unwrap();
        let diary = md.find("grabbed").unwrap();
        assert!(nursed < diary, "{md}");
        assert!(
            md.contains("> Jack says he has an engineer's grip."),
            "{md}"
        );
        assert!(!md.contains("Another child"), "{md}");
        assert!(md.contains("2048-10-12T02:10:00-07:00"), "{md}");

        assert_eq!(doc.rows.iter().filter(|r| r.is_document).count(), 1);
        let entry_rows: Vec<&GridRow> = doc.rows.iter().filter(|r| !r.is_document).collect();
        assert_eq!(entry_rows.len(), 2);
        assert_eq!(entry_rows[0].kind, "Nursing");
        assert_eq!(entry_rows[1].kind, "Diary");
        assert!(
            entry_rows[1].preview.contains("engineer's grip"),
            "{}",
            entry_rows[1].preview
        );
        assert_eq!(entry_rows[1].author.as_deref(), Some("Beverly Crusher"));
        assert_eq!(
            entry_rows[1].author_handle.as_deref(),
            Some("email:beverly@enterprise.test")
        );
        assert!(doc.problems.is_empty(), "{:?}", doc.problems);
    }

    #[test]
    fn a_month_with_no_zone_says_its_times_are_read_as_utc() {
        let doc = build("bc", &store(), "11", "2048-10", None, &Inputs::default());
        assert_eq!(doc.problems.len(), 1);
        assert!(join(&doc.sections).contains("read as UTC"));
    }
}
