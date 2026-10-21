//! One document per Google Doc: its newest capture as markdown, each
//! comment thread folded in after the text it was made on, and a grid
//! row for the Doc and for every comment and reply.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::blob_cas::BlobBundle;
use datalib_etl_render::front_matter::yaml_scalar;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::Inputs;
use datalib_etl_render::section::{join, Section};
use datalib_etl_render::title::Title;
use datalib_handle::Handle;
use datalib_schema::grid_rows::GridRow;
use datalib_schema::problems::{Outcome, Problem, ProblemRow, Reason, Scope, Stage};
use datalib_schema::providers::Provider;
use serde_json::Value;

use crate::comments::{self, Thread};
use crate::doc::{self, Block, BlockKind};
use crate::ids;
use crate::store::Capture;
use crate::unsupported;

/// v1: Docs only, with their comment threads.
pub const RENDER_VERSION: u32 = 1;

const SOURCE_LABEL: &str = "Google Drive";
const KIND_DOC: &str = "Google Doc";
const KIND_COMMENT: &str = "Google Doc Comment";

/// Everything one Doc renders from.
pub struct DocInput<'a> {
    pub file_id: &'a str,
    pub entry: &'a Value,
    pub capture: &'a Capture,
    pub native: &'a Value,
    pub images: &'a BlobBundle,
    pub comments: &'a [(String, Value)],
}

pub fn doc_dir(root: &Path, source_id: &str, file_id: &str) -> PathBuf {
    datalib_etl::layout::render_markdown_root(root, source_id)
        .join("docs")
        .join(file_id)
}

/// Write one Doc's `.md` and its images, and return what goes in the
/// store. `inputs` gets every raw row it read.
pub fn render_doc(
    root: &Path,
    source_id: &str,
    input: &DocInput<'_>,
    inputs: &Inputs,
) -> Result<RenderedMarkdown> {
    let DocInput {
        file_id,
        entry,
        capture,
        native,
        images,
        comments: comment_rows,
    } = *input;
    inputs.read("files", file_id);
    inputs.read("captures", &capture.id);
    inputs.read_all(
        "capture_parts",
        capture.parts.iter().map(|(id, _, _)| id.as_str()),
    );
    inputs.read_all("comments", comment_rows.iter().map(|(id, _)| id.as_str()));

    let dir = doc_dir(root, source_id, file_id);
    fs::create_dir_all(&dir).with_context(|| format!("mkdir -p {}", dir.display()))?;
    if !images.is_empty() {
        images.materialize_to_dir(&dir.join("blobs"))?;
    }
    let image_paths: HashMap<String, String> = capture
        .parts
        .iter()
        .filter_map(|(id, part, _)| {
            let pointer = part.strip_prefix("image:")?;
            let name = images.filename_for(id)?;
            Some((pointer.to_string(), format!("blobs/{name}")))
        })
        .collect();

    let file = FileFacts::of(source_id, file_id, entry, native);
    let body = doc::convert(native, &image_paths);
    let threads = comments::threads(source_id, comment_rows, file.me.as_ref());
    let placed = comments::place(&threads, &body.blocks);

    let mut sections = vec![Section::unkeyed(file.head())];
    sections.extend(interleave(&body.blocks, &threads, &placed));
    let md_path = dir.join("index.md");
    fs::write(&md_path, join(&sections)).with_context(|| format!("write {}", md_path.display()))?;
    let md_rel = md_path
        .strip_prefix(root)
        .unwrap_or(&md_path)
        .to_string_lossy()
        .into_owned();

    let mut problems: Vec<ProblemRow> = Vec::new();
    let mut rows: Vec<GridRow> = Vec::new();
    let text = body
        .blocks
        .iter()
        .map(|b| b.text.as_str())
        .filter(|t| !t.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    rows.extend(file.row(&md_rel, &text, &mut problems));
    for t in &threads {
        rows.extend(comment_rows_of(&file, t, &md_rel, &mut problems));
    }

    let problem_row = |item: Option<&str>, outcome, problem| {
        ProblemRow::new(
            source_id,
            Stage::Render,
            Scope::Markdown(&file.id.uuid),
            item,
            outcome,
            problem,
            Some(RENDER_VERSION),
        )
    };
    for p in unsupported::problems(&body.unsupported) {
        problems.push(problem_row(Some(&file.id.uuid), Outcome::Ok, p));
    }
    for pointer in &body.images_missing {
        let p = Problem::field("image", Reason::BlobMissing, pointer).at(pointer.clone());
        problems.push(problem_row(Some(&file.id.uuid), Outcome::Nulled, p));
    }

    Ok(RenderedMarkdown {
        markdown_uuid: file.id.uuid.clone(),
        source_id: source_id.to_string(),
        upstream_cursor: None,
        bucket_key: Some(file_id.to_string()),
        md_path,
        render_version: RENDER_VERSION,
        rows,
        sections,
        edges: Vec::new(),
        contacts: Vec::new(),
        problems,
    })
}

/// The body with each thread after the block it was made on. A thread
/// on a list item waits for the end of its list, so the list stays one
/// list; threads placed nowhere go at the end, under a heading of their
/// own.
fn interleave(blocks: &[Block], threads: &[Thread], placed: &[Option<usize>]) -> Vec<Section> {
    let mut after: BTreeMap<usize, Vec<&Thread>> = BTreeMap::new();
    let mut unplaced: Vec<&Thread> = Vec::new();
    for (t, at) in threads.iter().zip(placed) {
        match at {
            Some(i) => after.entry(*i).or_default().push(t),
            None => unplaced.push(t),
        }
    }
    let mut out = Vec::new();
    let mut pending: Vec<&Thread> = Vec::new();
    let mut start = 0;
    for i in 0..blocks.len() {
        pending.extend(after.remove(&i).unwrap_or_default());
        let list_goes_on = blocks[i].kind == BlockKind::ListItem
            && blocks
                .get(i + 1)
                .is_some_and(|b| b.kind == BlockKind::ListItem);
        if pending.is_empty() || list_goes_on {
            continue;
        }
        out.push(Section::unkeyed(format!(
            "{}\n\n",
            doc::join_blocks(&blocks[start..=i])
        )));
        for t in pending.drain(..) {
            out.extend(comments::sections(t));
        }
        start = i + 1;
    }
    if start < blocks.len() {
        out.push(Section::unkeyed(format!(
            "{}\n",
            doc::join_blocks(&blocks[start..])
        )));
    }
    if !unplaced.is_empty() {
        out.push(Section::unkeyed(
            "\n## Comments not tied to the text\n\n".to_string(),
        ));
        for t in unplaced {
            out.extend(comments::sections(t));
        }
    }
    out
}

/// What the rows and the head of one Doc share.
struct FileFacts {
    source_id: String,
    file_id: String,
    id: datalib_id::Identity,
    title: String,
    url: Option<String>,
    created: Option<String>,
    modified: Option<String>,
    owner: Option<(String, Option<Handle>)>,
    /// The account, where this file names it: an owner, the last editor
    /// or whoever shared it, marked `me`.
    me: Option<Handle>,
}

impl FileFacts {
    fn of(source_id: &str, file_id: &str, entry: &Value, native: &Value) -> Self {
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let users = || {
            entry
                .get("owners")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .chain(entry.get("lastModifyingUser"))
                .chain(entry.get("sharingUser"))
        };
        let me = users()
            .filter(|u| u.get("me").and_then(Value::as_bool) == Some(true))
            .find_map(|u| u.get("emailAddress").and_then(Value::as_str))
            .and_then(Handle::email);
        let owner = entry.get("owners").and_then(|o| o.get(0)).and_then(|o| {
            let name = s(o, "displayName")?;
            let handle = o
                .get("emailAddress")
                .and_then(Value::as_str)
                .and_then(Handle::email);
            Some((name, handle))
        });
        Self {
            source_id: source_id.to_string(),
            file_id: file_id.to_string(),
            id: ids::file(source_id, file_id),
            title: s(entry, "name")
                .or_else(|| s(native, "title"))
                .unwrap_or_else(|| "(untitled)".to_string()),
            url: s(entry, "webViewLink"),
            created: s(entry, "createdTime"),
            modified: s(entry, "modifiedTime"),
            owner,
            me,
        }
    }

    fn head(&self) -> String {
        let mut s = String::from("---\n");
        s.push_str(&format!("title: {}\n", yaml_scalar(&self.title)));
        s.push_str(&format!("provider: {}\n", Provider::GoogleDrive));
        s.push_str(&format!("source_label: {}\n", yaml_scalar(SOURCE_LABEL)));
        s.push_str(&format!("markdown_uuid: {}\n", self.id.uuid));
        s.push_str(&format!("file_id: {}\n", yaml_scalar(&self.file_id)));
        for (k, v) in [
            ("created_at", &self.created),
            ("modified_at", &self.modified),
            ("source_url", &self.url),
        ] {
            if let Some(v) = v {
                s.push_str(&format!("{k}: {}\n", yaml_scalar(v)));
            }
        }
        s.push_str("---\n\n");
        s.push_str(
            &Title {
                text: &self.title,
                suffix: None,
                markdown_uuid: Some(&self.id.uuid),
                source_url: self.url.as_deref(),
            }
            .render(),
        );
        s
    }

    fn account(&self) -> Option<String> {
        self.me.as_ref().map(|h| h.value().to_string())
    }

    fn row(&self, md_rel: &str, text: &str, problems: &mut Vec<ProblemRow>) -> Option<GridRow> {
        let (author, handle) = match &self.owner {
            Some((name, handle)) => (
                Some(name.clone()),
                handle.as_ref().map(|h| h.as_str().to_string()),
            ),
            None => (None, None),
        };
        GridRow::builder()
            .uuid(self.id.uuid.clone())
            .provider(Provider::GoogleDrive)
            .kind(KIND_DOC)
            .source_label(SOURCE_LABEL)
            .is_document(true)
            .item_count(Some(1))
            .created_at(self.created.clone())
            .modified_at(self.modified.clone())
            .author(author)
            .author_handle(handle)
            .account(self.account())
            .conversation_name(Some(self.title.clone()))
            .conversation_uuid(self.id.uuid.clone())
            .entire_chat(format!("/chat/{}", self.id.uuid))
            .body(format!("{}\n{text}", self.title))
            .qmd_path(Some(md_rel.to_string()))
            .source_url(self.url.clone())
            .upstream_id(Some(self.id.natural_key.clone()))
            .upstream_entity_kind(Some(self.id.entity_kind.to_string()))
            .markdown_uuid(Some(self.id.uuid.clone()))
            .build_or_record(&self.source_id, &self.file_id, RENDER_VERSION, problems)
    }
}

fn comment_rows_of(
    file: &FileFacts,
    t: &Thread,
    md_rel: &str,
    problems: &mut Vec<ProblemRow>,
) -> Vec<GridRow> {
    let url = file.url.as_deref().map(|u| {
        format!(
            "{u}{}disco={}",
            if u.contains('?') { "&" } else { "?" },
            t.comment_id
        )
    });
    t.messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            let mut body = m.text.clone();
            if let (0, Some(q)) = (i, &t.quote) {
                body = format!("{q}\n{body}");
            }
            GridRow::builder()
                .uuid(m.id.uuid.clone())
                .provider(Provider::GoogleDrive)
                .kind(KIND_COMMENT)
                .source_label(SOURCE_LABEL)
                .item_count(Some(1))
                .created_at(m.created.clone())
                .modified_at(m.modified.clone())
                .author(Some(m.author.clone()))
                .author_handle(m.handle.as_ref().map(|h| h.as_str().to_string()))
                .account(file.account())
                .conversation_name(Some(file.title.clone()))
                .conversation_uuid(file.id.uuid.clone())
                .message_index(Some(i as i64))
                .entire_chat(format!("/chat/{}", file.id.uuid))
                .body(body)
                .qmd_path(Some(md_rel.to_string()))
                .source_url(url.clone())
                .upstream_id(Some(m.id.natural_key.clone()))
                .upstream_entity_kind(Some(m.id.entity_kind.to_string()))
                .markdown_uuid(Some(file.id.uuid.clone()))
                .build_or_record(&file.source_id, &t.key, RENDER_VERSION, problems)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn blocks(spec: &[(&str, BlockKind)]) -> Vec<Block> {
        spec.iter()
            .map(|(t, kind)| Block {
                md: t.to_string(),
                text: t.to_string(),
                kind: *kind,
            })
            .collect()
    }

    fn thread(source: &str, key: &str, quote: &str) -> Thread {
        comments::threads(
            source,
            &[(
                key.to_string(),
                json!({"author": {"displayName": "Data"}, "content": "Noted.",
                       "quotedFileContent": {"value": quote}}),
            )],
            None,
        )
        .remove(0)
    }

    /// A thread on a list item waits for the list to end, so the list is
    /// not cut in two.
    #[test]
    fn a_thread_on_a_list_item_follows_the_whole_list() {
        let b = blocks(&[
            ("Away team:", BlockKind::Plain),
            ("- Riker", BlockKind::ListItem),
            ("- Worf", BlockKind::ListItem),
            ("Energize.", BlockKind::Plain),
        ]);
        let t = vec![thread("src", "f#1", "Riker")];
        let placed = comments::place(&t, &b);
        let md = join(&interleave(&b, &t, &placed));
        let list = md.find("- Riker\n- Worf\n\n<details").expect(&md);
        assert!(md[list..].contains("</details>\n\nEnergize.\n"), "{md}");
    }

    #[test]
    fn a_thread_placed_nowhere_goes_at_the_end() {
        let b = blocks(&[("Captain's log.", BlockKind::Plain)]);
        let t = vec![thread("src", "f#1", "not in the text")];
        let placed = comments::place(&t, &b);
        let md = join(&interleave(&b, &t, &placed));
        assert!(
            md.starts_with("Captain's log.\n\n## Comments not tied to the text\n\n<details"),
            "{md}"
        );
    }
}
