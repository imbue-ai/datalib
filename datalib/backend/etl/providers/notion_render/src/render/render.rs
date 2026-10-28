//! Emit one document per Notion page and one per comment thread.
//!
//! Notion returns a page body already rendered as its enhanced markdown,
//! and the download step stores it whole. This resolves each attachment
//! slot to its address in the source's blob store, turns Notion's
//! dialect into the CommonMark the viewer reads (`enhanced.rs`), and
//! writes it out under the front matter and title every document has.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::blob_cas::BlobBundle;
use datalib_etl::layout::render_markdown_root;
use datalib_etl::progress::Progress;
use datalib_etl_render::front_matter::yaml_scalar;
use datalib_etl_render::grid_index::{doc_md_path, RenderedMarkdown};
use datalib_etl_render::html::{escape_md_block, escape_md_inline};
use datalib_etl_render::inputs::{Bucket, Buckets, Lookup};
use datalib_etl_render::message::{chip_link, stamp_text_html};
use datalib_etl_render::title::Title;
use datalib_handle::Handle;
use datalib_schema::grid_rows::GridRow;
use datalib_schema::providers::Provider;
use serde_json::Value;

use super::enhanced;
use super::grid_rows::{comment_author_handle, gather_documents, PageDocument, ThreadDocument};
use super::parse::{ParsedNotion, ATTACHMENTS_PROJECTION_SQL};
use datalib_etl_notion::ingest::db::{RawDb, TreeRow};
use datalib_etl_notion::ingest::schema_raw::TreeKind;
use datalib_etl_render::pages::batches;

/// v4: ids are minted through `datalib_id` instead of passing Notion's
///     through, every row carries its backpointer, and a page's or
///     comment's id carries its `created_time` in its leading bits
///     (`datalib_id`'s v8 layout). Every uuid moved; `notion_page_uuid`
///     now holds the page's datalib id.
/// v8: an attachment links to its address in the blob store rather than
///     a copy beside the page.
/// v9: a comment's time is a `<time>` stamp.
/// v10: a comment's author is a chip, by their address where Notion gave
///      one, and a row carries its author's handle.
/// v11: a page opens with the path of pages and databases it sits under.
/// v12: a page body is Notion's enhanced markdown turned into CommonMark,
///      under the shared title line, with its ids in the front matter.
pub const RENDER_VERSION: u32 = 12;
pub fn notion_url(page_id: &str) -> String {
    format!("https://www.notion.so/{}", page_id.replace('-', ""))
}

pub fn notion_thread_url(
    page_id: &str,
    discussion_id: Option<&str>,
    anchor_block_id: Option<&str>,
) -> String {
    let pg = page_id.replace('-', "");
    let mut url = format!("https://www.notion.so/{pg}");
    match (discussion_id, anchor_block_id) {
        (Some(d), Some(a)) if !d.is_empty() => {
            url.push_str(&format!("?d={}", d.replace('-', "")));
            url.push_str(&format!("#{}", a.replace('-', "")));
        }
        (Some(d), None) if !d.is_empty() => {
            url.push_str(&format!("?d={}", d.replace('-', "")));
        }
        (_, Some(a)) if !a.is_empty() => {
            url.push_str(&format!("#{}", a.replace('-', "")));
        }
        _ => {}
    }
    url
}

/// Rewrite attachment slots to where the bundle says the bytes are.
///
/// The stored markdown references each attachment by slot — the
/// unsigned upstream URL — because that is the only identifier that
/// survives Notion re-signing the link on every fetch. A slot with no
/// bytes in the CAS is left as-is: an upstream link still beats a
/// broken one.
fn localize_attachments(markdown: &str, bundle: &BlobBundle) -> String {
    let mut out = markdown.to_string();
    let slots: Vec<String> = bundle
        .iter()
        .map(|(ref_id, _)| ref_id.to_string())
        .collect();
    for slot in slots {
        if let Some(href) = bundle.href_for(&slot) {
            out = out.replace(&slot, &href);
        }
    }
    out
}

/// The front matter and the title line: the page's ids, its times, and
/// the link back to Notion.
fn head(title: &str, markdown_uuid: &str, page_id: &str, page: &Value) -> String {
    let mut fm = String::from("---\n");
    fm.push_str(&format!("title: {}\n", yaml_scalar(title)));
    fm.push_str(&format!("provider: {}\n", Provider::Notion));
    fm.push_str(&format!("markdown_uuid: {markdown_uuid}\n"));
    fm.push_str(&format!("page_id: {}\n", yaml_scalar(page_id)));
    for (k, v) in [
        ("created_at", "created_time"),
        ("last_edited_at", "last_edited_time"),
    ] {
        if let Some(t) = page.get(v).and_then(|x| x.as_str()) {
            fm.push_str(&format!("{k}: {}\n", yaml_scalar(t)));
        }
    }
    if let Some(u) = page.get("url").and_then(|x| x.as_str()) {
        fm.push_str(&format!("source_url: {}\n", yaml_scalar(u)));
    }
    fm.push_str("source_label: Notion\n");
    fm.push_str("---\n\n");
    let url = page.get("url").and_then(|x| x.as_str());
    fm.push_str(
        &Title {
            text: title,
            markdown_uuid: Some(markdown_uuid),
            suffix: None,
            source_url: url,
        }
        .render(),
    );
    fm
}

/// The line above a page naming where it sits, top first: each page,
/// database and titled block above it, up to the top of what the store
/// knows. A page this source renders links its document; anything else
/// links Notion. A column or a toggle is walked through and not shown.
/// Empty for a page at the top of the workspace.
fn breadcrumb(page_id: &str, tree: Lookup<'_, HashMap<String, TreeRow>>, stanza: &str) -> String {
    let mut crumbs: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut next = tree.get(page_id).and_then(parent_of);
    while let Some(id) = next {
        if !seen.insert(id.clone()) {
            break;
        }
        let Some(row) = tree.get(&id) else {
            break;
        };
        let title = || escape_md_inline(row.title.as_deref().unwrap_or("Untitled"));
        match (row.kind, &row.title) {
            (Some(TreeKind::Page), _) => {
                let uuid = super::ids::page(stanza, &id, row.created_time.as_deref()).uuid;
                crumbs.push(format!("[{}](/chat/{uuid})", title()));
            }
            (_, Some(_)) => crumbs.push(format!("[{}]({})", title(), notion_url(&id))),
            (_, None) => {}
        }
        next = parent_of(row);
    }
    if crumbs.is_empty() {
        return String::new();
    }
    crumbs.reverse();
    format!("{}\n\n", crumbs.join(" › "))
}

fn parent_of(row: &TreeRow) -> Option<String> {
    match row.parent_type.as_deref() {
        Some("workspace") | None => None,
        Some(_) => row.parent_id.clone(),
    }
}

/// Page properties, for a page whose body is empty.
///
/// A database row's content usually *is* its properties — 71% of pages
/// in a measured workspace have no body at all — so emitting nothing
/// for them would drop most of a workspace on the floor.
fn properties_table(page: &Value) -> String {
    let Some(props) = page.get("properties").and_then(|v| v.as_object()) else {
        return String::new();
    };
    let mut rows: Vec<(String, String)> = Vec::new();
    for (name, prop) in props {
        let rendered = render_property(prop);
        if !rendered.is_empty() {
            rows.push((name.clone(), rendered));
        }
    }
    if rows.is_empty() {
        return String::new();
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    let mut out = String::from("| Property | Value |\n| --- | --- |\n");
    for (k, v) in rows {
        out.push_str(&format!("| {} | {} |\n", table_cell(&k), table_cell(&v)));
    }
    out
}

/// A property's name or plain value as one table cell, its line breaks
/// drawn as `<br>` since a real one would end the row.
fn table_cell(value: &str) -> String {
    value
        .lines()
        .map(escape_md_inline)
        .collect::<Vec<_>>()
        .join("<br>")
}

fn plain(rt: Option<&Value>) -> String {
    rt.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.get("plain_text").and_then(|v| v.as_str()))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

fn render_property(prop: &Value) -> String {
    match prop.get("type").and_then(|v| v.as_str()).unwrap_or("") {
        "title" => plain(prop.get("title")),
        "rich_text" => plain(prop.get("rich_text")),
        "url" => prop
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        "email" => prop
            .get("email")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        "phone_number" => prop
            .get("phone_number")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        "number" => prop
            .get("number")
            .and_then(|v| v.as_f64())
            .map(|n| n.to_string())
            .unwrap_or_default(),
        "checkbox" => match prop.get("checkbox").and_then(|v| v.as_bool()) {
            Some(true) => "yes".into(),
            Some(false) => "no".into(),
            None => String::new(),
        },
        "select" => prop
            .get("select")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        "status" => prop
            .get("status")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        "multi_select" => names_of(prop.get("multi_select")),
        "people" => names_of(prop.get("people")),
        "date" => {
            let d = prop.get("date");
            let start = d
                .and_then(|v| v.get("start"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let end = d
                .and_then(|v| v.get("end"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if end.is_empty() {
                start.into()
            } else {
                format!("{start} → {end}")
            }
        }
        "created_time" => prop
            .get("created_time")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        "last_edited_time" => prop
            .get("last_edited_time")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .into(),
        _ => String::new(),
    }
}

fn names_of(v: Option<&Value>) -> String {
    v.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.get("name").and_then(|n| n.as_str()))
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn thread_markdown(
    stanza: &str,
    page_title: &str,
    members: &[&Value],
    anchor: Option<&str>,
    author_of: &dyn Fn(&Value) -> Option<Handle>,
) -> String {
    let mut out = format!(
        "---\ntitle: {}\nsource_label: Notion\n---\n\n",
        yaml_scalar(page_title)
    );
    // A comment names the block it hangs off and carries no quote of
    // it, so without this the thread opens with no indication of what
    // it is about.
    if let Some(a) = anchor.filter(|a| !a.trim().is_empty()) {
        out.push_str(&format!(
            "> {}\n\n",
            escape_md_block(a).replace('\n', "\n> ")
        ));
    }
    if members
        .iter()
        .any(|c| c.get("original_content_deleted").and_then(|v| v.as_bool()) == Some(true))
    {
        out.push_str("*The commented-on content has been deleted upstream.*\n\n");
    }
    for c in members {
        let uuid = super::ids::comment(
            stanza,
            c.get("id").and_then(|v| v.as_str()).unwrap_or(""),
            c.get("created_time").and_then(|v| v.as_str()),
        )
        .uuid;
        let author = c
            .get("display_name")
            .and_then(|d| d.get("resolved_name"))
            .and_then(|v| v.as_str())
            .unwrap_or("(unknown)");
        let when = c.get("created_time").and_then(|v| v.as_str()).unwrap_or("");
        // The id + data-section-uuid pair is what the UI walks to anchor
        // per-message feedback; see AGENTS.md "QMDs are write-only".
        out.push_str(&format!(
            "<div id=\"m-{uuid}\" data-section-uuid=\"{uuid}\" class=\"msg msg--notion\">\n\n"
        ));
        let who = match author_of(c) {
            Some(h) => chip_link(author, &h),
            None => format!("**{}**", escape_md_inline(author)),
        };
        out.push_str(&format!("{who} · {}\n\n", stamp_text_html(when)));
        out.push_str(&escape_md_block(&plain(c.get("rich_text"))));
        out.push_str("\n\n</div>\n\n");
    }
    out
}

/// Write a document to the file its id names, and point each of its rows
/// at that file.
fn file_document(
    root: &Path,
    stanza: &str,
    markdown_uuid: &str,
    rows: &[GridRow],
    body: &str,
) -> Result<(PathBuf, Vec<GridRow>)> {
    let md_path = doc_md_path(&render_markdown_root(root, stanza), markdown_uuid);
    let dir = md_path.parent().expect("a document's file has a folder");
    fs::create_dir_all(dir).with_context(|| format!("mkdir -p {}", dir.display()))?;
    fs::write(&md_path, body).with_context(|| format!("write {}", md_path.display()))?;
    let md_rel = md_path
        .strip_prefix(root)
        .unwrap_or(&md_path)
        .to_string_lossy()
        .into_owned();
    let rows = rows
        .iter()
        .map(|r| GridRow {
            qmd_path: Some(md_rel.clone()),
            ..r.clone()
        })
        .collect();
    Ok((md_path, rows))
}

/// Where a pass reads its pages' attachment bytes: what each page's
/// weigh, and the bundles of a batch of pages.
pub trait PageBlobs {
    fn weights(&self, pages: &[&str]) -> Result<Vec<u64>>;
    fn load(&self, pages: &[&str]) -> Result<HashMap<String, BlobBundle>>;
}

/// Bundles already in hand.
impl PageBlobs for HashMap<String, BlobBundle> {
    fn weights(&self, pages: &[&str]) -> Result<Vec<u64>> {
        Ok(pages
            .iter()
            .map(|p| self.get(*p).map_or(0, |b| b.total_bytes()))
            .collect())
    }

    fn load(&self, pages: &[&str]) -> Result<HashMap<String, BlobBundle>> {
        Ok(pages
            .iter()
            .filter_map(|p| Some((p.to_string(), self.get(*p)?.clone())))
            .collect())
    }
}

/// The bytes behind `parsed.refs_by_page`, read from the raw store at the
/// commit the parse read and the blob store beside it, open until
/// [`StorePageBlobs::close`].
pub struct StorePageBlobs<'a> {
    db: RawDb,
    refs_by_page: &'a HashMap<String, Vec<String>>,
}

impl<'a> StorePageBlobs<'a> {
    /// `None` when the parse read no store, or no page has bytes.
    pub fn open(parsed: &'a ParsedNotion) -> Result<Option<Self>> {
        let (Some(db_path), Some(head)) = (&parsed.db_path, &parsed.head) else {
            return Ok(None);
        };
        if parsed.refs_by_page.is_empty() {
            return Ok(None);
        }
        let Some(db) = block_on(RawDb::open_reader(db_path, Some(head)))? else {
            return Ok(None);
        };
        Ok(Some(Self {
            db,
            refs_by_page: &parsed.refs_by_page,
        }))
    }

    fn refs<'p>(&self, pages: &[&'p str]) -> Vec<(&'p str, Vec<String>)> {
        pages
            .iter()
            .map(|p| (*p, self.refs_by_page.get(*p).cloned().unwrap_or_default()))
            .collect()
    }

    pub fn close(self) {
        block_on(self.db.close());
    }
}

impl PageBlobs for StorePageBlobs<'_> {
    fn weights(&self, pages: &[&str]) -> Result<Vec<u64>> {
        let sizes = block_on(BlobBundle::sizes_many(
            self.db.pool(),
            Some(self.db.cas()),
            ATTACHMENTS_PROJECTION_SQL,
            self.refs(pages),
        ))?;
        Ok(pages
            .iter()
            .map(|p| sizes.get(p).copied().unwrap_or(0))
            .collect())
    }

    fn load(&self, pages: &[&str]) -> Result<HashMap<String, BlobBundle>> {
        let bundles = block_on(BlobBundle::load_many(
            self.db.pool(),
            Some(self.db.cas()),
            ATTACHMENTS_PROJECTION_SQL,
            self.refs(pages),
        ))
        .context("load attachments")?;
        Ok(bundles
            .into_iter()
            .map(|(page, bundle)| (page.to_string(), bundle))
            .collect())
    }
}

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

/// [`render_notion`] with each page's bytes read from the store the parse
/// read, a batch of at most `blob_bytes` at a time.
pub fn render_from_store(
    parsed: &ParsedNotion,
    root: &Path,
    stanza: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
    blob_bytes: u64,
) -> Result<RenderSummary> {
    match StorePageBlobs::open(parsed)? {
        Some(blobs) => {
            let rendered = render_notion(
                parsed,
                root,
                stanza,
                progress,
                on_doc_complete,
                &blobs,
                blob_bytes,
            );
            blobs.close();
            rendered
        }
        None => render_notion(
            parsed,
            root,
            stanza,
            progress,
            on_doc_complete,
            &HashMap::new(),
            blob_bytes,
        ),
    }
}

pub fn render_notion(
    parsed: &ParsedNotion,
    root: &Path,
    stanza: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
    blobs: &dyn PageBlobs,
    blob_bytes: u64,
) -> Result<RenderSummary> {
    let mut summary = RenderSummary::default();
    fs::create_dir_all(render_markdown_root(root, stanza))?;
    if parsed.pages.is_empty() && parsed.comments.is_empty() {
        return Ok(summary);
    }

    let docs = gather_documents(parsed, stanza)?;
    let pages_by_id: HashMap<&str, &Value> = parsed
        .pages
        .iter()
        .filter_map(|p| p.get("id").and_then(|v| v.as_str()).map(|id| (id, p)))
        .collect();
    progress.set_length(Some((docs.pages.len() + docs.threads.len()) as u64));

    let empty_bundle = BlobBundle::default();
    let page_ids: Vec<&str> = docs.pages.iter().map(|d| d.page_uuid.as_str()).collect();
    let weights = blobs.weights(&page_ids)?;
    for range in batches(&weights, blob_bytes) {
        let bundles = blobs.load(&page_ids[range.clone()])?;
        for doc in &docs.pages[range] {
            let PageDocument {
                page_uuid,
                markdown_uuid,
                page_title,
                inputs,
                ..
            } = doc;
            let Some(page) = pages_by_id.get(page_uuid.as_str()) else {
                continue;
            };
            if let Some(ids) = parsed.attachment_ids_by_page.get(page_uuid) {
                inputs.read_all("notion_attachments", ids.iter().map(String::as_str));
            }
            let bundle = bundles.get(page_uuid).unwrap_or(&empty_bundle);
            let cx = enhanced::Context {
                stanza,
                tree: inputs.lookup("tree", &parsed.tree),
                users: inputs.lookup("users", &parsed.user_names),
                emails: inputs.lookup("users", &parsed.user_emails),
            };
            let body = inputs
                .lookup("page_markdown", &parsed.markdown_by_page)
                .get(page_uuid)
                .map(|m| enhanced::to_markdown(&localize_attachments(m, bundle), &cx))
                .unwrap_or_default();
            let body = if body.trim().is_empty() {
                properties_table(page)
            } else {
                body
            };
            let mut out = head(page_title, markdown_uuid, page_uuid, page);
            out.push_str(&breadcrumb(
                page_uuid,
                inputs.lookup("tree", &parsed.tree),
                stanza,
            ));
            out.push_str(&body);
            let (md_path, rows) = file_document(root, stanza, markdown_uuid, &doc.rows, &out)?;

            on_doc_complete(RenderedMarkdown {
                markdown_uuid: markdown_uuid.clone(),
                source_id: String::new(),
                upstream_cursor: None,
                bucket_key: Some(page_uuid.clone()),
                md_path,
                render_version: RENDER_VERSION,
                rows,
                sections: Vec::new(),
                edges: Vec::new(),
                contacts: Vec::new(),
                problems: doc.problems.clone(),
            })?;
            summary.buckets.push(Bucket {
                key: page_uuid.clone(),
                inputs: inputs.declared(),
            });
            summary.rendered += 1;
            progress.inc(1);
        }
    }

    let mut by_disc: BTreeMap<&str, Vec<&Value>> = BTreeMap::new();
    for c in &parsed.comments {
        if let Some(d) = c.get("discussion_id").and_then(|v| v.as_str()) {
            if !d.is_empty() {
                by_disc.entry(d).or_default().push(c);
            }
        }
    }
    for doc in &docs.threads {
        let ThreadDocument {
            discussion_uuid,
            markdown_uuid,
            page_title,
            inputs,
            ..
        } = doc;
        let Some(members) = by_disc.get(discussion_uuid.as_str()) else {
            continue;
        };
        let anchors = inputs.lookup("comment_anchors", &parsed.anchor_text);
        let anchor = doc
            .anchor_block_uuid
            .as_deref()
            .and_then(|b| anchors.get(b))
            .map(String::as_str);
        let emails = inputs.lookup("users", &parsed.user_emails);
        let author_of = |c: &Value| comment_author_handle(c, emails);
        let body = thread_markdown(stanza, page_title, members, anchor, &author_of);
        let (md_path, rows) = file_document(root, stanza, markdown_uuid, &doc.rows, &body)?;
        on_doc_complete(RenderedMarkdown {
            markdown_uuid: markdown_uuid.clone(),
            source_id: String::new(),
            upstream_cursor: None,
            bucket_key: Some(discussion_uuid.clone()),
            md_path,
            render_version: RENDER_VERSION,
            rows,
            sections: Vec::new(),
            edges: Vec::new(),
            contacts: Vec::new(),
            problems: doc.problems.clone(),
        })?;
        summary.buckets.push(Bucket {
            key: discussion_uuid.clone(),
            inputs: inputs.declared(),
        });
        summary.rendered += 1;
        progress.inc(1);
    }
    Ok(summary)
}

#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub rendered: usize,
    /// Every bucket rendered, with what it read — page ids and
    /// discussion ids, for the caller to declare.
    pub buckets: Buckets,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    /// A page's attachments are read from the store its parse read, a
    /// batch at a time, and the page links each where the blob store
    /// holds it, writing no copy. Every other render test hands in
    /// bundles already in memory; this is the one that reads a store.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stored_attachment_is_read_from_the_store_and_linked_there() {
        use datalib_etl_notion::ingest::db::{PageMarkdownUpsert, PageUpsert};
        const SLOT: &str =
            "https://prod-files-secure.s3.us-west-2.amazonaws.com/ws/9c/viewscreen.png";
        let dir = tempdir().unwrap();
        let db_file = dir.path().join("notion-api.doltlite_db");
        let db = RawDb::open(&db_file).await.unwrap();
        db.upsert_pages(&[PageUpsert {
            id: "p1".into(),
            payload: json!({"id": "p1", "object": "page",
                "properties": {"title": {"type": "title", "title": [{"plain_text": "Bridge"}]}}})
            .to_string(),
            ..Default::default()
        }])
        .await
        .unwrap();
        // Stored as the fetcher stores it: the body with its link already
        // the unsigned slot, and the slot listed as an attachment.
        let mut tx = db.pool().begin().await.unwrap();
        db.store_body(
            &mut tx,
            &PageMarkdownUpsert {
                id: "p1".into(),
                markdown: format!("![main viewer]({SLOT})\n"),
                ..Default::default()
            },
            &[SLOT.to_string()],
            true,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let edges = db.load_attachments().await.unwrap();
        assert_eq!(edges.len(), 1, "the body names one attachment");
        let blake3 = db
            .cas()
            .put(b"\x89PNG\r\n\x1a\nviewscreen", Some("image/png"))
            .await
            .unwrap();
        let mut tx = db.pool().begin().await.unwrap();
        db.store_blob(&mut tx, &edges[0].id, &blake3).await.unwrap();
        tx.commit().await.unwrap();
        datalib_etl::doltlite_raw::commit_run(db.pool(), "test fixture")
            .await
            .unwrap();
        db.close().await;

        let parsed = super::super::parse::parse_api_dir(
            &db_file,
            datalib_etl_render::inputs::RawRange::cold(),
        )
        .unwrap();
        let out = tempdir().unwrap();
        let mut pages = Vec::new();
        render_from_store(
            &parsed,
            out.path(),
            "notion",
            &Progress::noop(),
            &mut |md: RenderedMarkdown| {
                if md.rows.iter().any(|r| r.kind == "Notion Page") {
                    pages.push(md.md_path);
                }
                Ok(())
            },
            1,
        )
        .unwrap();
        let page = pages.first().expect("the page rendered");
        let md = fs::read_to_string(page).unwrap();
        assert!(
            md.contains("](/data/") && md.contains(&format!("/blob/{blake3})")),
            "linked in the blob store: {md}"
        );
        assert!(!md.contains(SLOT), "not at the upstream slot: {md}");
        assert!(
            !page.parent().unwrap().join("blobs").exists(),
            "no copy beside the page"
        );
    }

    fn node(kind: TreeKind, title: Option<&str>, parent: (&str, &str)) -> TreeRow {
        TreeRow {
            kind: Some(kind),
            title: title.map(String::from),
            parent_type: Some(parent.0.into()),
            parent_id: Some(parent.1.into()),
            created_time: None,
        }
    }

    /// A row's path climbs its database to the page above, through a
    /// column that shows nothing, and declares every `tree` row it read,
    /// so a rename anywhere above re-renders the row.
    #[test]
    fn the_path_above_a_row_names_its_database_and_pages() {
        let tree: HashMap<String, TreeRow> = [
            (
                "row",
                node(TreeKind::Page, Some("Picard"), ("data_source_id", "db")),
            ),
            (
                "db",
                node(
                    TreeKind::Database,
                    Some("Crew [manifest]"),
                    ("block_id", "col"),
                ),
            ),
            ("col", node(TreeKind::Block, None, ("page_id", "bridge"))),
            (
                "bridge",
                node(TreeKind::Page, Some("Bridge"), ("workspace", "workspace")),
            ),
        ]
        .into_iter()
        .map(|(id, row)| (id.to_string(), row))
        .collect();
        let inputs = datalib_etl_render::inputs::Inputs::default();
        let line = breadcrumb("row", inputs.lookup("tree", &tree), "notion");
        let bridge = super::super::ids::page("notion", "bridge", None).uuid;
        assert_eq!(
            line,
            format!(
                "[Bridge](/chat/{bridge}) › [Crew \\[manifest\\]](https://www.notion.so/db)\n\n"
            )
        );
        let read: Vec<String> = inputs.declared().into_iter().map(|i| i.id).collect();
        assert_eq!(read, ["bridge", "col", "db", "row"]);
    }

    /// A page above a root that roots mode placed but does not mirror
    /// has no document here, so its crumb links Notion.
    #[test]
    fn a_page_this_source_does_not_mirror_links_notion() {
        let tree: HashMap<String, TreeRow> = [
            (
                "debrief",
                node(TreeKind::Page, Some("Debrief"), ("page_id", "wiki")),
            ),
            (
                "wiki",
                node(
                    TreeKind::UnmirroredPage,
                    Some("Wiki"),
                    ("workspace", "workspace"),
                ),
            ),
        ]
        .into_iter()
        .map(|(id, row)| (id.to_string(), row))
        .collect();
        let inputs = datalib_etl_render::inputs::Inputs::default();
        assert_eq!(
            breadcrumb("debrief", inputs.lookup("tree", &tree), "notion"),
            "[Wiki](https://www.notion.so/wiki)\n\n"
        );
    }

    /// A page at the top has no path, and one whose parent this
    /// credential cannot see stops there rather than at a blank crumb.
    #[test]
    fn the_path_stops_at_the_top_of_what_is_known() {
        let tree: HashMap<String, TreeRow> = [
            (
                "top",
                node(TreeKind::Page, Some("Bridge"), ("workspace", "workspace")),
            ),
            (
                "shared",
                node(TreeKind::Page, Some("Away team"), ("page_id", "hidden")),
            ),
            (
                "hidden",
                TreeRow {
                    kind: Some(TreeKind::Block),
                    title: None,
                    parent_type: None,
                    parent_id: None,
                    created_time: None,
                },
            ),
        ]
        .into_iter()
        .map(|(id, row)| (id.to_string(), row))
        .collect();
        let inputs = datalib_etl_render::inputs::Inputs::default();
        assert_eq!(
            breadcrumb("top", inputs.lookup("tree", &tree), "notion"),
            ""
        );
        assert_eq!(
            breadcrumb("shared", inputs.lookup("tree", &tree), "notion"),
            ""
        );
    }

    /// A database row's properties are its content. Rendering nothing
    /// for a body-less page would drop most of a real workspace.
    #[test]
    fn a_body_less_page_renders_its_properties() {
        let page = json!({"id":"p1","properties":{
            "Name":{"type":"title","title":[{"plain_text":"Standup Notes"}]},
            "Status":{"type":"select","select":{"name":"Active"}},
            "Empty":{"type":"rich_text","rich_text":[]}
        }});
        let t = properties_table(&page);
        assert!(t.contains("| Name | Standup Notes |"), "{t}");
        assert!(t.contains("| Status | Active |"), "{t}");
        assert!(!t.contains("Empty"), "empty properties are omitted: {t}");
    }

    /// A page and a thread are each written to the file their id names,
    /// and every row names that file. The row and the writer once named a
    /// thread's file apart, and a user saw a 404 preview.
    #[test]
    fn each_document_lands_in_the_file_its_id_and_its_rows_name() {
        let d = tempdir().unwrap();
        let page = "b1d6e000-1701-4d00-8000-000000000001";
        let disc = "d0000001-1701-4d00-8000-000000000001";
        let parsed = ParsedNotion {
            pages: vec![json!({
                "id": page,
                "object": "page",
                "properties": {"title": {"type": "title", "title": [{"plain_text": "Handbook"}]}},
            })],
            comments: vec![json!({
                "id": "c1",
                "page_id": page,
                "discussion_id": disc,
                "created_time": "2369-04-15T01:00:00.000Z",
                "display_name": {"resolved_name": "Data"},
                "rich_text": [{"plain_text": "hi"}],
                "parent": {"type": "block_id", "block_id": "b1"},
            })],
            ..Default::default()
        };
        let mut emitted: Vec<RenderedMarkdown> = Vec::new();
        let mut on_doc = |md: RenderedMarkdown| {
            emitted.push(md);
            Ok(())
        };
        render_notion(
            &parsed,
            d.path(),
            "notion",
            &Progress::noop(),
            &mut on_doc,
            &HashMap::new(),
            1,
        )
        .unwrap();
        assert_eq!(emitted.len(), 2, "a page and a thread");
        for md in &emitted {
            let want = doc_md_path(&render_markdown_root(d.path(), "notion"), &md.markdown_uuid);
            assert_eq!(md.md_path, want);
            assert!(want.exists(), "{} was not written", want.display());
            let rel = want.strip_prefix(d.path()).unwrap().to_string_lossy();
            for r in &md.rows {
                assert_eq!(r.qmd_path.as_deref(), Some(&*rel), "row {}", r.uuid);
            }
        }
    }

    /// The quoted block leads the thread file, so a reader sees what
    /// the discussion is about before the first reply.
    #[test]
    fn a_thread_file_opens_with_the_quoted_block() {
        let c = json!({"id": "c1", "created_time": "2369-04-15T01:00:00.000Z",
                       "display_name": {"resolved_name": "Data"},
                       "rich_text": [{"plain_text": "Recommend recalibration"}]});
        let md = thread_markdown(
            "notion",
            "Handbook",
            &[&c],
            Some("Warp core alignment"),
            &|_| Handle::email("data@enterprise.test"),
        );
        assert!(md.contains("> Warp core alignment"), "{md}");
        assert!(md.find("> Warp core alignment") < md.find("Recommend recalibration"));
        assert!(
            md.contains(
                "[Data](mailto:data@enterprise.test \"Data <data@enterprise.test>\") · <time"
            ),
            "an author with an address is a chip: {md}"
        );
    }

    /// A comment, its author, the block it hangs off and a property are
    /// plain text in Notion's API (our `plain` drops the annotations), so
    /// none of it may open a tag. The page body, Notion's own markdown,
    /// is left as Notion wrote it.
    #[test]
    fn a_thread_and_properties_in_markup_render_escaped() {
        const MARKUP: &str = "<script>x</script> & co";
        let escaped = "&lt;script&gt;x&lt;/script&gt; &amp; co";
        let c = json!({"id": "c1", "created_time": "2369-04-15T01:00:00.000Z",
                       "display_name": {"resolved_name": MARKUP},
                       "rich_text": [{"plain_text": MARKUP}]});
        let md = thread_markdown("notion", "Handbook", &[&c], Some(MARKUP), &|_| None);
        let (_, body) = md
            .split_once("---\n\n")
            .expect("front matter, then the body");
        assert!(!body.contains("<script>"), "{md}");
        assert!(body.starts_with(&format!("> {escaped}\n")), "{md}");
        assert!(body.contains(&format!("**{escaped}** · ")), "{md}");
        assert!(body.contains(&format!("\n{escaped}\n")), "{md}");

        let page = json!({"id":"p1","properties":{
            MARKUP: {"type":"rich_text","rich_text":[{"plain_text": "a | b\n<b>c</b>"}]}
        }});
        let t = properties_table(&page);
        assert!(
            t.contains(&format!("| {escaped} | a \\| b<br>&lt;b&gt;c&lt;/b&gt; |")),
            "{t}"
        );
    }

    /// `original_content_deleted` is a real upstream signal: the thing
    /// the comment was about is gone. Say so rather than rendering a
    /// thread that appears to be about nothing.
    #[test]
    fn a_thread_whose_anchor_was_deleted_says_so() {
        let c = json!({"id": "c1", "created_time": "2369-04-15T01:00:00.000Z",
                       "original_content_deleted": true,
                       "display_name": {"resolved_name": "Data"},
                       "rich_text": [{"plain_text": "hi"}]});
        let md = thread_markdown("notion", "Handbook", &[&c], None, &|_| None);
        assert!(md.contains("deleted upstream"), "{md}");
    }
}
