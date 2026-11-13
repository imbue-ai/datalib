//! The skills on the export owner's profile (`Skills.csv`) as one
//! document, a section and a grid row per skill, so a skill is found by
//! search like anything else the export holds.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use datalib_etl::layout::render_markdown_root;
use datalib_etl::progress::Progress;
use datalib_etl_render::front_matter::{yaml_name, yaml_scalar};
use datalib_etl_render::grid_index::{doc_md_path, RenderedMarkdown};
use datalib_etl_render::html::escape_name_inline;
use datalib_etl_render::inputs::{changed_rows, Bucket, Input, Inputs};
use datalib_etl_render::section::{join, msg_div_open_with, Section, MSG_DIV_CLOSE};
use datalib_etl_render::title::Title;
use datalib_schema::grid_rows::GridRow;
use datalib_schema::problems::{Outcome, Problem, ProblemRow, Reason, Scope, Stage};
use datalib_schema::providers::Provider;
use serde_json::Value;

use crate::ids;
use crate::processor::{FeedOutcome, Source};
use crate::render::{field, narrow_docs, nonempty, RENDER_VERSION};
use datalib_etl_linkedin::ingest::{db_path_for, RawDb};

pub const TABLE: &str = "skills";
const SOURCE_LABEL: &str = "LinkedIn";
const TITLE: &str = "LinkedIn skills";
const KIND_SKILLS: &str = "LinkedIn Skills";
const KIND_SKILL: &str = "LinkedIn Skill";

/// The skills document, its `md_path` relative to the data root, and the
/// rows it read.
pub struct SkillsDoc {
    pub doc: RenderedMarkdown,
    pub inputs: Vec<Input>,
}

pub fn render_skills(
    source: &Source<'_>,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<FeedOutcome> {
    let db_path = db_path_for(source.raw_dir);
    if !db_path.exists() {
        return Ok(FeedOutcome::default());
    }
    let Some((rows, changed, new_head)) = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            // Read at a commit: this store belongs to the download step, and
            // nothing committed means nothing to render from.
            let Some(db) = RawDb::open_reader(&db_path, source.range.pin).await? else {
                return Ok(None);
            };
            let pin = db.pin().expect("a reader is pinned at open").clone();
            let rows =
                datalib_etl::doltlite_raw::load_payloads_with_id_if_present(db.pool(), TABLE)
                    .await
                    .context("load skills")?;
            let changed = changed_rows(db.pool(), source.range, &pin, &[TABLE]).await?;
            // Closed, not dropped: the next open of this store is a
            // second connection until this one is actually gone.
            db.close().await;
            Ok::<_, anyhow::Error>(Some((rows, changed, pin.commit().to_string())))
        })
    })?
    else {
        return Ok(FeedOutcome::default());
    };

    let mut docs: Vec<SkillsDoc> = build(source.name, &rows, source.account, source.account_inputs)
        .into_iter()
        .collect();
    let mut outcome = narrow_docs(&mut docs, doc_key, changed, source.range, new_head);
    for SkillsDoc { doc, inputs } in docs {
        let md_path = source.out_dir.join(&doc.md_path);
        let dir = md_path.parent().expect("a document's file has a folder");
        fs::create_dir_all(dir).with_context(|| format!("mkdir -p {}", dir.display()))?;
        fs::write(&md_path, join(&doc.sections))
            .with_context(|| format!("write {}", md_path.display()))?;
        outcome.buckets.push(Bucket {
            key: doc.markdown_uuid.clone(),
            inputs,
        });
        on_doc_complete(RenderedMarkdown { md_path, ..doc })?;
        progress.inc(1);
    }
    Ok(outcome)
}

fn doc_key(d: &SkillsDoc) -> (&str, &[Input]) {
    (d.doc.markdown_uuid.as_str(), d.inputs.as_slice())
}

/// The one skills document, or `None` when the export lists no skill.
/// Pure: `md_path` is relative to the data root.
pub fn build(
    source_id: &str,
    rows: &[(String, Value)],
    account: Option<&str>,
    account_inputs: &[Input],
) -> Option<SkillsDoc> {
    if rows.is_empty() {
        return None;
    }
    let inputs = Inputs::default();
    for (row_id, _) in rows {
        inputs.read(TABLE, row_id);
    }
    for input in account_inputs {
        inputs.read(&input.table, &input.id);
    }
    let doc_id = ids::skills(source_id);
    let md_rel = doc_md_path(
        &render_markdown_root(Path::new(""), source_id),
        &doc_id.uuid,
    )
    .to_string_lossy()
    .into_owned();

    let mut skills: Vec<(&str, &str)> = Vec::new();
    let mut problems = Vec::new();
    for (row_id, p) in rows {
        match nonempty(field(p, "Name")) {
            Some(name) => skills.push((row_id.as_str(), name)),
            None => problems.push(ProblemRow::new(
                source_id,
                Stage::Render,
                Scope::Markdown(&doc_id.uuid),
                None,
                Outcome::Dropped,
                Problem::explained(
                    Reason::NoIdentity,
                    Some("Name".to_string()),
                    "a row of Skills.csv with no name",
                ),
                Some(RENDER_VERSION),
            )),
        }
    }
    skills.sort_by_key(|(_, name)| name.to_lowercase());

    let mut sections = vec![Section::unkeyed(head(&doc_id))];
    let mut rows_out: Vec<GridRow> = Vec::new();
    // The body is the escaped name, as on the page: the preview reads the
    // escapes back, so a name in markup shows as typed.
    let escaped: Vec<String> = skills.iter().map(|(_, n)| escape_name_inline(n)).collect();
    for (index, ((row_id, _), name)) in skills.iter().zip(&escaped).enumerate() {
        let id = ids::skill(source_id, row_id);
        let mut s = msg_div_open_with(&id.uuid, Provider::Linkedin, &["linkedin-skill"]);
        s.push_str(&format!("\n\n{name}\n\n"));
        s.push_str(MSG_DIV_CLOSE);
        s.push_str("\n\n");
        sections.push(Section::keyed(&id.uuid, s));
        let row = GridRow::builder()
            .uuid(id.uuid.clone())
            .provider(Provider::Linkedin)
            .kind(KIND_SKILL)
            .source_label(SOURCE_LABEL)
            .item_count(Some(1))
            .account(account.map(str::to_string))
            .conversation_name(Some(TITLE.to_string()))
            .conversation_uuid(doc_id.uuid.clone())
            .message_index(Some(index as i64))
            .entire_chat(format!("/chat/{}", doc_id.uuid))
            .body(name.clone())
            .qmd_path(Some(md_rel.clone()))
            .upstream_id(Some(id.natural_key.clone()))
            .upstream_entity_kind(Some(id.entity_kind.to_string()))
            .markdown_uuid(Some(doc_id.uuid.clone()))
            .build_or_record(source_id, row_id, RENDER_VERSION, &mut problems);
        rows_out.extend(row);
    }
    let doc_row = GridRow::builder()
        .uuid(doc_id.uuid.clone())
        .provider(Provider::Linkedin)
        .kind(KIND_SKILLS)
        .source_label(SOURCE_LABEL)
        .is_document(true)
        .item_count(Some(skills.len() as i64))
        .account(account.map(str::to_string))
        .conversation_name(Some(TITLE.to_string()))
        .conversation_uuid(doc_id.uuid.clone())
        .entire_chat(format!("/chat/{}", doc_id.uuid))
        .body(escaped.join("\n"))
        .qmd_path(Some(md_rel.clone()))
        .upstream_id(Some(doc_id.natural_key.clone()))
        .upstream_entity_kind(Some(doc_id.entity_kind.to_string()))
        .markdown_uuid(Some(doc_id.uuid.clone()))
        .build_or_record(source_id, TABLE, RENDER_VERSION, &mut problems);
    rows_out.splice(0..0, doc_row);

    Some(SkillsDoc {
        doc: RenderedMarkdown {
            markdown_uuid: doc_id.uuid.clone(),
            source_id: source_id.to_string(),
            upstream_cursor: None,
            bucket_key: Some(doc_id.uuid.clone()),
            md_path: PathBuf::from(md_rel),
            render_version: RENDER_VERSION,
            rows: rows_out,
            sections,
            section_layout: None,
            content_hash: None,
            edges: Vec::new(),
            contacts: Vec::new(),
            photos: Vec::new(),
            problems,
        },
        inputs: inputs.declared(),
    })
}

fn head(doc_id: &datalib_id::Identity) -> String {
    let mut s = String::from("---\n");
    s.push_str(&format!("title: {}\n", yaml_name(TITLE)));
    s.push_str(&format!("provider: {}\n", Provider::Linkedin));
    s.push_str(&format!("source_label: {}\n", yaml_scalar(SOURCE_LABEL)));
    s.push_str(&format!("markdown_uuid: {}\n", doc_id.uuid));
    s.push_str("---\n\n");
    s.push_str(
        &Title {
            text: TITLE,
            suffix: None,
            markdown_uuid: Some(&doc_id.uuid),
            source_url: None,
        }
        .render(),
    );
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn skill_rows(names: &[&str]) -> Vec<(String, Value)> {
        names
            .iter()
            .map(|n| (n.to_string(), json!({ "Name": n })))
            .collect()
    }

    /// Each skill is a section of the one document and a row of its own
    /// that search finds by the skill's name.
    #[test]
    fn every_skill_is_a_section_and_a_row() {
        let rows = skill_rows(&["Tactical Analysis", "Diplomacy"]);
        let SkillsDoc { doc, inputs } =
            build("li", &rows, Some("data@enterprise.starfleet.test"), &[]).unwrap();
        let kinds: Vec<(&str, &str)> = doc
            .rows
            .iter()
            .map(|r| (r.kind.as_str(), r.preview.as_str()))
            .collect();
        assert_eq!(
            kinds,
            [
                (KIND_SKILLS, "Diplomacy Tactical Analysis"),
                (KIND_SKILL, "Diplomacy"),
                (KIND_SKILL, "Tactical Analysis"),
            ]
        );
        assert_eq!(
            doc.rows.iter().filter(|r| r.is_document).count(),
            1,
            "one document row"
        );
        let keyed: Vec<&str> = doc
            .sections
            .iter()
            .filter_map(|s| s.uuid.as_deref())
            .collect();
        assert_eq!(keyed, [&doc.rows[1].uuid, &doc.rows[2].uuid]);
        let diplomacy = &doc.sections[1].md;
        let uuid = &doc.rows[1].uuid;
        assert!(
            diplomacy.starts_with(&format!(
                "<div id=\"m-{uuid}\" data-section-uuid=\"{uuid}\" class=\"msg"
            )),
            "{diplomacy}"
        );
        assert_eq!(inputs.len(), 2, "both rows declared: {inputs:?}");
    }

    /// A skill is what the person typed: escaped on the page, as typed in
    /// the row search reads.
    #[test]
    fn a_skill_in_markup_renders_escaped() {
        let rows = skill_rows(&["<script>x</script> & co"]);
        let doc = build("li", &rows, None, &[]).unwrap().doc;
        let md = &doc.sections[1].md;
        assert!(
            md.contains("&lt;script&gt;x&lt;/script&gt; &amp; co"),
            "{md}"
        );
        assert_eq!(doc.rows[1].preview, "<script>x</script> & co");
    }

    #[test]
    fn no_skills_is_no_document() {
        assert!(build("li", &[], None, &[]).is_none());
    }

    #[test]
    fn a_skill_with_no_name_is_dropped_on_record() {
        let mut rows = skill_rows(&["Diplomacy"]);
        rows.push(("hash".to_string(), json!({ "Name": " " })));
        let doc = build("li", &rows, None, &[]).unwrap().doc;
        assert_eq!(doc.rows.len(), 2, "the document and Diplomacy");
        assert_eq!(doc.problems.len(), 1);
    }
}
