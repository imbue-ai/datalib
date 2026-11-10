//! The text-edge library (`etl/text_edges/cases.toml`): each case's text
//! rendered as the first of two messages through the path its `enters`
//! names, with what search would hold of it. The page judges it, in
//! `datalib/ui/tests/text_edges.test.ts`.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{bail, Context, Result};
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::render::ENTITY_KIND_CONVERSATION;
use datalib_etl_chat_common::types::AuthorKind;
use datalib_etl_chat_common::{
    ItemKind, NormalizedChat, NormalizedChatItem, NormalizedDoc, RecordStampPrecision,
    RenderProfile, TextFormat,
};
use datalib_etl_forge_render_common::{ChangeRequest, Comment, ForgeProfile, Parsed};
use datalib_etl_render::inputs::Inputs;
use datalib_etl_render::section::Section;
use datalib_etl_slack_render::render::mrkdwn::{to_commonmark, Labels};
use datalib_schema::plain_text::plain_text;
use datalib_schema::providers::Provider;
use serde::{Deserialize, Serialize};

const CASES: &str = include_str!("../../text_edges/cases.toml");

pub const FIRST: &str = "00000000-0000-8000-8000-0000000e0001";
pub const NEXT: &str = "00000000-0000-8000-8000-0000000e0002";
const NEXT_TEXT: &str = "Make it so.";
/// What a case's text is measured against: the same path with nothing
/// in the text but a word.
const BASELINE_TEXT: &str = "Engage.";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cases {
    case: Vec<CaseIn>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaseIn {
    id: String,
    edge: String,
    enters: Enters,
    text: String,
    #[serde(default)]
    shows: Vec<String>,
    #[serde(default)]
    hides: Vec<String>,
    #[serde(default)]
    finds: Vec<String>,
    #[serde(default)]
    elements: BTreeMap<String, u32>,
    fails_today: Option<String>,
}

/// How a case's text arrives, each the path production takes for it.
#[derive(Deserialize, Serialize, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[serde(rename_all = "snake_case")]
enum Enters {
    /// Markdown an assistant or a person wrote (Claude, ChatGPT, Claude
    /// Code, Codex, Todoist): as written, then chat-common.
    Markdown,
    /// A Slack message as Slack's API sends it: `to_commonmark`, then
    /// chat-common.
    Slack,
    /// A GitHub pull request's description, with one comment after it:
    /// forge-render-common.
    Forge,
    /// An email's HTML body: email's HTML-to-markdown step, then
    /// chat-common.
    EmailHtml,
    /// A text message (Signal, WhatsApp, LinkedIn): chat-common's plain
    /// text.
    Plain,
}

#[derive(Serialize)]
pub struct Edges {
    cases: Vec<CaseOut>,
    /// Per `enters`, the page for [`BASELINE_TEXT`]: an element a case
    /// adds is one its page has more of than this.
    baselines: BTreeMap<String, Rendered>,
}

#[derive(Serialize)]
struct CaseOut {
    id: String,
    edge: String,
    enters: Enters,
    text: String,
    shows: Vec<String>,
    hides: Vec<String>,
    finds: Vec<String>,
    elements: BTreeMap<String, u32>,
    fails_today: Option<String>,
    rendered: Rendered,
}

/// One document as the document route would send it, and as search
/// would hold it.
#[derive(Serialize)]
struct Rendered {
    /// The file, front matter included: what qmd indexes.
    file: String,
    /// The file after its front matter: what the page draws.
    body: String,
    /// The body's sections, as the route sends their layout; `None` when
    /// the renderer declares none and the page draws the body whole.
    sections: Option<Vec<SectionOut>>,
    /// The body as the grid's text (`plain_text`).
    grid_text: String,
    next_uuid: &'static str,
    next_text: &'static str,
}

#[derive(Serialize)]
struct SectionOut {
    uuid: Option<String>,
    md: String,
}

pub fn edges(scratch: &Path) -> Result<Edges> {
    let parsed: Cases = toml::from_str(CASES).context("parse text_edges/cases.toml")?;
    let mut seen = std::collections::BTreeSet::new();
    let mut cases = Vec::new();
    for (i, c) in parsed.case.into_iter().enumerate() {
        if !seen.insert(c.id.clone()) {
            bail!("text_edges: two cases are named {}", c.id);
        }
        let dir = scratch.join(format!("edge-{i}"));
        let rendered = render(c.enters, &c.text, &dir).with_context(|| c.id.clone())?;
        cases.push(CaseOut {
            id: c.id,
            edge: c.edge,
            enters: c.enters,
            text: c.text,
            shows: c.shows,
            hides: c.hides,
            finds: c.finds,
            elements: c.elements,
            fails_today: c.fails_today,
            rendered,
        });
    }
    let mut baselines = BTreeMap::new();
    for enters in [
        Enters::Markdown,
        Enters::Slack,
        Enters::Forge,
        Enters::EmailHtml,
        Enters::Plain,
    ] {
        let dir = scratch.join(format!("baseline-{enters:?}"));
        let name = serde_json::to_value(enters)?
            .as_str()
            .unwrap_or_default()
            .to_string();
        baselines.insert(name, render(enters, BASELINE_TEXT, &dir)?);
    }
    Ok(Edges { cases, baselines })
}

fn render(enters: Enters, text: &str, dir: &Path) -> Result<Rendered> {
    let (file, sections) = match enters {
        Enters::Markdown => chat(
            Provider::Claude,
            TextFormat::Markdown,
            text.to_string(),
            dir,
        )?,
        Enters::Slack => {
            let inputs = Inputs::default();
            let none = BTreeMap::new();
            let labels = Labels {
                users: inputs.lookup("users", &none),
                channels: inputs.lookup("channels", &none),
                team_id: "T1701",
            };
            chat(
                Provider::Slack,
                TextFormat::Markdown,
                to_commonmark(text, labels),
                dir,
            )?
        }
        Enters::EmailHtml => chat(
            Provider::Email,
            TextFormat::Markdown,
            datalib_etl_email_render::render::render::html_markdown(text),
            dir,
        )?,
        Enters::Plain => chat(Provider::Signal, TextFormat::Plain, text.to_string(), dir)?,
        Enters::Forge => forge(text, dir)?,
    };
    let skip = front_matter_len(&file);
    let body = file[skip..].to_string();
    let sections = sections.map(|mut s| {
        if let Some(first) = s.first_mut() {
            first.md = first.md[skip.min(first.md.len())..].to_string();
        }
        s
    });
    Ok(Rendered {
        grid_text: plain_text(&body, usize::MAX),
        file,
        body,
        sections,
        next_uuid: NEXT,
        next_text: NEXT_TEXT,
    })
}

/// Bytes of the `---` front matter the file opens with, if any.
fn front_matter_len(file: &str) -> usize {
    let Some(rest) = file.strip_prefix("---\n") else {
        return 0;
    };
    rest.find("\n---\n").map_or(0, |end| 4 + end + 5)
}

type Pieces = Option<Vec<SectionOut>>;

fn pieces(sections: Vec<Section>) -> Pieces {
    (!sections.is_empty()).then(|| {
        sections
            .into_iter()
            .map(|s| SectionOut {
                uuid: s.uuid,
                md: s.md,
            })
            .collect()
    })
}

fn chat(
    provider: Provider,
    format: TextFormat,
    text: String,
    dir: &Path,
) -> Result<(String, Pieces)> {
    let profile = RenderProfile {
        provider,
        source_label: "Text edges".to_string(),
        chat_kind: "Chat".to_string(),
        message_kind: "Message".to_string(),
        reaction_kind: "Reaction".to_string(),
        chat_entity_kind: ENTITY_KIND_CONVERSATION,
        stamp_precision: RecordStampPrecision::Seconds,
        render_version: 1,
        text_format: format,
    };
    let item = |uuid: &str, author: &str, text: String| NormalizedChatItem {
        message_uuid: uuid.to_string(),
        author_handle: None,
        author_kind: AuthorKind::Named,
        author_display: author.to_string(),
        date_ms: Some(12_602_794_200_000),
        date_offset_secs: None,
        text: Some(text),
        kind: ItemKind::Text,
        attachments: vec![],
        reactions: vec![],
        labels: Vec::new(),
        system_note: None,
        source_url: None,
        kind_label: None,
        source_ref: None,
        is_aside: false,
        branch: Vec::new(),
        unread: false,
        replies: None,
        recipients: Vec::new(),
        mentions: Vec::new(),
        problems: Vec::new(),
    };
    let chat = NormalizedChat {
        named_after: Vec::new(),
        contacts: Vec::new(),
        inputs: Vec::new(),
        id: "text-edge".into(),
        chat_uuid: "00000000-0000-8000-8000-0000000e00c1".into(),
        display: "Ten Forward".into(),
        title: Some("Ten Forward".into()),
        author: None,
        account: None,
        project: None,
        external_id: None,
        upstream_account: None,
        source_url: None,
        org_uuid: None,
        org_name: None,
        buckets: vec![NormalizedDoc {
            period_key: "all".to_string(),
            markdown_uuid: "00000000-0000-8000-8000-0000000e00d1".into(),
            source_ref: None,
            items: vec![
                item(FIRST, "Quark", text),
                item(NEXT, "Picard", NEXT_TEXT.to_string()),
            ],
            orphan_reactions: Vec::new(),
        }],
    };
    let mut docs = Vec::new();
    datalib_etl_chat_common::render_all(
        &profile,
        &[chat],
        dir,
        "text-edges",
        &HashMap::new(),
        &Progress::noop(),
        &mut |doc| {
            docs.push((doc.md_path, doc.sections));
            Ok(())
        },
    )?;
    one_doc(docs)
}

fn forge(text: &str, dir: &Path) -> Result<(String, Pieces)> {
    let profile = ForgeProfile {
        provider: Provider::Github,
        tag: "github",
        source_label: "GitHub",
        doc_kind: "GitHub PR",
        doc_entity_kind: "pull_request",
        table: "pull_requests",
        container_key: "repo",
        number_key: "pr_number",
        from_ref_key: "head_ref",
        to_ref_key: "base_ref",
        number_sigil: '#',
        reviews: true,
        render_version: 1,
    };
    let cr = ChangeRequest {
        uuid: FIRST.into(),
        row_id: "r".into(),
        container: "starfleet/enterprise".into(),
        number: 1701,
        title: "Recalibrate the deflector".into(),
        body: text.to_string(),
        state: Some("open".into()),
        url: None,
        head_sha: None,
        base_sha: None,
        from_ref: None,
        to_ref: None,
        author: Some("Geordi La Forge".into()),
        author_handle: datalib_handle::Handle::github("glaforge"),
        created_at: Some("2369-04-15T08:30:00+00:00".into()),
        updated_at: None,
        merged_at: None,
    };
    let comment = Comment {
        uuid: NEXT.into(),
        table: "issue_comments",
        row_id: "c1".into(),
        parent_row_id: "r".into(),
        kind: "Comment",
        entity_kind: "issue_comment",
        section: datalib_etl_forge_render_common::Section::General,
        external_id: 1,
        in_reply_to_id: None,
        author: Some("Picard".into()),
        author_handle: None,
        body: NEXT_TEXT.into(),
        url: None,
        path: None,
        line: None,
        commit_sha: None,
        created_at: "2369-04-15T09:00:00+00:00".into(),
        updated_at: None,
        state: None,
    };
    let parsed = Parsed {
        change_requests: vec![cr],
        comments: vec![comment],
        ..Default::default()
    };
    let mut docs = Vec::new();
    datalib_etl_forge_render_common::render_all(
        &profile,
        &parsed,
        dir,
        "text-edges",
        &Progress::noop(),
        &mut |doc| {
            docs.push((doc.md_path, doc.sections));
            Ok(())
        },
    )?;
    one_doc(docs)
}

fn one_doc(docs: Vec<(std::path::PathBuf, Vec<Section>)>) -> Result<(String, Pieces)> {
    let [(path, sections)] = <[_; 1]>::try_from(docs)
        .map_err(|d| anyhow::anyhow!("expected one document, got {}", d.len()))?;
    let file =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    Ok((file, pieces(sections)))
}
