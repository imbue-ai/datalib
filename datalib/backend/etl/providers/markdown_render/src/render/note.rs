//! One note as its document and its grid row.

use std::path::Path;

use datalib_etl_markdown::ingest::schema_raw::MarkdownFileRow;
use datalib_etl_render::front_matter::Front;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::html::{close_open_blocks, md_code_block};
use datalib_etl_render::section::Section;
use datalib_etl_render::title::Title;
use datalib_id::{IdNamespace, Identity};
use datalib_schema::data_nodes::DataNodeRow;
use datalib_schema::grid_rows::GridRow;
use datalib_schema::problems::ProblemRow;
use datalib_schema::providers::Provider;

use super::RENDER_VERSION;

pub const PROVIDER: Provider = Provider::Markdown;
pub const SOURCE_LABEL: &str = "Markdown";
pub const KIND_NOTE: &str = "Note";

/// The `entity_kind` of a note's id. Unlike [`KIND_NOTE`], which is the
/// grid's label and may be reworded, this is part of the uuid.
pub const ENTITY_KIND_NOTE: &str = "note";

/// A note is its path under the scan root, so the id is minted from the
/// path and carries no stamp: an edit moves the file's mtime, and must
/// not move the note's id.
pub fn note_id(source_id: &str, rel_path: &str) -> Identity {
    Identity::mint(
        IdNamespace::Markdown,
        source_id,
        None,
        ENTITY_KIND_NOTE,
        rel_path.to_string(),
        None,
    )
}

/// The name a person knows a note by: its file name without `.md`, the
/// way Obsidian titles it.
pub fn title_of(rel_path: &str) -> String {
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    let lower = name.to_ascii_lowercase();
    [".markdown", ".md"]
        .iter()
        .find(|ext| lower.ends_with(*ext) && name.len() > ext.len())
        .map_or(name, |ext| &name[..name.len() - ext.len()])
        .to_string()
}

/// A note split into the YAML properties block it opens with, if it opens
/// with one (`---` on the first line, closed by `---` or `...`), and the
/// rest. A note whose block never closes is all body, as an editor reads
/// it.
pub fn split_properties(text: &str) -> (Option<&str>, &str) {
    let is_marker = |line: &str, marks: &[&str]| marks.contains(&line.trim_end());
    let Some(first_end) = text.find('\n') else {
        return (None, text);
    };
    if !is_marker(&text[..first_end], &["---"]) {
        return (None, text);
    }
    let mut at = first_end + 1;
    while at <= text.len() {
        let end = text[at..].find('\n').map_or(text.len(), |i| at + i);
        if is_marker(&text[at..end], &["---", "..."]) {
            let properties = text[first_end + 1..at].trim_end_matches(['\n', '\r']);
            let body = text.get(end + 1..).unwrap_or("");
            return (Some(properties), body);
        }
        at = end + 1;
    }
    (None, text)
}

/// `file:///…` for the note's place on disk; `None` where the path does
/// not make one, since a half-valid link is worse than none.
pub fn file_url(root: &Path, rel_path: &str) -> Option<String> {
    url::Url::from_file_path(root.join(rel_path))
        .ok()
        .map(|u| u.to_string())
}

pub fn render_note(
    source_id: &str,
    note: &MarkdownFileRow,
    root: Option<&Path>,
) -> RenderedMarkdown {
    let id = note_id(source_id, &note.id);
    let title = title_of(&note.id);
    let url = root.and_then(|r| file_url(r, &note.id));
    let (properties, body) = split_properties(&note.text);

    let front = Front::new()
        .name("title", &title)
        .plain("provider", PROVIDER)
        .text("source_label", SOURCE_LABEL)
        .plain("markdown_uuid", &id.uuid)
        .text("source_path", &note.id)
        .text_if("modified_at", note.modified_at.as_deref())
        .text_if("source_url", url.as_deref());
    let mut head = front.draw();
    head.push_str(
        &Title {
            text: &title,
            suffix: None,
            markdown_uuid: Some(&id.uuid),
            source_url: url.as_deref(),
        }
        .render(),
    );
    // Each piece is drawn alone, so nothing the note's own markup leaves
    // open reaches past it.
    let mut sections = vec![Section::unkeyed(head)];
    if let Some(p) = properties.filter(|p| !p.trim().is_empty()) {
        sections.push(Section::unkeyed(format!(
            "{}\n\n",
            md_code_block("yaml", p)
        )));
    }
    if !body.trim().is_empty() {
        sections.push(Section::unkeyed(format!(
            "{}\n",
            close_open_blocks(body.trim_end())
        )));
    }

    let mut problems: Vec<ProblemRow> = Vec::new();
    let row = GridRow::builder()
        .uuid(id.uuid.clone())
        .provider(PROVIDER)
        .kind(KIND_NOTE)
        .source_label(SOURCE_LABEL)
        .is_document(true)
        .item_count(Some(1))
        .modified_at(note.modified_at.clone())
        .conversation_name(Some(title.clone()))
        .conversation_uuid(id.uuid.clone())
        .entire_chat(format!("/chat/{}", id.uuid))
        .body(format!("{title}\n{}", note.text))
        .source_url(url)
        .upstream_id(Some(id.natural_key.clone()))
        .upstream_entity_kind(Some(id.entity_kind.to_string()))
        .markdown_uuid(Some(id.uuid.clone()))
        .build_or_record(source_id, &note.id, RENDER_VERSION, &mut problems);
    let node = DataNodeRow::new(&id.uuid, &id.uuid, source_id, KIND_NOTE)
        .named(Some(title))
        .upstream(id.entity_kind, id.natural_key.clone(), None)
        .read_together();

    RenderedMarkdown {
        searchable: true,
        content_hash: None,
        markdown_uuid: id.uuid.clone(),
        source_id: source_id.to_string(),
        upstream_cursor: None,
        render_unit_key: Some(note.id.clone()),
        render_version: RENDER_VERSION,
        rows: row.into_iter().collect(),
        sections,
        front,
        edges: Vec::new(),
        contacts: Vec::new(),
        data_nodes: vec![node],
        filings: Vec::new(),
        photos: Vec::new(),
        problems,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl_render::section::join;

    fn note(id: &str, text: &str) -> MarkdownFileRow {
        MarkdownFileRow {
            id: id.into(),
            blake3: "ab".into(),
            size: text.len() as i64,
            modified_at: Some("2364-04-13T15:45:00Z".into()),
            text: text.into(),
        }
    }

    #[test]
    fn a_title_is_the_file_name_without_its_extension() {
        assert_eq!(title_of("Bridge/Warp core.md"), "Warp core");
        assert_eq!(title_of("Roster.MARKDOWN"), "Roster");
        assert_eq!(title_of("log.md.md"), "log.md");
        assert_eq!(title_of(".md"), ".md");
    }

    #[test]
    fn properties_are_split_off_only_when_their_block_closes() {
        let (p, body) = split_properties("---\ntags: [log]\n---\n# Log\n");
        assert_eq!((p, body), (Some("tags: [log]"), "# Log\n"));
        let (p, body) = split_properties("---\r\na: 1\r\n...\r\nbody");
        assert_eq!((p, body), (Some("a: 1"), "body"));
        assert_eq!(
            split_properties("---\nno close\n"),
            (None, "---\nno close\n")
        );
        assert_eq!(split_properties("# Log\n---\n"), (None, "# Log\n---\n"));
        assert_eq!(split_properties("---\n---"), (Some(""), ""));
    }

    /// The note's path is in the front matter as `source_path`, which is
    /// what makes search say where a note is filed.
    #[test]
    fn the_path_is_a_fact_and_the_properties_are_shown_as_written() {
        let md = render_note(
            "notes",
            &note(
                "Bridge/Warp core.md",
                "---\ntags: [engineering]\n---\nAntimatter at 98%.\n",
            ),
            Some(Path::new("/vault")),
        );
        let path = md.front.facts().iter().find(|f| f.key == "source_path");
        assert!(path.is_some(), "{:?}", md.front);
        let page = join(&md.sections);
        assert!(
            page.contains("source_path: \"Bridge/Warp core.md\""),
            "{page}"
        );
        assert!(page.contains("```yaml\ntags: [engineering]\n```"), "{page}");
        assert!(page.ends_with("Antimatter at 98%.\n"), "{page}");
        assert_eq!(md.rows.len(), 1);
        let row = &md.rows[0];
        assert!(row.is_document);
        assert_eq!(row.uuid, md.markdown_uuid);
        assert_eq!(
            row.source_url.as_deref(),
            Some("file:///vault/Bridge/Warp%20core.md")
        );
        assert_eq!(md.render_unit_key.as_deref(), Some("Bridge/Warp core.md"));
    }

    /// An edit moves the mtime; the note's id must not move with it.
    #[test]
    fn a_note_is_its_path_whatever_it_says_or_when() {
        let a = render_note("notes", &note("log.md", "one"), None);
        let mut edited = note("log.md", "two");
        edited.modified_at = Some("2370-01-01T00:00:00Z".into());
        let b = render_note("notes", &edited, None);
        assert_eq!(a.markdown_uuid, b.markdown_uuid);
        assert_ne!(
            a.markdown_uuid,
            render_note("notes", &note("Log.md", "one"), None).markdown_uuid
        );
    }

    /// A tag the note leaves open is closed at the note's end, and the
    /// file name is text, not markup.
    #[test]
    fn markup_a_note_leaves_open_is_closed_and_its_name_is_text() {
        let md = render_note("notes", &note("<b>x & co.md", "```\nunclosed"), None);
        let page = join(&md.sections);
        assert!(page.contains("&lt;b&gt;x &amp; co"), "{page}");
        assert!(page.trim_end().ends_with("```"), "{page}");
    }
}
