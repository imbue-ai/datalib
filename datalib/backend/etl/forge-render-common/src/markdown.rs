//! A change request's one document: front matter, title, description,
//! then its reviews, its general conversation and its inline threads.
//! The description and each comment are markdown their authors wrote and
//! go in as written, HTML included, each in a message wrapper and a
//! section of its own, so the page renders each alone: a tag one leaves
//! open cannot reach the next. Everything else here is plain text and is
//! escaped.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl_render::front_matter::{yaml_name, yaml_scalar};
use datalib_etl_render::html::{escape_md_inline, escape_name_inline, md_code_span};
use datalib_etl_render::message::{chip_link, source_link, stamp_text_html};
use datalib_etl_render::section::{join, msg_div_open, Section, MSG_DIV_CLOSE};
use datalib_etl_render::title::Title;
use datalib_handle::Handle;

use crate::{ordered, ChangeRequest, Comment, ForgeProfile};

pub(crate) fn write(
    profile: &ForgeProfile,
    cr: &ChangeRequest,
    comments: &[&Comment],
    md_path: &Path,
) -> Result<Vec<Section>> {
    if let Some(dir) = md_path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let sections = render(profile, cr, comments);
    fs::write(md_path, join(&sections)).with_context(|| format!("write {}", md_path.display()))?;
    Ok(sections)
}

/// The document as sections: the frame (front matter, title, the part
/// headings) unkeyed, and each message keyed by its uuid.
fn render(profile: &ForgeProfile, cr: &ChangeRequest, comments: &[&Comment]) -> Vec<Section> {
    let mut sections = Vec::new();
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("provider: {}\n", profile.tag));
    let front: [(&str, String); 12] = [
        (profile.container_key, yaml_scalar(&cr.container)),
        (profile.number_key, cr.number.to_string()),
        ("title", yaml_name(&cr.title)),
        ("state", yaml_opt(cr.state.as_deref())),
        ("author", yaml_opt(cr.author.as_deref())),
        ("created_at", yaml_opt(cr.created_at.as_deref())),
        ("updated_at", yaml_opt(cr.updated_at.as_deref())),
        ("merged_at", yaml_opt(cr.merged_at.as_deref())),
        ("head_sha", yaml_opt(cr.head_sha.as_deref())),
        ("base_sha", yaml_opt(cr.base_sha.as_deref())),
        (profile.from_ref_key, yaml_opt(cr.from_ref.as_deref())),
        (profile.to_ref_key, yaml_opt(cr.to_ref.as_deref())),
    ];
    for (key, value) in front {
        out.push_str(&format!("{key}: {value}\n"));
    }
    out.push_str("---\n\n");

    let title_text = format!("{} ({}{})", cr.title, profile.number_sigil, cr.number);
    out.push_str(
        &Title {
            suffix: None,
            text: &title_text,
            markdown_uuid: Some(&cr.uuid),
            source_url: cr.url.as_deref(),
        }
        .render(),
    );
    let state = escape_md_inline(cr.state.as_deref().unwrap_or("unknown"));
    let author = person(cr.author.as_deref(), cr.author_handle.as_ref());
    let from = md_code_span(cr.from_ref.as_deref().unwrap_or("?"));
    let to = md_code_span(cr.to_ref.as_deref().unwrap_or("?"));
    out.push_str(&format!("*{state}* — {author} — {from} → {to}\n\n"));

    out.push_str("## Description\n\n");
    if cr.body.trim().is_empty() {
        out.push_str("*(no description)*\n\n");
    } else {
        sections.push(Section::unkeyed(std::mem::take(&mut out)));
        sections.push(message(profile, &cr.uuid, None, &cr.body));
    }

    let ordered = ordered(comments);
    if profile.reviews {
        out.push_str("## Reviews\n\n");
        if ordered.reviews.is_empty() {
            out.push_str("*(no reviews)*\n\n");
        }
        for r in &ordered.reviews {
            push_comment(profile, &mut sections, &mut out, r);
        }
    }

    out.push_str("## General discussion\n\n");
    if ordered.general.is_empty() {
        out.push_str("*(no general comments)*\n\n");
    }
    for c in &ordered.general {
        push_comment(profile, &mut sections, &mut out, c);
    }

    out.push_str("## Inline comments\n\n");
    if ordered.inline.is_empty() {
        out.push_str("*(no inline comments)*\n\n");
    }
    for ((path, line), thread) in &ordered.inline {
        out.push_str(&format!(
            "### {}\n\n",
            md_code_span(&format!("{path}:{line}"))
        ));
        for c in thread {
            push_comment(profile, &mut sections, &mut out, c);
        }
    }

    if !out.is_empty() {
        sections.push(Section::unkeyed(out));
    }
    if let Some(last) = sections.last_mut() {
        while last.md.ends_with("\n\n") {
            last.md.pop();
        }
    }
    sections
}

/// Ends the frame piece `out` holds and adds `c` as a message of its own.
fn push_comment(
    profile: &ForgeProfile,
    sections: &mut Vec<Section>,
    out: &mut String,
    c: &Comment,
) {
    if !out.is_empty() {
        sections.push(Section::unkeyed(std::mem::take(out)));
    }
    let header = header(c);
    sections.push(message(profile, &c.uuid, Some(&header), &c.body));
}

/// One message in its wrapper: the header line, if any, then the body
/// as its author wrote it. A review with no body is a bare approval, so
/// its header says everything; a comment with none says it is empty.
fn message(profile: &ForgeProfile, uuid: &str, header: Option<&str>, body: &str) -> Section {
    let mut md = msg_div_open(uuid, profile.provider);
    md.push_str("\n\n");
    if let Some(header) = header {
        md.push_str(header);
        md.push_str("\n\n");
    }
    let body = body.trim_end();
    if !body.is_empty() {
        md.push_str(body);
        md.push_str("\n\n");
    } else if header.is_none() {
        md.push_str("*(empty)*\n\n");
    }
    md.push_str(MSG_DIV_CLOSE);
    md.push_str("\n\n");
    Section::keyed(uuid, md)
}

/// Who wrote something: a chip where the forge gave an account, else
/// `@login` as text.
fn person(author: Option<&str>, handle: Option<&Handle>) -> String {
    let shown = author.unwrap_or("unknown");
    match handle {
        Some(h) => chip_link(shown, h),
        None => format!("@{}", escape_name_inline(shown)),
    }
}

fn header(c: &Comment) -> String {
    // A chip stands out on its own; a login as text is bolded instead.
    let who = match &c.author_handle {
        Some(_) => person(c.author.as_deref(), c.author_handle.as_ref()),
        None => format!("**{}**", person(c.author.as_deref(), None)),
    };
    let when = stamp_text_html(&c.created_at);
    let link = c
        .url
        .as_deref()
        .map(|u| format!(" {}", source_link(u)))
        .unwrap_or_default();
    let state = c
        .state
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| format!(" *({})*", escape_md_inline(s)))
        .unwrap_or_default();
    let reply = if c.in_reply_to_id.is_some() {
        " *(reply)*"
    } else {
        ""
    };
    format!("{who}{state}{reply} · {when}{link}")
}

fn yaml_opt(s: Option<&str>) -> String {
    s.map(yaml_scalar).unwrap_or_else(|| "null".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Section;
    use datalib_schema::providers::Provider;

    const MARKUP: &str = "<script>x</script> & co";

    fn profile() -> ForgeProfile {
        ForgeProfile {
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
        }
    }

    /// A login, a state, a branch and a file path are names, not markup;
    /// the description and the comments are markdown their authors wrote
    /// and stay so.
    /// A change request and one comment on it, every text field markup.
    fn sample() -> (ChangeRequest, Comment) {
        let cr = ChangeRequest {
            uuid: "u".into(),
            row_id: "r".into(),
            container: "o/repo".into(),
            number: 7,
            title: MARKUP.into(),
            body: "**Engage**".into(),
            state: Some(MARKUP.into()),
            url: None,
            head_sha: None,
            base_sha: None,
            from_ref: Some("feat/`x`".into()),
            to_ref: Some("main".into()),
            author: Some(MARKUP.into()),
            author_handle: None,
            created_at: None,
            updated_at: None,
            merged_at: None,
        };
        let comment = Comment {
            uuid: "c".into(),
            table: "t",
            row_id: "c1".into(),
            parent_row_id: "r".into(),
            kind: "k",
            entity_kind: "e",
            section: Section::Inline,
            external_id: 1,
            in_reply_to_id: None,
            author: Some(MARKUP.into()),
            author_handle: None,
            body: "*looks good*".into(),
            url: Some("https://e.invalid/c (1)".into()),
            path: Some("src/`a`.rs".into()),
            line: Some(3),
            commit_sha: None,
            created_at: "2364-04-11".into(),
            updated_at: None,
            state: Some(MARKUP.into()),
        };
        (cr, comment)
    }

    /// An author the forge gave an account for is a chip, in the change
    /// request's line and in a comment's header; the row carries it.
    #[test]
    fn an_author_with_an_account_is_a_chip() {
        let (mut cr, mut comment) = sample();
        cr.author = Some("Data".into());
        cr.author_handle = Handle::github("data-soong");
        comment.author = Some("Data".into());
        comment.author_handle = Handle::github("data-soong");
        let md = join(&render(&profile(), &cr, &[&comment]));
        let chip = "[Data](datalib:handle/github/data-soong \"Data (github:data-soong)\")";
        assert!(md.contains(&format!(" — {chip} — ")), "{md}");
        assert!(
            md.contains(&format!("\n{chip} *(")),
            "a header, not bold: {md}"
        );
    }

    #[test]
    fn a_change_request_in_markup_renders_escaped() {
        let (cr, comment) = sample();
        let md = join(&render(&profile(), &cr, &[&comment]));
        let (_, body) = md
            .split_once("---\n\n")
            .expect("front matter, then the body");
        let escaped = "&lt;script&gt;x&lt;/script&gt; &amp; co";
        assert!(!body.contains("<script>"), "{body}");
        assert!(
            body.contains(&format!(
                "*{escaped}* — @{escaped} — `` feat/`x` `` → `main`"
            )),
            "{body}"
        );
        assert!(body.contains("\n**Engage**\n"), "the description: {body}");
        assert!(body.contains("### `` src/`a`.rs:3 ``"), "{body}");
        assert!(
            body.contains(&format!(
                "**@{escaped}** *({escaped})* · <time datetime=\"2364-04-11\">Sat Apr 11th, 2364</time> \
                 <a class=\"source-link\" href=\"https://e.invalid/c (1)\" target=\"_blank\" \
                 rel=\"noopener noreferrer\">↗</a>"
            )),
            "{body}"
        );
        assert!(body.contains("\n*looks good*\n"), "a comment: {body}");
    }

    /// The description and each comment are a section of their own, in
    /// a wrapper keyed by their uuid, holding what their author wrote,
    /// HTML included: the page renders each alone, so a tag one leaves
    /// open cannot reach the next. Joined, the sections are the file.
    #[test]
    fn each_message_is_a_section_of_its_own_as_written() {
        let (mut cr, mut comment) = sample();
        cr.body = "**Engage** <div style=\"display:none\">".into();
        comment.body = "<details><summary>Logs</summary>\n\nwarp 9\n\n</details>".into();
        let sections = render(&profile(), &cr, &[&comment]);
        let keys: Vec<Option<&str>> = sections.iter().map(|s| s.uuid.as_deref()).collect();
        assert_eq!(keys, [None, Some("u"), None, Some("c")], "{sections:#?}");
        let description = &sections[1].md;
        assert!(
            description.starts_with(
                "<div id=\"m-u\" data-section-uuid=\"u\" class=\"msg msg--github\">\n"
            ),
            "{description}"
        );
        assert!(description.ends_with("\n</div>\n\n"), "{description}");
        assert!(
            description.contains("\n**Engage** <div style=\"display:none\">\n"),
            "{description}"
        );
        assert!(
            sections[3].md.contains("<details><summary>Logs</summary>"),
            "{}",
            sections[3].md
        );
    }
}
