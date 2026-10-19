//! A page per thing the account kept: a Maps list (`Saved/`), a My Maps
//! map, a Play Books book. Each is one document; its entries are the
//! list's places, the map's placemarks and the book's highlights, notes,
//! bookmarks and listening positions. Every key of a row this render does
//! not read is a warning on its entry, by shape.

use std::collections::BTreeMap;

use datalib_etl::bulk::BulkUpsertable;
use datalib_etl_chat_common::findings::{noted, shape_of, unread_keys};
use datalib_etl_chat_common::types::{NormalizedChat, NormalizedChatItem};
use datalib_etl_google_takeout::ingest::schema_raw::{
    MapsSavedListRow, MyMapsPlacemarkRow, PlayBooksAnnotationRow,
};
use datalib_etl_render::html::{escape_md_block, escape_md_inline, md_link_dest};
use datalib_etl_render::inputs::Inputs;
use datalib_schema::problems::{Problem, Reason};
use serde_json::Value;

use crate::feeds::{self, Row};
use crate::ids;

/// A raw row as a page reads it: its table's row and the file it is from.
pub struct FileRow {
    pub row: Row,
    pub file: String,
}

fn by_file(rows: &[FileRow]) -> BTreeMap<&str, Vec<&FileRow>> {
    let mut out: BTreeMap<&str, Vec<&FileRow>> = BTreeMap::new();
    for r in rows {
        out.entry(r.file.as_str()).or_default().push(r);
    }
    for list in out.values_mut() {
        list.sort_by_key(|r| r.row.payload.get("position").and_then(Value::as_u64));
    }
    out
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

// ── Saved lists ─────────────────────────────────────────────────────

const LIST_KEYS: &[&str] = &["list", "description", "position", "row"];
/// `Tags` and `Comment` are columns every list has, empty on every list
/// seen; drawn when they say something.
const LIST_ROW_KEYS: &[&str] = &["Title", "Note", "URL", "Tags", "Comment"];

pub fn saved_lists(source_id: &str, rows: &[FileRow]) -> Vec<NormalizedChat> {
    by_file(rows)
        .into_iter()
        .map(|(file, rows)| {
            let chat_id = format!("{}{file}", feeds::SAVED_LIST);
            let inputs = Inputs::default();
            let first = &rows[0].row.payload;
            let name = str_of(first, "list").unwrap_or("Saved list").to_string();
            let mut items = Vec::new();
            if let Some(d) = str_of(first, "description") {
                items.push(feeds::description_item(
                    source_id,
                    &chat_id,
                    escape_md_block(d),
                ));
            }
            for r in rows {
                inputs.read(MapsSavedListRow::TABLE, &r.row.id);
                items.push(saved_list_item(source_id, &r.row));
            }
            feeds::whole(source_id, &chat_id, &name, items, inputs)
        })
        .collect()
}

fn saved_list_item(source_id: &str, row: &Row) -> NormalizedChatItem {
    let p = &row.payload;
    let mut problems = unread_keys(p, LIST_KEYS, "");
    let cells = p.get("row").cloned().unwrap_or(Value::Null);
    problems.extend(unread_keys(&cells, LIST_ROW_KEYS, "/row"));
    let title = str_of(&cells, "Title");
    let url = str_of(&cells, "URL");
    let heading = match (title, url) {
        (Some(t), Some(u)) => Some(format!(
            "**[{}]({})**",
            escape_md_inline(t),
            md_link_dest(u)
        )),
        (Some(t), None) => Some(format!("**{}**", escape_md_inline(t))),
        (None, Some(u)) => Some(format!("[{}]({})", escape_md_inline(u), md_link_dest(u))),
        (None, None) => None,
    };
    let mut parts: Vec<String> = heading.into_iter().collect();
    for key in ["Note", "Comment"] {
        if let Some(t) = str_of(&cells, key) {
            parts.push(escape_md_block(t));
        }
    }
    if let Some(tags) = str_of(&cells, "Tags") {
        parts.push(format!("Tags: {}", escape_md_inline(tags)));
    }
    if parts.is_empty() {
        problems.push(noted(
            "row",
            &format!("a list entry with nothing to show: {}", shape_of(&cells)),
        ));
    }
    let mut item = feeds::own_item(
        ids::feed_item(source_id, ids::KIND_SAVED_LIST_ITEM, &row.id, None),
        None,
        "Google Maps Saved Item",
        problems,
    );
    item.text = (!parts.is_empty()).then(|| parts.join("\n\n"));
    item.source_url = url.map(str::to_string);
    item
}

// ── My Maps ─────────────────────────────────────────────────────────

const PLACEMARK_PAYLOAD_KEYS: &[&str] = &["map", "folder", "placemark", "position"];
const MAP_KEYS: &[&str] = &["name", "description"];
const FOLDER_KEYS: &[&str] = &["name", "description"];
/// `styleUrl` names the icon or line style: how it looks.
const PLACEMARK_KEYS: &[&str] = &[
    "name",
    "description",
    "styleUrl",
    "Point",
    "LineString",
    "Polygon",
    "ExtendedData",
];
/// `tessellate` says how a line follows the ground: how it is drawn.
const GEOMETRY_KEYS: &[&str] = &["coordinates", "tessellate", "outerBoundaryIs"];

pub fn my_maps(source_id: &str, rows: &[FileRow]) -> Vec<NormalizedChat> {
    by_file(rows)
        .into_iter()
        .map(|(file, rows)| {
            let chat_id = format!("{}{file}", feeds::MY_MAP);
            let inputs = Inputs::default();
            let map = &rows[0].row.payload["map"];
            let name = str_of(map, "name").unwrap_or("My Maps map").to_string();
            let mut items = Vec::new();
            if let Some(d) = str_of(map, "description") {
                items.push(feeds::description_item(
                    source_id,
                    &chat_id,
                    feeds::html_markdown(d),
                ));
            }
            for (i, r) in rows.iter().enumerate() {
                inputs.read(MyMapsPlacemarkRow::TABLE, &r.row.id);
                let mut item = placemark_item(source_id, &r.row);
                // What the map and its layers leave unread is said once.
                if i == 0 {
                    item.problems.extend(unread_keys(map, MAP_KEYS, "/map"));
                }
                items.push(item);
            }
            feeds::whole(source_id, &chat_id, &name, items, inputs)
        })
        .collect()
}

fn placemark_item(source_id: &str, row: &Row) -> NormalizedChatItem {
    let p = &row.payload;
    let mut problems = unread_keys(p, PLACEMARK_PAYLOAD_KEYS, "");
    let folder = p.get("folder").filter(|f| !f.is_null());
    if let Some(f) = folder {
        problems.extend(unread_keys(f, FOLDER_KEYS, "/folder"));
    }
    let pm = &p["placemark"];
    problems.extend(unread_keys(pm, PLACEMARK_KEYS, "/placemark"));

    let mut parts: Vec<String> = Vec::new();
    if let Some(name) = str_of(pm, "name") {
        parts.push(format!("**{}**", escape_md_inline(name)));
    }
    if let Some(d) = str_of(pm, "description") {
        parts.push(feeds::html_markdown(d));
    }
    let (kind_label, place) = geometry(pm, &mut problems);
    parts.extend(place);
    for url in media_links(pm, &mut problems) {
        parts.push(format!(
            "🖼 [{}]({})",
            escape_md_inline(&url),
            md_link_dest(&url)
        ));
    }
    if parts.is_empty() {
        problems.push(noted(
            "placemark",
            &format!("a placemark with nothing to show: {}", shape_of(pm)),
        ));
    }
    let mut item = feeds::own_item(
        ids::feed_item(source_id, ids::KIND_MY_MAPS_PLACEMARK, &row.id, None),
        None,
        kind_label,
        problems,
    );
    item.text = (!parts.is_empty()).then(|| parts.join("\n\n"));
    item.labels = folder
        .and_then(|f| str_of(f, "name"))
        .map(|n| vec![n.to_string()])
        .unwrap_or_default();
    item
}

/// What the placemark is, and a line saying where: a pin links to its
/// spot, a line or a shape says how many points it has.
fn geometry(pm: &Value, problems: &mut Vec<Problem>) -> (&'static str, Option<String>) {
    for (key, label) in [
        ("Point", "My Maps Pin"),
        ("LineString", "My Maps Line"),
        ("Polygon", "My Maps Shape"),
    ] {
        let Some(g) = pm.get(key) else { continue };
        problems.extend(unread_keys(g, GEOMETRY_KEYS, &format!("/placemark/{key}")));
        let coordinates = g
            .pointer("/outerBoundaryIs/LinearRing/coordinates")
            .or_else(|| g.get("coordinates"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let points: Vec<&str> = coordinates.split_whitespace().collect();
        let line = if key == "Point" {
            points.first().and_then(|p| {
                let mut it = p.split(',');
                let (lon, lat) = (it.next()?, it.next()?);
                let url = format!("https://www.google.com/maps/search/?api=1&query={lat},{lon}");
                Some(format!("📍 [{lat}, {lon}]({})", md_link_dest(&url)))
            })
        } else {
            Some(format!("〰 {} points", points.len()))
        };
        return (label, line);
    }
    ("My Maps Placemark", None)
}

/// `ExtendedData`'s `gx_media_links`: the pictures attached to a pin, as
/// URLs separated by spaces.
fn media_links(pm: &Value, problems: &mut Vec<Problem>) -> Vec<String> {
    let Some(data) = pm.pointer("/ExtendedData/Data") else {
        return Vec::new();
    };
    let entries: Vec<&Value> = match data {
        Value::Array(a) => a.iter().collect(),
        v => vec![v],
    };
    let mut out = Vec::new();
    for (i, d) in entries.into_iter().enumerate() {
        let path = format!("/placemark/ExtendedData/Data/{i}");
        problems.extend(unread_keys(d, &["@name", "value"], &path));
        let value = d.get("value").and_then(Value::as_str).unwrap_or("");
        match d.get("@name").and_then(Value::as_str) {
            Some("gx_media_links") => out.extend(value.split_whitespace().map(str::to_string)),
            _ => {
                problems.push(Problem::field("Data", Reason::UncoveredType, &shape_of(d)).at(path))
            }
        }
    }
    out
}

// ── Play Books ──────────────────────────────────────────────────────

const ENTRY_PAYLOAD_KEYS: &[&str] = &["book", "section", "kind", "fields", "position"];
/// `meta-entry` is the book's links and publisher lines.
const BOOK_KEYS: &[&str] = &["title", "author", "narrator", "meta-entry", "links"];
/// `collection` is an empty box on every entry seen.
const ENTRY_KEYS: &[&str] = &[
    "book-text",
    "annotation-text",
    "last-modified-date",
    "document-position-text",
    "collection",
    "link",
];

pub fn play_books(source_id: &str, rows: &[FileRow]) -> Vec<NormalizedChat> {
    by_file(rows)
        .into_iter()
        .map(|(file, mut rows)| {
            let chat_id = format!("{}{file}", feeds::PLAY_BOOK);
            let inputs = Inputs::default();
            let book = &rows[0].row.payload["book"];
            let title = str_of(book, "title").unwrap_or("A book").to_string();
            let mut items = Vec::new();
            let by: Vec<String> = [("author", "By"), ("narrator", "Narrated by")]
                .iter()
                .filter_map(|(k, label)| str_of(book, k).map(|v| format!("{label} {v}")))
                .collect();
            if !by.is_empty() {
                items.push(feeds::note_item(source_id, &chat_id, by.join(" · ")));
            }
            rows.sort_by_key(|r| r.row.when.clone());
            for (i, r) in rows.iter().enumerate() {
                inputs.read(PlayBooksAnnotationRow::TABLE, &r.row.id);
                let mut item = entry_item(source_id, &r.row);
                if i == 0 {
                    item.problems.extend(unread_keys(book, BOOK_KEYS, "/book"));
                }
                items.push(item);
            }
            let mut chat = feeds::whole(source_id, &chat_id, &title, items, inputs);
            chat.source_url = book
                .get("links")
                .and_then(Value::as_array)
                .and_then(|l| l.first())
                .and_then(|l| str_of(l, "href"))
                .map(str::to_string);
            chat
        })
        .collect()
}

fn entry_item(source_id: &str, row: &Row) -> NormalizedChatItem {
    let p = &row.payload;
    let mut problems = unread_keys(p, ENTRY_PAYLOAD_KEYS, "");
    let fields = p.get("fields").cloned().unwrap_or(Value::Null);
    problems.extend(unread_keys(&fields, ENTRY_KEYS, "/fields"));
    let quote = str_of(&fields, "book-text");
    let note = str_of(&fields, "annotation-text");
    let position = str_of(&fields, "document-position-text");
    let kind_label = match (str_of(p, "section"), note) {
        (Some("My notes"), Some(_)) => "Play Books Note",
        (Some("My notes"), None) => "Play Books Highlight",
        (Some("My bookmarks"), _) => "Play Books Bookmark",
        (Some("Listening History"), _) => "Play Books Listening Position",
        (section, _) => {
            problems.push(noted(
                "section",
                &format!(
                    "a section this render does not know: {}",
                    section.map_or("none".to_string(), |s| format!(
                        "string({} chars)",
                        s.chars().count()
                    ))
                ),
            ));
            "Play Books Entry"
        }
    };
    let mut parts: Vec<String> = Vec::new();
    if let Some(q) = quote {
        parts.push(
            escape_md_block(q)
                .lines()
                .map(|l| format!("> {l}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    if let Some(n) = note {
        parts.push(escape_md_block(n));
    }
    if let Some(at) = position {
        parts.push(format!("*{}*", escape_md_inline(at)));
    }
    let raw = str_of(&fields, "last-modified-date").map(|s| s.replace('\n', " "));
    let row_for_stamp = Row {
        when: row.when.clone().or(raw),
        ..row.clone()
    };
    let date_ms = feeds::stamp_ms(&row_for_stamp, &mut problems);
    let mut item = feeds::own_item(
        ids::feed_item(source_id, ids::KIND_PLAY_BOOKS_ENTRY, &row.id, date_ms),
        date_ms,
        kind_label,
        problems,
    );
    item.text = (!parts.is_empty()).then(|| parts.join("\n\n"));
    item.source_url = str_of(&fields, "link").map(str::to_string);
    item
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl_chat_common::types::ItemKind;
    use serde_json::json;

    fn file_row(id: &str, file: &str, when: Option<&str>, payload: Value) -> FileRow {
        FileRow {
            row: Row {
                id: id.to_string(),
                payload,
                when: when.map(str::to_string),
            },
            file: file.to_string(),
        }
    }

    #[test]
    fn a_list_is_a_page_of_its_places_with_their_notes() {
        let rows = [
            file_row(
                "b",
                "Saved/Shore leave.csv",
                None,
                json!({"list": "Shore leave", "description": "Where the crew unwinds", "position": 1,
                "row": {"Title": "Ten Forward", "Note": "Ask Guinan", "URL": "https://www.google.com/maps/place/tf", "Tags": "", "Comment": ""}}),
            ),
            file_row(
                "a",
                "Saved/Shore leave.csv",
                None,
                json!({"list": "Shore leave", "description": "Where the crew unwinds", "position": 0,
                "row": {"Title": "Risa", "Note": "", "URL": "https://www.google.com/maps/place/risa", "Tags": "", "Comment": "", "Rating": "5"}}),
            ),
        ];
        let chats = saved_lists("gt", &rows);
        let [chat] = chats.as_slice() else {
            panic!("one list")
        };
        assert_eq!(chat.display, "Shore leave");
        let items = &chat.buckets[0].items;
        assert_eq!(items[0].kind, ItemKind::Text, "searched with the rest");
        assert_eq!(items[0].text.as_deref(), Some("Where the crew unwinds"));
        assert_eq!(
            items[1].text.as_deref(),
            Some("**[Risa](https://www.google.com/maps/place/risa)**")
        );
        assert_eq!(
            items[2].text.as_deref(),
            Some("**[Ten Forward](https://www.google.com/maps/place/tf)**\n\nAsk Guinan")
        );
        let unread: Vec<&str> = items[1]
            .problems
            .iter()
            .filter_map(|p| p.field.as_deref())
            .collect();
        assert_eq!(unread, ["Rating"]);
    }

    #[test]
    fn a_map_is_a_page_of_its_placemarks_under_their_layers() {
        let map = json!({"name": "Away missions", "description": ""});
        let rows = [
            file_row(
                "p1",
                "My Maps/Away.kmz",
                None,
                json!({"map": map, "folder": {"name": "Shore leave"}, "position": 0,
                "placemark": {"name": "Risa", "description": "Horga'hn optional.<br>Bring sunscreen.", "styleUrl": "#icon-1",
                    "Point": {"coordinates": "-122.4,37.8,0"},
                    "ExtendedData": {"Data": {"@name": "gx_media_links", "value": "https://memory-alpha.example/risa.jpg"}}}}),
            ),
            file_row(
                "p2",
                "My Maps/Away.kmz",
                None,
                json!({"map": map, "folder": {"name": "Patrol"}, "position": 1,
                "placemark": {"name": "Neutral Zone", "LineString": {"tessellate": "1", "coordinates": "1,2,0 3,4,0"}, "visibility": "0"}}),
            ),
        ];
        let chats = my_maps("gt", &rows);
        let items = &chats[0].buckets[0].items;
        assert_eq!(chats[0].display, "Away missions");
        assert_eq!(items[0].labels, ["Shore leave"]);
        assert_eq!(items[0].kind_label.as_deref(), Some("My Maps Pin"));
        let text = items[0].text.as_deref().unwrap();
        assert!(text.starts_with("**Risa**\n\nHorga'hn optional."), "{text}");
        assert!(text.contains("📍 [37.8, -122.4]("), "{text}");
        assert!(
            text.ends_with(
                "🖼 [https://memory-alpha.example/risa.jpg](https://memory-alpha.example/risa.jpg)"
            ),
            "{text}"
        );
        assert!(items[0].problems.is_empty(), "{:?}", items[0].problems);
        assert!(items[1].text.as_deref().unwrap().ends_with("〰 2 points"));
        let unread: Vec<&str> = items[1]
            .problems
            .iter()
            .filter_map(|p| p.field.as_deref())
            .collect();
        assert_eq!(unread, ["visibility"]);
    }

    #[test]
    fn a_book_is_a_page_of_its_highlights_and_notes_in_date_order() {
        let book = json!({"title": "The Art of Diplomacy", "author": "Jean-Luc Picard"});
        let rows = [
            file_row(
                "n2",
                "Google Play Books/D/D.html",
                Some("2364-02-04T18:00:00-08:00"),
                json!({"book": book, "section": "My notes", "kind": "annotation", "position": 1,
                       "fields": {"book-text": "Tea, Earl Grey, hot.", "last-modified-date": "Last modified on\nFeb 4, 2364, 6:00:00 PM\nPacific Time"}}),
            ),
            file_row(
                "n1",
                "Google Play Books/D/D.html",
                Some("2364-01-12T09:41:00-08:00"),
                json!({"book": book, "section": "My notes", "kind": "annotation", "position": 0,
                       "fields": {"book-text": "Make it so.", "annotation-text": "The captain's motto.", "last-modified-date": "…"}}),
            ),
        ];
        let chats = play_books("gt", &rows);
        let items = &chats[0].buckets[0].items;
        assert_eq!(chats[0].display, "The Art of Diplomacy");
        assert_eq!(items[0].system_note.as_deref(), Some("By Jean-Luc Picard"));
        assert_eq!(items[1].kind_label.as_deref(), Some("Play Books Note"));
        assert_eq!(
            items[1].text.as_deref(),
            Some("> Make it so.\n\nThe captain's motto.")
        );
        assert_eq!(items[2].kind_label.as_deref(), Some("Play Books Highlight"));
        assert!(items[1].date_ms < items[2].date_ms);
    }
}
