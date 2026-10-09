//! Writes one JSON file for `//datalib/ui:unit_test`'s
//! `hostile_text.test.ts`, which renders it through the app's own
//! markdown-it and sanitizer: the markdown every shared renderer makes
//! when each plain-text field holds a string of HTML and markdown, and
//! the escape helpers' output for a few hundred generated strings.
//! The page is the judge of whether text reads as typed, so the
//! judging happens there; this only produces the markdown.
//!
//! It reaches the renderers through their public entry points:
//! chat-common, calendar-common, contact-common, forge-render-common,
//! Slack's mrkdwn conversion, `MessageHeader` and `Title`. A provider's
//! own private text paths (Facebook posts, Claude's tool results, a
//! time-series page's facts) are not reached here and keep their own
//! tests.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use datalib_contact_schema::{ContactKind, Detail, NormalizedContact};
use datalib_etl::progress::Progress;
use datalib_etl_calendar_common::types::{
    Attendee, EventLink, EventShape, NormalizedEvent, OccurrenceRef, Person,
};
use datalib_etl_calendar_common::{CalendarRenderProfile, EventTime};
use datalib_etl_chat_common::render::ENTITY_KIND_CONVERSATION;
use datalib_etl_chat_common::types::{OrphanReactions, Recipient, RecipientRole};
use datalib_etl_chat_common::{
    ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem, NormalizedDoc,
    NormalizedReaction, RecordStampPrecision, RenderProfile, TextFormat,
};
use datalib_etl_contact_common::{ContactDoc, ContactRenderProfile};
use datalib_etl_forge_render_common::{ChangeRequest, Comment, ForgeProfile, Parsed};
use datalib_etl_render::html;
use datalib_etl_render::inputs::Inputs;
use datalib_etl_render::message::MessageHeader;
use datalib_etl_render::title::Title;
use datalib_etl_slack_render::render::mrkdwn::{to_commonmark, Labels};
use datalib_schema::providers::Provider;
use serde::Serialize;

/// HTML and markdown in one string, tagged so each field's copy can be
/// told apart on the page: a tag, a link, an image, a table cell, a
/// code span, an entity, a blank line that would end an HTML block, and
/// a rule that would underline the line above into a heading. Two
/// characters of `tag` keep it under `Title`'s clamp.
fn hostile(tag: &str) -> String {
    format!(
        "<b>{tag}</b> [{tag}](https://e.test) ![{tag}](https://t.test/i.png) {tag}|b `{tag}` \
         &amp;{tag}\n\n<i>{tag}</i>\n---\n{tag}."
    )
}

/// What Slack sends for someone typing [`hostile`] — `&`, `<` and `>`
/// as entities — less the code span, which is Slack's own markup.
fn slack_typed(tag: &str) -> String {
    hostile(tag).replace(&format!("`{tag}` "), "")
}

fn slack_encoded(typed: &str) -> String {
    typed
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[derive(Serialize)]
struct Corpus {
    documents: Vec<Document>,
    escapes: Vec<Escape>,
}

#[derive(Serialize)]
struct Document {
    name: String,
    md: String,
    /// Each field whose text must read on the page exactly as typed.
    expect: Vec<Expect>,
}

#[derive(Serialize)]
struct Expect {
    field: &'static str,
    typed: String,
}

#[derive(Serialize)]
struct Escape {
    helper: &'static str,
    input: String,
    md: String,
}

/// Field name → tag, so a failure on the page names the field.
#[derive(Default)]
struct Fields(Vec<(&'static str, String)>);

impl Fields {
    fn tag(&mut self, field: &'static str) -> String {
        let n = self.0.len();
        let tag = format!(
            "{}{}",
            char::from(b'a' + (n / 10) as u8),
            char::from(b'0' + (n % 10) as u8)
        );
        self.0.push((field, tag.clone()));
        tag
    }

    fn hostile(&mut self, field: &'static str) -> String {
        hostile(&self.tag(field))
    }

    fn expect(&self, fields: &[&'static str]) -> Vec<Expect> {
        fields
            .iter()
            .map(|f| {
                let (_, tag) = self
                    .0
                    .iter()
                    .find(|(name, _)| name == f)
                    .unwrap_or_else(|| panic!("no field {f}"));
                Expect {
                    field: f,
                    typed: hostile(tag),
                }
            })
            .collect()
    }
}

// Writing the corpus path is this tool's output; there is no log.
#[allow(clippy::disallowed_macros)]
fn main() -> Result<()> {
    let Some(out) = std::env::args().nth(1) else {
        bail!("usage: render_hostile_samples <out.json>");
    };
    let scratch = tempfile::tempdir()?;
    let mut documents = Vec::new();
    documents.extend(header_and_title());
    documents.extend(chat_common(scratch.path())?);
    documents.extend(calendar(scratch.path())?);
    documents.extend(contacts(scratch.path())?);
    documents.extend(forge(scratch.path())?);
    documents.extend(slack());
    let corpus = Corpus {
        documents,
        escapes: escapes(),
    };
    std::fs::write(&out, serde_json::to_string_pretty(&corpus)?)
        .with_context(|| format!("write {out}"))?;
    println!("{out}");
    Ok(())
}

fn header_and_title() -> Vec<Document> {
    let mut f = Fields::default();
    let author = f.hostile("message header author");
    let header = MessageHeader {
        author: &author,
        handle: None,
        date_ms: Some(12_602_794_200_000),
        source_url: None,
    }
    .render();
    let text = f.hostile("title text");
    let suffix = f.hostile("title suffix");
    let title = Title {
        text: &text,
        suffix: Some(&suffix),
        markdown_uuid: Some("00000000-0000-8000-8000-000000000001"),
        source_url: None,
    }
    .render();
    vec![
        Document {
            name: "MessageHeader".into(),
            md: format!("{header}\n"),
            expect: f.expect(&["message header author"]),
        },
        Document {
            name: "Title".into(),
            md: title,
            expect: f.expect(&["title text", "title suffix"]),
        },
    ]
}

fn read_docs(paths: &[PathBuf], root: &Path, family: &str) -> Result<Vec<(String, String)>> {
    paths
        .iter()
        .map(|p| {
            let md = std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?;
            let rel = p.strip_prefix(root).unwrap_or(p).display().to_string();
            Ok((format!("{family}: {rel}"), md))
        })
        .collect()
}

fn chat_common(scratch: &Path) -> Result<Vec<Document>> {
    let mut f = Fields::default();
    let profile = RenderProfile {
        provider: Provider::Signal,
        source_label: "Signal".to_string(),
        chat_kind: "Chat".to_string(),
        message_kind: "Message".to_string(),
        reaction_kind: "Reaction".to_string(),
        chat_entity_kind: ENTITY_KIND_CONVERSATION,
        stamp_precision: RecordStampPrecision::Seconds,
        render_version: 1,
        text_format: TextFormat::Plain,
    };
    let item = |uuid: &str, author: String, text: Option<String>| NormalizedChatItem {
        message_uuid: uuid.to_string(),
        author_handle: None,
        author_display: author,
        date_ms: Some(12_602_794_200_000),
        text,
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
        recipients: Vec::new(),
        mentions: Vec::new(),
        problems: Vec::new(),
    };
    let reaction = |uuid: &str, emoji: String, who: String| NormalizedReaction {
        reaction_uuid: uuid.to_string(),
        reactor_handle: None,
        reactor_display: who,
        emoji,
        date_ms: Some(12_602_794_200_000),
        source_ref: None,
    };
    let attachment = |rel: Option<&str>, name: String, url: Option<String>| NormalizedAttachment {
        rel_path: rel.map(str::to_string),
        file_name: Some(name),
        mime_type: Some("application/octet-stream".into()),
        byte_len: Some(1701),
        source_url: url,
        ref_id: None,
    };

    let mut said = item(
        "m1",
        f.hostile("item author"),
        Some(f.hostile("item text (plain)")),
    );
    said.labels = vec![f.hostile("item label")];
    said.recipients = vec![Recipient {
        role: RecipientRole::To,
        display: f.hostile("recipient"),
        handle: None,
    }];
    said.reactions = vec![reaction(
        "r1",
        f.hostile("reaction emoji"),
        f.hostile("reactor"),
    )];
    let mut system = item("m2", String::new(), None);
    system.kind = ItemKind::System;
    system.system_note = Some(f.hostile("system note"));
    let mut files = item("m3", "Data".into(), Some(f.hostile("attachment caption")));
    files.kind = ItemKind::Attachment;
    files.attachments = vec![
        attachment(Some("blobs/a.bin"), f.hostile("attachment name"), None),
        attachment(
            None,
            f.hostile("unfetched attachment name"),
            Some(f.hostile("unfetched attachment url")),
        ),
    ];
    let chat = NormalizedChat {
        contacts: Vec::new(),
        inputs: Vec::new(),
        id: "hostile-chat".into(),
        chat_uuid: "00000000-0000-8000-8000-0000000000c1".into(),
        display: f.hostile("chat display"),
        title: Some(f.hostile("chat title")),
        author: None,
        account: Some(f.hostile("chat account")),
        project: Some(f.hostile("chat project")),
        external_id: Some(f.hostile("chat external id")),
        upstream_account: None,
        source_url: None,
        org_uuid: None,
        org_name: None,
        path_prefix: None,
        buckets: vec![NormalizedDoc {
            period_key: "all".to_string(),
            markdown_uuid: "00000000-0000-8000-8000-0000000000d1".into(),
            source_ref: None,
            items: vec![said, system, files],
            orphan_reactions: vec![OrphanReactions {
                target_native_id: f.hostile("orphan target id"),
                reactions: vec![reaction(
                    "r2",
                    f.hostile("orphan reaction emoji"),
                    f.hostile("orphan reactor"),
                )],
            }],
        }],
    };
    let out = scratch.join("chat");
    let mut paths = Vec::new();
    datalib_etl_chat_common::render_all(
        &profile,
        &[chat],
        &out,
        "hostile",
        &HashMap::new(),
        &Progress::noop(),
        &mut |doc| {
            paths.push(doc.md_path);
            Ok(())
        },
    )?;
    let expect = [
        "item author",
        "item text (plain)",
        "item label",
        "recipient",
        "reaction emoji",
        "reactor",
        "system note",
        "attachment caption",
        "attachment name",
        "unfetched attachment name",
        "unfetched attachment url",
        "chat title",
        "orphan target id",
        "orphan reaction emoji",
        "orphan reactor",
    ];
    Ok(read_docs(&paths, &out, "chat-common")?
        .into_iter()
        .map(|(name, md)| Document {
            name,
            md,
            expect: f.expect(&expect),
        })
        .collect())
}

fn calendar(scratch: &Path) -> Result<Vec<Document>> {
    let mut f = Fields::default();
    let at = |v: &str| EventTime::from_ical(v, Some("America/Los_Angeles")).expect("a time");
    let event = NormalizedEvent {
        event_uuid: "00000000-0000-8000-8000-0000000000e1".into(),
        shape: EventShape::Series {
            rules: vec!["FREQ=WEEKLY;BYDAY=MO".into()],
            rdates: Vec::new(),
            cancelled: Vec::new(),
            changed: vec![OccurrenceRef {
                uuid: "00000000-0000-8000-8000-0000000000e2".into(),
                original_start: at("20260112T090000"),
                start: Some(at("20260112T110000")),
                title: Some(f.hostile("changed occurrence title")),
            }],
        },
        calendar_uuid: "00000000-0000-8000-8000-0000000000e3".into(),
        calendar_label: f.hostile("calendar label"),
        calendar_time_zone: Some("America/Los_Angeles".into()),
        upstream_id: f.hostile("event upstream id"),
        upstream_entity_kind: "event",
        title: Some(f.hostile("event title")),
        start: Some(at("20260105T090000")),
        end: Some(at("20260105T100000")),
        status: Some("confirmed".into()),
        busy: Some(true),
        location: Some(f.hostile("event location")),
        description: Some(f.hostile("event description")),
        organizer: Some(Person {
            name: Some(f.hostile("organizer name")),
            email: Some("picard@enterprise.test".into()),
        }),
        attendees: vec![Attendee {
            person: Person {
                name: Some(f.hostile("attendee name")),
                email: Some("troi@enterprise.test".into()),
            },
            response: Some("tentative".into()),
            optional: false,
            resource: false,
        }],
        links: vec![EventLink {
            label: f.hostile("event link label"),
            url: "https://x.test/join".into(),
        }],
        source_url: None,
        created: Some(f.hostile("event created")),
        modified_at: Some("2026-09-02T16:00:00+00:00".into()),
        inputs: Vec::new(),
        problems: Vec::new(),
    };
    let profile = CalendarRenderProfile {
        provider: Provider::Calendar,
        source_label: "Calendar".into(),
        account: None,
        render_version: 1,
    };
    let out = scratch.join("calendar");
    let mut paths = Vec::new();
    datalib_etl_calendar_common::render_all(
        &profile,
        &[event],
        &out,
        "hostile",
        &Progress::noop(),
        &mut |doc| {
            paths.push(doc.md_path);
            Ok(())
        },
    )?;
    let expect = [
        "changed occurrence title",
        "event title",
        "event location",
        "event description",
        "organizer name",
        "attendee name",
        "event link label",
    ];
    Ok(read_docs(&paths, &out, "calendar-common")?
        .into_iter()
        .map(|(name, md)| Document {
            name,
            md,
            expect: f.expect(&expect),
        })
        .collect())
}

fn contacts(scratch: &Path) -> Result<Vec<Document>> {
    let mut f = Fields::default();
    let mut contact =
        NormalizedContact::new("hostile", f.hostile("contact key"), ContactKind::Person);
    contact.names = vec![f.hostile("contact name")];
    contact.org = Some(f.hostile("contact org"));
    contact.title = Some(f.hostile("contact title"));
    contact.note = Some(f.hostile("contact note"));
    contact.details = vec![Detail::new(
        f.hostile("detail label"),
        f.hostile("detail value"),
    )];
    contact.members = vec![f.hostile("contact member")];
    contact.created_at = Some("2024-01-02T00:00:00+00:00".into());
    let doc = ContactDoc {
        contact,
        doc_uuid: "00000000-0000-8000-8000-0000000000f1".into(),
        group_uuid: "00000000-0000-8000-8000-0000000000f2".into(),
        group_label: f.hostile("contact group"),
        upstream_account: None,
        inputs: Vec::new(),
    };
    let profile = ContactRenderProfile {
        provider: Provider::Contacts,
        source_label: "Contacts".into(),
        contact_kind: "Contact".into(),
        contact_entity_kind: "contact",
        account: None,
        render_version: 1,
    };
    let out = scratch.join("contacts");
    let mut paths = Vec::new();
    datalib_etl_contact_common::render_all(
        &profile,
        &[doc],
        &out,
        "hostile",
        &Progress::noop(),
        &mut |doc| {
            paths.push(doc.md_path);
            Ok(())
        },
    )?;
    let expect = [
        "contact name",
        "contact org",
        "contact title",
        "contact note",
        "detail label",
        "detail value",
        "contact member",
    ];
    Ok(read_docs(&paths, &out, "contact-common")?
        .into_iter()
        .map(|(name, md)| Document {
            name,
            md,
            expect: f.expect(&expect),
        })
        .collect())
}

fn forge(scratch: &Path) -> Result<Vec<Document>> {
    let mut f = Fields::default();
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
        dir_prefix: "pr-",
        container_needs_owner: true,
        reviews: true,
        render_version: 1,
    };
    let cr = ChangeRequest {
        uuid: "00000000-0000-8000-8000-000000000101".into(),
        row_id: "r".into(),
        container: "o/repo".into(),
        number: 7,
        title: f.hostile("change request title"),
        // Authored markdown, shown as markdown.
        body: "**Engage**".into(),
        state: Some(f.hostile("change request state")),
        url: None,
        head_sha: Some(f.hostile("head sha")),
        base_sha: None,
        from_ref: Some(f.hostile("from ref")),
        to_ref: Some(f.hostile("to ref")),
        author: Some(f.hostile("change request author")),
        created_at: Some(f.hostile("created at")),
        updated_at: None,
        merged_at: None,
    };
    let comment = Comment {
        uuid: "00000000-0000-8000-8000-000000000102".into(),
        table: "review_comments",
        row_id: "c1".into(),
        parent_row_id: "r".into(),
        kind: "Review Comment",
        entity_kind: "review_comment",
        section: datalib_etl_forge_render_common::Section::Inline,
        external_id: 1,
        in_reply_to_id: None,
        author: Some(f.hostile("comment author")),
        body: "*looks good*".into(),
        url: Some("https://x.test/c".into()),
        path: Some(f.hostile("comment path")),
        line: Some(3),
        commit_sha: None,
        created_at: f.hostile("comment created at"),
        updated_at: None,
        state: Some(f.hostile("comment state")),
    };
    let parsed = Parsed {
        change_requests: vec![cr],
        comments: vec![comment],
        ..Default::default()
    };
    let out = scratch.join("forge");
    let mut paths = Vec::new();
    datalib_etl_forge_render_common::render_all(
        &profile,
        &parsed,
        &out,
        "hostile",
        &Progress::noop(),
        &mut |doc| {
            paths.push(doc.md_path);
            Ok(())
        },
    )?;
    let expect = [
        "change request title",
        "change request state",
        "from ref",
        "to ref",
        "change request author",
        "comment author",
        "comment path",
        "comment created at",
        "comment state",
    ];
    Ok(read_docs(&paths, &out, "forge-render-common")?
        .into_iter()
        .map(|(name, md)| Document {
            name,
            md,
            expect: f.expect(&expect),
        })
        .collect())
}

fn slack() -> Vec<Document> {
    let mut f = Fields::default();
    let body = f.tag("slack text");
    let user = f.tag("slack user name");
    let label = f.tag("slack mention label");
    let channel = f.tag("slack channel name");
    let link = f.tag("slack link label");
    let users = BTreeMap::from([("U1".to_string(), hostile(&user))]);
    let channels = BTreeMap::from([("C1".to_string(), hostile(&channel))]);
    let inputs = Inputs::default();
    // A workspace id, so each mention is a chip link: the hostile name
    // then rides in a link's text and its title too.
    let labels = Labels {
        users: inputs.lookup("users", &users),
        channels: inputs.lookup("channels", &channels),
        team_id: "T1",
    };
    let text = format!(
        "{}\nby <@U1> and <@U2|{}> in <#C1>, see <https://x.test/a|{}>",
        slack_encoded(&slack_typed(&body)),
        slack_encoded(&slack_typed(&label)),
        slack_encoded(&slack_typed(&link)),
    );
    let expect = vec![
        Expect {
            field: "slack text",
            typed: slack_typed(&body),
        },
        Expect {
            field: "slack user name",
            typed: hostile(&user),
        },
        Expect {
            field: "slack mention label",
            typed: slack_typed(&label),
        },
        Expect {
            field: "slack channel name",
            typed: hostile(&channel),
        },
        Expect {
            field: "slack link label",
            typed: slack_typed(&link),
        },
    ];
    vec![Document {
        name: "slack to_commonmark".into(),
        md: to_commonmark(&text, labels),
        expect,
    }]
}

/// Strings from an alphabet of every character markdown or HTML gives a
/// meaning to, each run through the helpers in the context it is meant
/// for. Deterministic, so a failure reproduces.
fn escapes() -> Vec<Escape> {
    const ANY: &[char] = &[
        'a', 'b', 'm', 'p', '1', ' ', ' ', '\t', '\n', '\r', '<', '>', '&', '"', '\'', '`', '*',
        '_', '[', ']', '(', ')', '!', '#', '-', '+', '=', '|', '\\', '~', ':', '/', '.', ';', '{',
        '}', '%', '$',
    ];
    let mut rng = XorShift(0x5eed_1701_d00d_cafe);
    let mut out = Vec::new();
    for _ in 0..300 {
        let s = rng.string(ANY, 24);
        out.push(Escape {
            helper: "escape_md_inline",
            md: format!("p: {}", html::escape_md_inline(&s)),
            input: s.clone(),
        });
        out.push(Escape {
            helper: "escape_text",
            md: format!("<div class=\"t\">{}</div>", html::escape_text(&s)),
            input: s.clone(),
        });
        out.push(Escape {
            helper: "escape_attr",
            md: format!(
                "p: <time class=\"msg-ts\" title=\"{}\">x</time>",
                html::escape_attr(&s)
            ),
            input: s.clone(),
        });
        if !s.is_empty() {
            out.push(Escape {
                helper: "md_code_span",
                md: format!("p: {}", html::md_code_span(&s)),
                input: s.clone(),
            });
        }
        // A block keeps a person's emphasis on purpose, so its strings
        // leave out the two characters that make it.
        let block: String = s.chars().filter(|c| !matches!(c, '*' | '_')).collect();
        out.push(Escape {
            helper: "escape_md_block",
            md: html::escape_md_block(&block),
            input: block,
        });
    }
    out
}

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn string(&mut self, alphabet: &[char], max_len: u64) -> String {
        let len = self.next() % (max_len + 1);
        (0..len)
            .map(|_| alphabet[(self.next() % alphabet.len() as u64) as usize])
            .collect()
    }
}
