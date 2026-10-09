//! A change request's one document: front matter, title, description,
//! then its reviews, its general conversation and its inline threads.
//! The description and the comments are markdown their authors wrote and
//! go in as written; everything else here is plain text and is escaped.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl_render::front_matter::yaml_scalar;
use datalib_etl_render::html::{escape_md_inline, md_code_span, md_link_dest};
use datalib_etl_render::title::Title;

use crate::{ordered, ChangeRequest, Comment, ForgeProfile};

pub(crate) fn write(
    profile: &ForgeProfile,
    cr: &ChangeRequest,
    comments: &[&Comment],
    md_path: &Path,
) -> Result<()> {
    if let Some(dir) = md_path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let out = render(profile, cr, comments);
    fs::write(md_path, &out).with_context(|| format!("write {}", md_path.display()))
}

fn render(profile: &ForgeProfile, cr: &ChangeRequest, comments: &[&Comment]) -> String {
    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("provider: {}\n", profile.tag));
    let front: [(&str, String); 12] = [
        (profile.container_key, yaml_scalar(&cr.container)),
        (profile.number_key, cr.number.to_string()),
        ("title", yaml_scalar(&cr.title)),
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
    let author = escape_md_inline(cr.author.as_deref().unwrap_or("unknown"));
    let from = md_code_span(cr.from_ref.as_deref().unwrap_or("?"));
    let to = md_code_span(cr.to_ref.as_deref().unwrap_or("?"));
    out.push_str(&format!("*{state}* — @{author} — {from} → {to}\n\n"));

    out.push_str("## Description\n\n");
    if cr.body.trim().is_empty() {
        out.push_str("*(no description)*\n\n");
    } else {
        out.push_str(&cr.body);
        if !cr.body.ends_with('\n') {
            out.push('\n');
        }
        out.push('\n');
    }

    let ordered = ordered(comments);
    if profile.reviews {
        out.push_str("## Reviews\n\n");
        if ordered.reviews.is_empty() {
            out.push_str("*(no reviews)*\n\n");
        }
        for r in &ordered.reviews {
            out.push_str(&header(r));
            out.push_str("\n\n");
            // A review with no body is a bare approval: its header says
            // everything.
            if !r.body.trim().is_empty() {
                out.push_str(&quote_body(&r.body));
                out.push_str("\n\n");
            }
        }
    }

    out.push_str("## General discussion\n\n");
    if ordered.general.is_empty() {
        out.push_str("*(no general comments)*\n\n");
    }
    for c in &ordered.general {
        push_comment(&mut out, c);
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
            push_comment(&mut out, c);
        }
    }

    while out.ends_with("\n\n") {
        out.pop();
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn push_comment(out: &mut String, c: &Comment) {
    out.push_str(&header(c));
    out.push_str("\n\n");
    out.push_str(&quote_body(&c.body));
    out.push_str("\n\n");
}

fn header(c: &Comment) -> String {
    let who = escape_md_inline(c.author.as_deref().unwrap_or("unknown"));
    let when = escape_md_inline(&c.created_at);
    let link = c
        .url
        .as_deref()
        .map(|u| format!(" — [link]({})", md_link_dest(u)))
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
    format!("**@{who}**{state}{reply} @ {when}{link}")
}

/// Each line prefixed `> `, so the comment renders as a blockquote.
fn quote_body(body: &str) -> String {
    if body.is_empty() {
        return "> *(empty)*".into();
    }
    body.lines()
        .map(|l| {
            if l.is_empty() {
                ">".to_string()
            } else {
                format!("> {l}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
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
            dir_prefix: "pr-",
            container_needs_owner: true,
            reviews: true,
            render_version: 1,
        }
    }

    /// A login, a state, a branch and a file path are names, not markup;
    /// the description and the comments are markdown their authors wrote
    /// and stay so.
    #[test]
    fn a_change_request_in_markup_renders_escaped() {
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
            body: "*looks good*".into(),
            url: Some("https://e.invalid/c (1)".into()),
            path: Some("src/`a`.rs".into()),
            line: Some(3),
            commit_sha: None,
            created_at: "2364-04-11".into(),
            updated_at: None,
            state: Some(MARKUP.into()),
        };
        let md = render(&profile(), &cr, &[&comment]);
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
                "**@{escaped}** *({escaped})* @ 2364-04-11 — [link](<https://e.invalid/c (1)>)"
            )),
            "{body}"
        );
        assert!(body.contains("> *looks good*"), "a comment: {body}");
    }
}
