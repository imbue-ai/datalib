//! A change request's one document: front matter, title, description,
//! then its reviews, its general conversation and its inline threads.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::title::Title;

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
    let state = cr.state.as_deref().unwrap_or("unknown");
    let author = cr.author.as_deref().unwrap_or("unknown");
    let from = cr.from_ref.as_deref().unwrap_or("?");
    let to = cr.to_ref.as_deref().unwrap_or("?");
    out.push_str(&format!("*{state}* — @{author} — `{from}` → `{to}`\n\n"));

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
        out.push_str(&format!("### `{path}:{line}`\n\n"));
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
    let who = c.author.as_deref().unwrap_or("unknown");
    let when = c.created_at.as_str();
    let link = c
        .url
        .as_deref()
        .map(|u| format!(" — [link]({u})"))
        .unwrap_or_default();
    let state = c
        .state
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| format!(" *({s})*"))
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

fn yaml_scalar(s: &str) -> String {
    if s.is_empty() {
        return "\"\"".into();
    }
    let needs_quote = s
        .chars()
        .any(|c| matches!(c, ':' | '#' | '\n' | '"' | '\''))
        || s != s.trim();
    if needs_quote {
        serde_json::to_string(s).unwrap_or_else(|_| s.into())
    } else {
        s.into()
    }
}

fn yaml_opt(s: Option<&str>) -> String {
    s.map(yaml_scalar).unwrap_or_else(|| "null".into())
}
