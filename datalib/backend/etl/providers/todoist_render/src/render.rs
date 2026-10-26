//! One document per Todoist project: its comments, then its sections in
//! order, each section's open tasks as a tree with every task's
//! comments after it, then the section's completed tasks, folded. A
//! grid row for the project, each task and each comment. TRANSLATE.md
//! says what becomes what.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl_render::front_matter::yaml_scalar;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::html::{
    escape_html_outside_code, escape_md_inline, escape_text, md_link_dest,
};
use datalib_etl_render::inputs::Inputs;
use datalib_etl_render::message::{chip_link, stamp_text_html, MessageHeader};
use datalib_etl_render::section::{join, msg_div_open_with, Section, MSG_DIV_CLOSE};
use datalib_etl_render::title::Title;
use datalib_handle::Handle;
use datalib_id::Identity;
use datalib_schema::grid_rows::GridRow;
use datalib_schema::problems::ProblemRow;
use datalib_schema::providers::Provider;
use serde_json::Value;

use crate::ids;
use crate::store::{flag, str_of, uid_of, Store};

/// v1: one document per project, completed tasks folded under each
///     section.
pub const RENDER_VERSION: u32 = 1;

const SOURCE_LABEL: &str = "Todoist";
const KIND_PROJECT: &str = "Todoist Project";
const KIND_TASK: &str = "Todoist Task";
const KIND_COMMENT: &str = "Todoist Comment";

pub fn project_url(id: &str) -> String {
    format!("https://app.todoist.com/app/project/{id}")
}

/// The form Todoist's API reference gives for a task's link.
pub fn task_url(id: &str) -> String {
    format!("https://app.todoist.com/app/task/{id}")
}

/// Where a project's `.md` lives, relative to the data root.
pub fn md_rel(source_id: &str, project_id: &str) -> PathBuf {
    let file: String = project_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    datalib_etl::layout::render_markdown_root(Path::new(""), source_id)
        .join("projects")
        .join(format!("{file}.md"))
}

/// A person a row names: who added a task, who is to do it, who wrote a
/// comment. Todoist names them by user id; the account and its
/// collaborators say who that is.
#[derive(Debug, Clone)]
struct Person {
    name: String,
    handle: Option<Handle>,
}

struct People<'a> {
    store: &'a Store,
    inputs: &'a Inputs,
}

impl People<'_> {
    fn get(&self, uid: Option<String>) -> Option<Person> {
        let uid = uid?;
        let uid = uid.as_str();
        let from_user = self
            .store
            .user
            .as_ref()
            .filter(|(id, _)| id == uid)
            .map(|(_, u)| u);
        let found = match from_user {
            Some(u) => Some(u),
            None => {
                self.inputs.read("collaborators", uid);
                self.store.collaborators.get(uid)
            }
        };
        let Some(p) = found else {
            return Some(Person {
                name: format!("Todoist user {uid}"),
                handle: None,
            });
        };
        let handle = str_of(p, "email").and_then(Handle::email);
        let name = str_of(p, "full_name")
            .map(String::from)
            .or_else(|| handle.as_ref().map(|h| h.value().to_string()))
            .unwrap_or_else(|| format!("Todoist user {uid}"));
        Some(Person { name, handle })
    }
}

/// One task as it renders: the row that says the most about it now.
#[derive(Debug, Clone)]
struct Task<'a> {
    id: &'a str,
    row: &'a Value,
    done: bool,
    /// When it was completed, from either row.
    completed_at: Option<&'a str>,
}

impl<'a> Task<'a> {
    fn section(&self) -> Option<&'a str> {
        str_of(self.row, "section_id")
    }
    fn parent(&self) -> Option<&'a str> {
        str_of(self.row, "parent_id")
    }
    fn order(&self) -> i64 {
        self.row
            .get("child_order")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MAX)
    }
}

/// A project's tasks. The sync's row wins over the completed list's: it
/// is the newer word, and it says whether a completed task was opened
/// again. A task either calls deleted is gone.
fn tasks_of<'a>(store: &'a Store, project: &str, inputs: &Inputs) -> Vec<Task<'a>> {
    let mut out = Vec::new();
    let ids = store
        .items
        .iter()
        .chain(store.completed.iter())
        .filter(|(_, t)| str_of(t, "project_id") == Some(project))
        .map(|(id, _)| id.as_str());
    let mut seen: Vec<&str> = ids.collect();
    seen.sort_unstable();
    seen.dedup();
    for id in seen {
        let synced = store.items.get(id);
        let completed = store.completed.get(id);
        if synced.is_some() {
            inputs.read("items", id);
        }
        if completed.is_some() {
            inputs.read("completed_items", id);
        }
        let row = match (synced, completed) {
            (Some(s), _) => s,
            (None, Some(c)) => c,
            (None, None) => continue,
        };
        // A task moved out of this project since the completed list saw it.
        if str_of(row, "project_id") != Some(project) || flag(row, "is_deleted") {
            continue;
        }
        let done = synced.is_none() || flag(row, "checked");
        let completed_at = str_of(row, "completed_at")
            .or_else(|| completed.and_then(|c| str_of(c, "completed_at")));
        out.push(Task {
            id,
            row,
            done,
            completed_at: if done { completed_at } else { None },
        });
    }
    out
}

/// What the head, the rows and the sections of one project share.
struct ProjectFacts {
    source_id: String,
    project_id: String,
    id: Identity,
    name: String,
    known: bool,
    url: String,
    md_rel: String,
    account: Option<String>,
}

/// One project's document: what the store holds of it, written to its
/// `.md` under `root`. `inputs` gets every raw row it read.
pub fn render_project(
    root: &Path,
    source_id: &str,
    store: &Store,
    project_id: &str,
    inputs: &Inputs,
) -> Result<RenderedMarkdown> {
    let doc = build(source_id, store, project_id, inputs);
    let md_path = root.join(md_rel(source_id, project_id));
    if let Some(dir) = md_path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("mkdir -p {}", dir.display()))?;
    }
    fs::write(&md_path, join(&doc.sections))
        .with_context(|| format!("write {}", md_path.display()))?;
    Ok(RenderedMarkdown { md_path, ..doc })
}

/// [`render_project`] without the file: the document as it would be
/// stored, its `md_path` relative to the data root.
pub fn build(
    source_id: &str,
    store: &Store,
    project_id: &str,
    inputs: &Inputs,
) -> RenderedMarkdown {
    inputs.read("projects", project_id);
    let project = store.projects.get(project_id);
    if let Some((uid, _)) = &store.user {
        inputs.read("user", uid);
    }
    let people = People { store, inputs };
    let rel = md_rel(source_id, project_id).to_string_lossy().into_owned();
    let facts = ProjectFacts {
        source_id: source_id.to_string(),
        project_id: project_id.to_string(),
        id: ids::project(source_id, project_id),
        name: project
            .and_then(|p| str_of(p, "name"))
            .map(String::from)
            .unwrap_or_else(|| format!("Project {project_id}")),
        known: project.is_some(),
        url: project_url(project_id),
        md_rel: rel,
        account: store
            .user
            .as_ref()
            .and_then(|(_, u)| str_of(u, "email"))
            .map(String::from),
    };

    let tasks = tasks_of(store, project_id, inputs);
    let mut notes: HashMap<&str, Vec<(&str, &Value)>> = HashMap::new();
    for (id, n) in &store.notes {
        let Some(task) = str_of(n, "item_id") else {
            continue;
        };
        if tasks.iter().any(|t| t.id == task) {
            inputs.read("notes", id);
            if !flag(n, "is_deleted") {
                notes.entry(task).or_default().push((id.as_str(), n));
            }
        }
    }
    for list in notes.values_mut() {
        list.sort_by(|a, b| (str_of(a.1, "posted_at"), a.0).cmp(&(str_of(b.1, "posted_at"), b.0)));
    }
    let mut project_notes: Vec<(&str, &Value)> = Vec::new();
    for (id, n) in &store.project_notes {
        if str_of(n, "project_id") == Some(project_id) {
            inputs.read("project_notes", id);
            if !flag(n, "is_deleted") {
                project_notes.push((id.as_str(), n));
            }
        }
    }
    project_notes
        .sort_by(|a, b| (str_of(a.1, "posted_at"), a.0).cmp(&(str_of(b.1, "posted_at"), b.0)));

    // Sections in their order; tasks in no section, or in one that is
    // gone, come first under no heading.
    let mut sections: Vec<(&str, &Value)> = Vec::new();
    for (id, s) in &store.sections {
        if str_of(s, "project_id") == Some(project_id) {
            inputs.read("sections", id);
            if !flag(s, "is_deleted") {
                sections.push((id.as_str(), s));
            }
        }
    }
    sections.sort_by_key(|(id, s)| (s.get("section_order").and_then(Value::as_i64), *id));
    let section_ids: Vec<&str> = sections.iter().map(|(id, _)| *id).collect();
    let mut grouped: BTreeMap<usize, Vec<&Task>> = BTreeMap::new();
    for t in &tasks {
        let at = t
            .section()
            .and_then(|s| section_ids.iter().position(|x| *x == s))
            .map_or(0, |i| i + 1);
        grouped.entry(at).or_default().push(t);
    }

    let mut out = Out {
        facts: &facts,
        people: &people,
        sections: vec![Section::unkeyed(head(&facts, project))],
        rows: Vec::new(),
        problems: Vec::new(),
        stamps: Vec::new(),
        items: 0,
    };
    for s in [
        project.and_then(|p| str_of(p, "created_at")),
        project.and_then(|p| str_of(p, "updated_at")),
    ] {
        out.stamp(s);
    }
    if !project_notes.is_empty() {
        out.sections.push(Section::unkeyed(
            "## Comments on the project\n\n".to_string(),
        ));
        for (id, n) in &project_notes {
            out.comment(id, n, None, true);
        }
    }
    for (at, group) in grouped {
        let section_name =
            (at > 0).then(|| str_of(sections[at - 1].1, "name").unwrap_or("(untitled section)"));
        if let Some(name) = section_name {
            let archived = if flag(sections[at - 1].1, "is_archived") {
                " (archived)"
            } else {
                ""
            };
            out.sections.push(Section::unkeyed(format!(
                "## {}{archived}\n\n",
                escape_md_inline(name)
            )));
        }
        let open: Vec<&Task> = group.iter().copied().filter(|t| !t.done).collect();
        for (t, depth) in tree(&open) {
            out.task(t, depth, section_name, notes.get(t.id));
        }
        let mut done: Vec<&Task> = group.iter().copied().filter(|t| t.done).collect();
        done.sort_by(|a, b| (b.completed_at, b.id).cmp(&(a.completed_at, a.id)));
        if !done.is_empty() {
            out.sections.push(Section::unkeyed(format!(
                "<details class=\"todoist-completed\">\n<summary>{}</summary>\n\n",
                escape_text(&format!("✓ {} completed", done.len()))
            )));
            for t in done {
                out.task(t, 0, section_name, notes.get(t.id));
            }
            out.sections
                .push(Section::unkeyed("</details>\n\n".to_string()));
        }
    }

    let Out {
        sections: md_sections,
        mut rows,
        mut problems,
        stamps,
        items,
        ..
    } = out;
    let created = stamps.iter().min().map(|(_, s)| s.clone());
    let modified = stamps.iter().max().map(|(_, s)| s.clone());
    let body = match project.and_then(|p| str_of(p, "description")) {
        Some(d) => format!("{}\n{d}", facts.name),
        None => facts.name.clone(),
    };
    let doc_row = GridRow::builder()
        .uuid(facts.id.uuid.clone())
        .provider(Provider::Todoist)
        .kind(KIND_PROJECT)
        .source_label(SOURCE_LABEL)
        .is_document(true)
        .item_count(Some(items))
        .created_at(created)
        .modified_at(modified)
        .account(facts.account.clone())
        .conversation_name(Some(facts.name.clone()))
        .conversation_uuid(facts.id.uuid.clone())
        .entire_chat(format!("/chat/{}", facts.id.uuid))
        .body(body)
        .qmd_path(Some(facts.md_rel.clone()))
        .source_url(Some(facts.url.clone()))
        .upstream_id(Some(facts.id.natural_key.clone()))
        .upstream_entity_kind(Some(facts.id.entity_kind.to_string()))
        .markdown_uuid(Some(facts.id.uuid.clone()))
        .build_or_record(source_id, project_id, RENDER_VERSION, &mut problems);
    rows.splice(0..0, doc_row);
    RenderedMarkdown {
        markdown_uuid: facts.id.uuid.clone(),
        source_id: source_id.to_string(),
        upstream_cursor: None,
        bucket_key: Some(project_id.to_string()),
        md_path: PathBuf::from(&facts.md_rel),
        render_version: RENDER_VERSION,
        rows,
        sections: md_sections,
        edges: Vec::new(),
        contacts: Vec::new(),
        problems,
    }
}

/// `tasks` in document order, each with its depth: every task after its
/// parent, siblings by `child_order`. A task whose parent is not among
/// them (completed, deleted, elsewhere) is a root.
fn tree<'t, 'a>(tasks: &[&'t Task<'a>]) -> Vec<(&'t Task<'a>, usize)> {
    let ids: Vec<&str> = tasks.iter().map(|t| t.id).collect();
    let mut children: HashMap<Option<&str>, Vec<&'t Task<'a>>> = HashMap::new();
    for t in tasks {
        let parent = t.parent().filter(|p| ids.contains(p) && *p != t.id);
        children.entry(parent).or_default().push(t);
    }
    for list in children.values_mut() {
        list.sort_by_key(|t| (t.order(), t.id));
    }
    let mut out = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut stack: Vec<(&'t Task<'a>, usize)> = children
        .get(&None)
        .map(|r| r.iter().rev().map(|t| (*t, 0)).collect())
        .unwrap_or_default();
    while let Some((t, depth)) = stack.pop() {
        // A cycle of parents would otherwise loop for good.
        if !seen.insert(t.id) {
            continue;
        }
        out.push((t, depth));
        if let Some(kids) = children.get(&Some(t.id)) {
            stack.extend(kids.iter().rev().map(|k| (*k, depth + 1)));
        }
    }
    out
}

fn head(facts: &ProjectFacts, project: Option<&Value>) -> String {
    let mut s = String::from("---\n");
    s.push_str(&format!("title: {}\n", yaml_scalar(&facts.name)));
    s.push_str(&format!("provider: {}\n", Provider::Todoist));
    s.push_str(&format!("source_label: {}\n", yaml_scalar(SOURCE_LABEL)));
    s.push_str(&format!("markdown_uuid: {}\n", facts.id.uuid));
    s.push_str(&format!("project_id: {}\n", yaml_scalar(&facts.project_id)));
    for k in ["created_at", "updated_at"] {
        if let Some(v) = project.and_then(|p| str_of(p, k)) {
            s.push_str(&format!("{k}: {}\n", yaml_scalar(v)));
        }
    }
    s.push_str(&format!("source_url: {}\n", yaml_scalar(&facts.url)));
    s.push_str("---\n\n");
    s.push_str(
        &Title {
            text: &facts.name,
            suffix: None,
            markdown_uuid: Some(&facts.id.uuid),
            source_url: Some(&facts.url),
        }
        .render(),
    );
    if !facts.known {
        s.push_str(
            "*Todoist's sync did not send this project, so only its tasks are known; it may be \
             archived.*\n\n",
        );
    }
    if let Some(p) = project {
        if flag(p, "is_archived") {
            s.push_str("*Archived.*\n\n");
        }
        if let Some(d) = str_of(p, "description") {
            s.push_str(&escape_html_outside_code(d));
            s.push_str("\n\n");
        }
    }
    s
}

struct Out<'f, 'p, 'a> {
    facts: &'f ProjectFacts,
    people: &'p People<'a>,
    sections: Vec<Section>,
    rows: Vec<GridRow>,
    problems: Vec<ProblemRow>,
    /// Every stamp in the document, as `(ms, as written)`, for the
    /// document row's span.
    stamps: Vec<(i64, String)>,
    items: i64,
}

impl Out<'_, '_, '_> {
    fn stamp(&mut self, s: Option<&str>) {
        let Some(s) = s.and_then(datalib_time::coerce_record_stamp) else {
            return;
        };
        if let Some(ms) = datalib_time::record_stamp_ms(&s) {
            self.stamps.push((ms, s));
        }
    }

    fn task(
        &mut self,
        t: &Task,
        depth: usize,
        section: Option<&str>,
        notes: Option<&Vec<(&str, &Value)>>,
    ) {
        let row = t.row;
        let added = str_of(row, "added_at");
        let updated = str_of(row, "updated_at");
        let id = ids::task(&self.facts.source_id, t.id, added);
        self.stamp(added);
        self.stamp(updated);
        self.stamp(t.completed_at);
        let content = str_of(row, "content").unwrap_or("(no text)");
        let description = str_of(row, "description");

        let level = "#".repeat((3 + depth).min(6));
        let check = if t.done { "☑" } else { "☐" };
        let mut s = msg_div_open_with(&id.uuid, Provider::Todoist, &["todoist-task"]);
        s.push_str("\n\n");
        s.push_str(&format!(
            "{level} {check} {}\n\n",
            escape_html_outside_code(&content.replace(['\r', '\n'], " "))
        ));
        let meta = self.meta(t);
        if !meta.is_empty() {
            s.push_str(&meta.join(" · "));
            s.push_str("\n\n");
        }
        if let Some(d) = description {
            s.push_str(&escape_html_outside_code(d));
            s.push_str("\n\n");
        }
        s.push_str(MSG_DIV_CLOSE);
        s.push_str("\n\n");
        self.sections.push(Section::keyed(&id.uuid, s));

        let adder = self
            .people
            .get(uid_of(row, "added_by_uid").or_else(|| uid_of(row, "user_id")));
        let labels = labels(row);
        let mut body = content.to_string();
        if let Some(d) = description {
            body.push('\n');
            body.push_str(d);
        }
        if !labels.is_empty() {
            body.push('\n');
            body.push_str(
                &labels
                    .iter()
                    .map(|l| format!("@{l}"))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
        let modified = updated.filter(|u| Some(*u) != added).map(String::from);
        let index = self.rows.len() as i64;
        let built = GridRow::builder()
            .uuid(id.uuid.clone())
            .provider(Provider::Todoist)
            .kind(KIND_TASK)
            .source_label(SOURCE_LABEL)
            .item_count(Some(1))
            .created_at(added.map(String::from))
            .modified_at(modified)
            .author(adder.as_ref().map(|p| p.name.clone()))
            .author_handle(adder.and_then(|p| p.handle).map(|h| h.as_str().to_string()))
            .account(self.facts.account.clone())
            .channel(section.map(String::from))
            .conversation_name(Some(self.facts.name.clone()))
            .conversation_uuid(self.facts.id.uuid.clone())
            .message_index(Some(index))
            .entire_chat(format!("/chat/{}", self.facts.id.uuid))
            .body(body)
            .qmd_path(Some(self.facts.md_rel.clone()))
            .source_url(Some(task_url(t.id)))
            .upstream_id(Some(id.natural_key.clone()))
            .upstream_entity_kind(Some(id.entity_kind.to_string()))
            .markdown_uuid(Some(self.facts.id.uuid.clone()))
            .build_or_record(
                &self.facts.source_id,
                t.id,
                RENDER_VERSION,
                &mut self.problems,
            );
        self.rows.extend(built);
        self.items += 1;

        for (note_id, n) in notes.into_iter().flatten() {
            self.comment(note_id, n, section, false);
        }
    }

    /// The line under a task's text: its priority, dates, labels, who
    /// is to do it, and when it was done.
    fn meta(&self, t: &Task) -> Vec<String> {
        let row = t.row;
        let mut out = Vec::new();
        // The API counts priority up (4 is most urgent); the app shows
        // it down (p1).
        if let Some(p @ 2..=4) = row.get("priority").and_then(Value::as_i64) {
            out.push(format!("p{}", 5 - p));
        }
        if let Some(due) = row.get("due").filter(|d| d.is_object()) {
            if let Some(date) = str_of(due, "date") {
                let mut s = format!("due {}", stamp_text_html(date));
                if flag(due, "is_recurring") {
                    if let Some(said) = str_of(due, "string") {
                        s.push_str(&format!(" ({})", escape_md_inline(said)));
                    }
                }
                out.push(s);
            }
        }
        if let Some(date) = row.get("deadline").and_then(|d| str_of(d, "date")) {
            out.push(format!("deadline {}", stamp_text_html(date)));
        }
        if let Some(d) = row.get("duration").filter(|d| d.is_object()) {
            if let (Some(n), Some(unit)) =
                (d.get("amount").and_then(Value::as_i64), str_of(d, "unit"))
            {
                let unit = if n == 1 {
                    unit.to_string()
                } else {
                    format!("{unit}s")
                };
                out.push(format!("takes {n} {}", escape_md_inline(&unit)));
            }
        }
        let labels = labels(row);
        if !labels.is_empty() {
            out.push(
                labels
                    .iter()
                    .map(|l| escape_md_inline(&format!("@{l}")))
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
        if let Some(p) = self.people.get(uid_of(row, "responsible_uid")) {
            out.push(format!("assigned to {}", person_md(&p)));
        }
        if t.done {
            match t.completed_at {
                Some(at) => out.push(format!("completed {}", stamp_text_html(at))),
                None => out.push("completed".to_string()),
            }
        }
        out
    }

    fn comment(&mut self, note_id: &str, n: &Value, section: Option<&str>, on_project: bool) {
        let posted = str_of(n, "posted_at");
        let id = if on_project {
            ids::project_comment(&self.facts.source_id, note_id, posted)
        } else {
            ids::comment(&self.facts.source_id, note_id, posted)
        };
        self.stamp(posted);
        let author = self.people.get(uid_of(n, "posted_uid"));
        let content = str_of(n, "content").unwrap_or("");
        let mut s = msg_div_open_with(&id.uuid, Provider::Todoist, &["todoist-comment"]);
        s.push_str("\n\n");
        s.push_str(
            &MessageHeader {
                author: author.as_ref().map_or("", |p| p.name.as_str()),
                handle: author.as_ref().and_then(|p| p.handle.as_ref()),
                date_ms: posted.and_then(datalib_time::record_stamp_ms),
                source_url: None,
            }
            .render(),
        );
        s.push_str("\n\n");
        if !content.is_empty() {
            s.push_str(&escape_html_outside_code(content));
            s.push_str("\n\n");
        }
        let attachment = n.get("file_attachment").filter(|a| a.is_object());
        if let Some(a) = attachment {
            let name = str_of(a, "file_name").unwrap_or("attachment");
            match str_of(a, "file_url") {
                Some(url) => s.push_str(&format!(
                    "📎 [{}]({})\n\n",
                    escape_md_inline(name),
                    md_link_dest(url)
                )),
                None => s.push_str(&format!("📎 {}\n\n", escape_md_inline(name))),
            }
        }
        s.push_str(MSG_DIV_CLOSE);
        s.push_str("\n\n");
        self.sections.push(Section::keyed(&id.uuid, s));

        let mut body = content.to_string();
        if let Some(name) = attachment.and_then(|a| str_of(a, "file_name")) {
            body.push('\n');
            body.push_str(name);
        }
        let index = self.rows.len() as i64;
        let built = GridRow::builder()
            .uuid(id.uuid.clone())
            .provider(Provider::Todoist)
            .kind(KIND_COMMENT)
            .source_label(SOURCE_LABEL)
            .item_count(Some(1))
            .created_at(posted.map(String::from))
            .author(author.as_ref().map(|p| p.name.clone()))
            .author_handle(
                author
                    .and_then(|p| p.handle)
                    .map(|h| h.as_str().to_string()),
            )
            .account(self.facts.account.clone())
            .channel(section.map(String::from))
            .conversation_name(Some(self.facts.name.clone()))
            .conversation_uuid(self.facts.id.uuid.clone())
            .message_index(Some(index))
            .entire_chat(format!("/chat/{}", self.facts.id.uuid))
            .body(body)
            .qmd_path(Some(self.facts.md_rel.clone()))
            .source_url(Some(match str_of(n, "item_id") {
                Some(task) => task_url(task),
                None => self.facts.url.clone(),
            }))
            .upstream_id(Some(id.natural_key.clone()))
            .upstream_entity_kind(Some(id.entity_kind.to_string()))
            .markdown_uuid(Some(self.facts.id.uuid.clone()))
            .build_or_record(
                &self.facts.source_id,
                note_id,
                RENDER_VERSION,
                &mut self.problems,
            );
        self.rows.extend(built);
        self.items += 1;
    }
}

fn labels(row: &Value) -> Vec<&str> {
    row.get("labels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect()
}

fn person_md(p: &Person) -> String {
    match &p.handle {
        Some(h) => chip_link(&p.name, h),
        None => escape_md_inline(&p.name),
    }
}
