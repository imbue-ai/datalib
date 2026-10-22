//! Google Chat: a space or a DM is a conversation, a page per month. A
//! message draws what its `messages.json` entry carries — text, the
//! message it quotes, its thread, link and Drive previews, a call, its
//! attachments, reactions and labels — with its earlier versions folded
//! in above it. Every key this render does not read is a problem row on
//! the message, by shape; so are the keys of `group_info.json` and
//! `user_info.json`, on the conversation's first message.

use std::collections::{BTreeMap, HashMap, HashSet};

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl_chat_common::findings::{noted, shape_of, unread_keys};
use datalib_etl_chat_common::types::{
    own_stamp_ms, AuthorKind, ItemKind, NamedPerson, NormalizedAttachment, NormalizedChat,
    NormalizedChatItem, NormalizedDoc, NormalizedReaction, UpstreamRef,
};
use datalib_etl_google_takeout::ingest::schema_raw::ChatAttachmentRow;
use datalib_etl_render::html::{escape_md_block, escape_md_inline, md_link_dest};
use datalib_etl_render::inputs::Inputs;
use datalib_handle::Handle;
use datalib_schema::problems::{Problem, Reason, Severity};
use serde_json::Value;

use crate::ids;
use crate::render::{mime_of, month_of};

const MESSAGE_KEYS: &[&str] = &[
    "message_id",
    "topic_id",
    "creator",
    "created_date",
    "updated_date",
    "text",
    "annotations",
    "attached_files",
    "previous_message_versions",
    "quoted_message_metadata",
    "reactions",
    "message_labels",
];
const PERSON_KEYS: &[&str] = &["name", "email", "user_type"];
const CREATOR_KEYS: &[&str] = &["name", "email", "user_type", "acting_user"];
const ACTING_USER_KEYS: &[&str] = &["name", "user_id", "user_type"];
const VERSION_KEYS: &[&str] = &[
    "text",
    "created_date",
    "updated_date",
    "annotations",
    "attached_files",
];
const QUOTE_KEYS: &[&str] = &["creator", "text", "annotations", "attached_files"];
const FILE_KEYS: &[&str] = &["export_name", "original_name"];
const REACTION_KEYS: &[&str] = &["emoji", "reactor_emails"];
const LABEL_KEYS: &[&str] = &["label_type", "creator", "sort_value"];
/// `start_index` and `length` place an annotation in the text, which the
/// text alone already shows.
const ANNOTATION_KEYS: &[&str] = &[
    "start_index",
    "length",
    "url_metadata",
    "drive_metadata",
    "youtube_metadata",
    "format_metadata",
    "gsuite_integration_metadata",
    "video_call_metadata",
];
/// A preview's picture and blurb are the linked page's, not anything
/// typed here; its title names the link.
const URL_KEYS: &[&str] = &["url", "title", "snippet", "image_url"];
const DRIVE_KEYS: &[&str] = &["id", "title", "thumbnail_url"];
const YOUTUBE_KEYS: &[&str] = &["id", "start_time"];
/// Bold, a colour: how the text looked, which a page of plain text drops.
const FORMAT_KEYS: &[&str] = &["format_type", "font_color"];
const GROUP_KEYS: &[&str] = &["members", "name"];
const USER_KEYS: &[&str] = &[
    "user",
    "membership_info",
    "starred_group_id",
    "starred_message_ids",
];
const MEMBERSHIP_KEYS: &[&str] = &["group_id", "group_name", "membership_state"];

/// The account's own `user_info.json`: who it is, the messages it starred.
struct Account<'a> {
    rows: &'a [(String, Value)],
    starred: HashSet<&'a str>,
    /// Space id → (membership state, starred).
    spaces: HashMap<String, (Option<&'a str>, bool)>,
}

impl<'a> Account<'a> {
    fn new(rows: &'a [(String, Value)]) -> Self {
        let mut starred = HashSet::new();
        let mut spaces: HashMap<String, (Option<&str>, bool)> = HashMap::new();
        for (_, u) in rows {
            let ids = u.get("starred_message_ids").and_then(Value::as_array);
            starred.extend(ids.into_iter().flatten().filter_map(Value::as_str));
            let groups = u.get("membership_info").and_then(Value::as_array);
            for g in groups.into_iter().flatten() {
                if let Some(id) = g.get("group_id").and_then(Value::as_str) {
                    let state = g.get("membership_state").and_then(Value::as_str);
                    spaces.entry(space_of_dir(id)).or_default().0 = state;
                }
            }
            let stars = u.get("starred_group_id").and_then(Value::as_array);
            for id in stars.into_iter().flatten().filter_map(Value::as_str) {
                spaces.entry(space_of_dir(id)).or_default().1 = true;
            }
        }
        Self {
            rows,
            starred,
            spaces,
        }
    }

    /// What `user_info.json` holds that nothing reads, filed once.
    fn findings(&self) -> Vec<Problem> {
        let mut out = Vec::new();
        for (_, u) in self.rows {
            out.extend(unread_keys(u, USER_KEYS, "/user_info"));
            if let Some(user) = u.get("user") {
                out.extend(unread_keys(user, PERSON_KEYS, "/user_info/user"));
            }
            let groups = u.get("membership_info").and_then(Value::as_array);
            for (i, g) in groups.into_iter().flatten().enumerate() {
                out.extend(unread_keys(
                    g,
                    MEMBERSHIP_KEYS,
                    &format!("/user_info/membership_info/{i}"),
                ));
            }
        }
        out
    }

    /// What the account's membership says of one space.
    fn space_findings(&self, space: &str) -> Vec<Problem> {
        let Some((state, starred)) = self.spaces.get(space) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if let Some(state) = state.filter(|s| *s != "MEMBER_JOINED") {
            let shown = if is_enum_word(state) {
                state
            } else {
                "another"
            };
            out.push(noted(
                "membership_state",
                &format!("the account's membership of this conversation is {shown}"),
            ));
        }
        if *starred {
            out.push(noted(
                "starred_group_id",
                "the account starred this conversation",
            ));
        }
        out
    }
}

/// `MEMBER_JOINED`, `CALL_ENDED`: Google's own word for a state, safe to
/// show where a value typed by a person would not be.
fn is_enum_word(s: &str) -> bool {
    !s.is_empty() && s.len() <= 40 && s.bytes().all(|b| b.is_ascii_uppercase() || b == b'_')
}

/// `CALL_ENDED` → `Call ended`.
fn enum_words(s: &str) -> String {
    let lower = s.replace('_', " ").to_lowercase();
    let mut chars = lower.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Who a space's members are, by email, as `group_info.json` names them.
type Members = HashMap<String, String>;

struct Space<'a> {
    dir: Option<&'a str>,
    group: Option<&'a Value>,
    messages: Vec<(&'a str, &'a Value)>,
}

/// Messages as `(row id, payload)`, groups as `(dir name, payload)`,
/// users as `(dir name, payload)`; the ids are what each space declares
/// it read. Every space reads every user row, since a star there lands
/// on one of its messages.
pub fn build(
    source_id: &str,
    messages: &[(String, Value)],
    groups: &[(String, Value)],
    users: &[(String, Value)],
) -> Vec<NormalizedChat> {
    let account = Account::new(users);
    let mut spaces: BTreeMap<String, Space> = BTreeMap::new();
    for (dir, g) in groups {
        spaces
            .entry(space_of_dir(dir))
            .or_insert_with(|| Space {
                dir: None,
                group: None,
                messages: Vec::new(),
            })
            .dir = Some(dir);
        if let Some(s) = spaces.get_mut(&space_of_dir(dir)) {
            s.group = Some(g);
        }
    }
    for (row_id, m) in messages {
        let space = space_of(m.get("message_id").and_then(Value::as_str).unwrap_or(""));
        spaces
            .entry(space)
            .or_insert_with(|| Space {
                dir: None,
                group: None,
                messages: Vec::new(),
            })
            .messages
            .push((row_id, m));
    }

    let mut user_findings = Some(account.findings());
    let mut chats = Vec::new();
    for (space, s) in spaces {
        if s.messages.is_empty() {
            continue;
        }
        let mut chat = conversation(source_id, &space, &s, &account);
        // What `user_info.json` leaves unread goes on the first
        // conversation, once.
        if let Some(found) = user_findings.take() {
            if let Some(first) = chat.buckets.first_mut().and_then(|b| b.items.first_mut()) {
                first.problems.extend(found);
            }
        }
        chats.push(chat);
    }
    chats
}

fn conversation(source_id: &str, space: &str, s: &Space, account: &Account) -> NormalizedChat {
    let mut inputs = Inputs::default();
    for (user_dir, _) in account.rows {
        inputs.read("chat_users", user_dir);
    }
    let members: Vec<NamedPerson> = s
        .group
        .and_then(|g| g.get("members"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            Some(NamedPerson {
                shown: m.get("name").and_then(Value::as_str)?.to_string(),
                handle: m
                    .get("email")
                    .and_then(Value::as_str)
                    .and_then(Handle::email),
            })
        })
        .fold(Vec::new(), |mut acc, p| {
            if !acc.contains(&p) {
                acc.push(p);
            }
            acc
        });
    let by_email: Members = s
        .group
        .and_then(|g| g.get("members"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            Some((
                m.get("email")?.as_str()?.to_lowercase(),
                m.get("name")?.as_str()?.to_string(),
            ))
        })
        .collect();

    // The first message of each thread, by time, to say whom a reply in
    // it answers.
    let mut ordered: Vec<(Option<i64>, &str, &Value)> = s
        .messages
        .iter()
        .map(|(row_id, m)| (created_ms(m), *row_id, *m))
        .collect();
    ordered.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    let mut thread_starter: HashMap<&str, &Value> = HashMap::new();
    for (_, _, m) in &ordered {
        if let Some(topic) = m.get("topic_id").and_then(Value::as_str) {
            thread_starter.entry(topic).or_insert(m);
        }
    }

    let mut by_month: BTreeMap<String, Vec<NormalizedChatItem>> = BTreeMap::new();
    for (_, row_id, m) in &ordered {
        inputs.read("chat_messages", row_id);
        let starter = m
            .get("topic_id")
            .and_then(Value::as_str)
            .and_then(|t| thread_starter.get(t))
            .filter(|first| !std::ptr::eq(**first, *m));
        let ctx = Ctx {
            source_id,
            message_id: m
                .get("message_id")
                .and_then(Value::as_str)
                .unwrap_or(row_id),
            members: &by_email,
            account,
        };
        let items = message_items(&ctx, m, starter.copied(), &mut inputs);
        let month = month_of(items.last().and_then(|i| i.date_ms));
        by_month.entry(month).or_default().extend(items);
    }

    let buckets: Vec<NormalizedDoc> = by_month
        .into_iter()
        .map(|(period_key, items)| {
            let month = ids::space_month(source_id, space, &period_key);
            NormalizedDoc {
                orphan_reactions: Vec::new(),
                markdown_uuid: month.uuid,
                source_ref: Some(UpstreamRef::new(month.entity_kind, month.natural_key)),
                period_key,
                items,
            }
        })
        .collect();

    let mut findings = account.space_findings(space);
    if let Some(dir) = s.dir {
        inputs.read("chat_groups", dir);
    }
    if let Some(g) = s.group {
        findings.extend(unread_keys(g, GROUP_KEYS, "/group_info"));
        let listed = g.get("members").and_then(Value::as_array);
        for (i, m) in listed.into_iter().flatten().enumerate() {
            findings.extend(unread_keys(
                m,
                PERSON_KEYS,
                &format!("/group_info/members/{i}"),
            ));
        }
    }
    let mut buckets = buckets;
    if let Some(first) = buckets.first_mut().and_then(|b| b.items.first_mut()) {
        first.problems.extend(findings);
    }

    // A named space is titled; a DM is named after its people.
    let name = s
        .group
        .and_then(|g| g.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|n| !n.is_empty());
    let (display, named_after) = match name {
        Some(n) => (n.to_string(), Vec::new()),
        None if !members.is_empty() => {
            let names: Vec<&str> = members.iter().map(|m| m.shown.as_str()).collect();
            (names.join(", "), members)
        }
        None => (space.to_string(), Vec::new()),
    };

    let chat_id = ids::space(source_id, space);
    NormalizedChat {
        named_after,
        contacts: Vec::new(),
        inputs: inputs.declared(),
        path_prefix: None,
        id: space.to_string(),
        chat_uuid: chat_id.uuid,
        display,
        author: None,
        account: None,
        project: None,
        external_id: Some(chat_id.natural_key),
        source_url: None,
        upstream_account: None,
        title: name.map(str::to_string),
        org_uuid: None,
        org_name: None,
        buckets,
    }
}

struct Ctx<'a> {
    source_id: &'a str,
    message_id: &'a str,
    members: &'a Members,
    account: &'a Account<'a>,
}

impl Ctx<'_> {
    /// A member's name for an email the group lists, else the email.
    fn name_of(&self, email: &str) -> String {
        self.members
            .get(&email.to_lowercase())
            .cloned()
            .unwrap_or_else(|| email.to_string())
    }
}

fn created_ms(m: &Value) -> Option<i64> {
    m.get("created_date")
        .and_then(Value::as_str)
        .and_then(parse_date_ms)
}

/// The message's earlier versions, oldest first, then the message.
fn message_items(
    ctx: &Ctx,
    m: &Value,
    thread_starter: Option<&Value>,
    inputs: &mut Inputs,
) -> Vec<NormalizedChatItem> {
    let mut problems = unread_keys(m, MESSAGE_KEYS, "/message");
    let creator = m.get("creator");
    if let Some(c) = creator {
        problems.extend(unread_keys(c, CREATOR_KEYS, "/message/creator"));
        if let Some(a) = c.get("acting_user") {
            problems.extend(unread_keys(
                a,
                ACTING_USER_KEYS,
                "/message/creator/acting_user",
            ));
        }
    }
    let name = creator.and_then(|c| c.get("name")).and_then(Value::as_str);
    let handle = creator
        .and_then(|c| c.get("email"))
        .and_then(Value::as_str)
        .and_then(Handle::email);
    let date_ms = own_stamp_ms(
        m.get("created_date").and_then(Value::as_str),
        "created_date",
        parse_date_ms,
        &mut problems,
    );

    let mut parts: Vec<String> = Vec::new();
    if let Some(first) = thread_starter {
        let who = first
            .get("creator")
            .and_then(|c| c.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("someone");
        parts.push(format!(
            "↪ *In the thread {} started*",
            escape_md_inline(who)
        ));
    }
    if let Some(q) = m.get("quoted_message_metadata") {
        parts.push(quote(q, &mut problems));
    }
    let text = m.get("text").and_then(Value::as_str).unwrap_or("");
    if !text.trim().is_empty() {
        parts.push(escape_md_block(text));
    }
    let shown = Shown::read(
        m.get("annotations"),
        text,
        "/message/annotations",
        &mut problems,
    );
    parts.extend(shown.lines);

    let files = files(
        ctx,
        m.get("attached_files"),
        "/message/attached_files",
        &mut problems,
    );
    for a in &files {
        if let Some(edge) = &a.ref_id {
            inputs.read("chat_attachments", edge);
        }
    }

    let edited = m
        .get("updated_date")
        .and_then(Value::as_str)
        .filter(|u| !u.trim().is_empty());
    if let Some(u) = edited {
        let ms = own_stamp_ms(Some(u), "updated_date", parse_date_ms, &mut problems);
        parts.push(match ms.map(day_of) {
            Some(day) => format!("*Edited {day}*"),
            None => "*Edited*".to_string(),
        });
    }
    if let Some(acting) = creator
        .and_then(|c| c.get("acting_user"))
        .and_then(|a| a.get("name"))
        .and_then(Value::as_str)
    {
        parts.push(format!("*Sent via {}*", escape_md_inline(acting)));
    }

    let mut labels = Vec::new();
    let listed = m.get("message_labels").and_then(Value::as_array);
    for (i, l) in listed.into_iter().flatten().enumerate() {
        let path = format!("/message/message_labels/{i}");
        problems.extend(unread_keys(l, LABEL_KEYS, &path));
        match l.get("label_type").and_then(Value::as_str) {
            Some(t) if is_enum_word(t) => labels.push(enum_words(t)),
            _ => problems
                .push(Problem::field("label_type", Reason::UncoveredType, &shape_of(l)).at(path)),
        }
    }
    if ctx.account.starred.contains(ctx.message_id) {
        labels.push("Starred".to_string());
    }

    let body = (!parts.is_empty()).then(|| parts.join("\n\n"));
    let id = ids::message(ctx.source_id, ctx.message_id, date_ms);
    let mut item = NormalizedChatItem {
        message_uuid: id.uuid.clone(),
        author_handle: handle,
        author_kind: match name {
            Some(_) => AuthorKind::Named,
            None => AuthorKind::Unknown,
        },
        author_display: name.unwrap_or("Unknown").to_string(),
        date_ms,
        text: body,
        kind: ItemKind::Text,
        attachments: files,
        reactions: Vec::new(),
        labels,
        system_note: None,
        source_url: None,
        kind_label: None,
        source_ref: Some(UpstreamRef::new(id.entity_kind, id.natural_key)),
        is_aside: false,
        branch: Vec::new(),
        unread: false,
        recipients: Vec::new(),
        mentions: Vec::new(),
        problems: Vec::new(),
    };
    if let Some(call) = shown.call {
        // A call with nothing else to show is a system note; one with a
        // text, a link or a file says so under them.
        if item.text.is_none() && item.attachments.is_empty() {
            item.kind = ItemKind::System;
            item.system_note = Some(call);
        } else {
            let mut t = item.text.take().unwrap_or_default();
            if !t.is_empty() {
                t.push_str("\n\n");
            }
            t.push_str(&format!("*{call}*"));
            item.text = Some(t);
        }
    }
    if !item.attachments.is_empty() {
        item.kind = ItemKind::Attachment;
    }
    if item.text.is_none() && item.attachments.is_empty() && item.system_note.is_none() {
        item.kind = ItemKind::System;
        item.system_note = Some("No content in the export".to_string());
        problems.push(noted(
            "message",
            &format!("a message with nothing to show: {}", shape_of(m)),
        ));
    }

    let reactions = m.get("reactions").and_then(Value::as_array);
    for (i, r) in reactions.into_iter().flatten().enumerate() {
        let path = format!("/message/reactions/{i}");
        problems.extend(unread_keys(r, REACTION_KEYS, &path));
        let emoji = r.get("emoji");
        if let Some(e) = emoji {
            problems.extend(unread_keys(e, &["unicode"], &format!("{path}/emoji")));
        }
        let Some(unicode) = emoji.and_then(|e| e.get("unicode")).and_then(Value::as_str) else {
            problems.push(Problem::field("emoji", Reason::UncoveredType, &shape_of(r)).at(path));
            continue;
        };
        let reactors = r.get("reactor_emails").and_then(Value::as_array);
        for email in reactors.into_iter().flatten().filter_map(Value::as_str) {
            let rid = ids::message_reaction(ctx.source_id, ctx.message_id, email, unicode);
            item.reactions.push(NormalizedReaction {
                reaction_uuid: rid.uuid,
                reactor_handle: Handle::email(email),
                reactor_display: ctx.name_of(email),
                reactor_kind: AuthorKind::Named,
                emoji: unicode.to_string(),
                date_ms: None,
                source_ref: Some(UpstreamRef::new(rid.entity_kind, rid.natural_key)),
            });
        }
    }
    item.problems = problems;

    let mut out = versions(ctx, m, &item, inputs);
    out.push(item);
    out
}

/// Earlier versions as items of their own, oldest first, folded together
/// just above the message they became.
fn versions(
    ctx: &Ctx,
    m: &Value,
    now: &NormalizedChatItem,
    inputs: &mut Inputs,
) -> Vec<NormalizedChatItem> {
    let Some(listed) = m.get("previous_message_versions").and_then(Value::as_array) else {
        return Vec::new();
    };
    let branch = format!("versions:{}", ctx.message_id);
    let mut out = Vec::new();
    for (n, v) in listed.iter().enumerate() {
        let path = format!("/message/previous_message_versions/{n}");
        let mut problems = unread_keys(v, VERSION_KEYS, &path);
        // A version is dated by when it was saved; the first, never
        // edited before, by when the message was sent.
        let stamp = v
            .get("updated_date")
            .or_else(|| v.get("created_date"))
            .and_then(Value::as_str);
        let date_ms = own_stamp_ms(stamp, "updated_date", parse_date_ms, &mut problems);
        let text = v.get("text").and_then(Value::as_str).unwrap_or("");
        let mut parts: Vec<String> = Vec::new();
        if !text.trim().is_empty() {
            parts.push(escape_md_block(text));
        }
        let shown = Shown::read(
            v.get("annotations"),
            text,
            &format!("{path}/annotations"),
            &mut problems,
        );
        parts.extend(shown.lines);
        let files = files(
            ctx,
            v.get("attached_files"),
            &format!("{path}/attached_files"),
            &mut problems,
        );
        for a in &files {
            if let Some(edge) = &a.ref_id {
                inputs.read("chat_attachments", edge);
            }
        }
        let id = ids::message_version(ctx.source_id, ctx.message_id, n, date_ms);
        out.push(NormalizedChatItem {
            message_uuid: id.uuid,
            date_ms,
            text: (!parts.is_empty()).then(|| parts.join("\n\n")),
            kind: if files.is_empty() {
                ItemKind::Text
            } else {
                ItemKind::Attachment
            },
            attachments: files,
            reactions: Vec::new(),
            labels: Vec::new(),
            system_note: None,
            kind_label: Some("Google Chat Message Version".to_string()),
            source_ref: Some(UpstreamRef::new(id.entity_kind, id.natural_key)),
            branch: vec![branch.clone()],
            problems,
            ..now.clone()
        });
    }
    out
}

/// The message a reply quotes, as a quote block above the reply.
fn quote(q: &Value, problems: &mut Vec<Problem>) -> String {
    let path = "/message/quoted_message_metadata";
    problems.extend(unread_keys(q, QUOTE_KEYS, path));
    let creator = q.get("creator");
    if let Some(c) = creator {
        problems.extend(unread_keys(c, PERSON_KEYS, &format!("{path}/creator")));
    }
    let who = creator
        .and_then(|c| c.get("name"))
        .and_then(Value::as_str)
        .or_else(|| creator.and_then(|c| c.get("email")).and_then(Value::as_str))
        .unwrap_or("Unknown");
    let text = q.get("text").and_then(Value::as_str).unwrap_or("");
    let mut lines = vec![format!("**{}**:", escape_md_inline(who))];
    if !text.trim().is_empty() {
        lines.push(escape_md_block(text));
    }
    // The quoted message's own previews and files are drawn where it is;
    // here they only need counting.
    let mut ignored = Vec::new();
    let shown = Shown::read(
        q.get("annotations"),
        text,
        &format!("{path}/annotations"),
        &mut ignored,
    );
    problems.extend(ignored);
    lines.extend(shown.lines);
    let n_files = q
        .get("attached_files")
        .and_then(Value::as_array)
        .map_or(0, |a| {
            for (i, f) in a.iter().enumerate() {
                problems.extend(unread_keys(
                    f,
                    FILE_KEYS,
                    &format!("{path}/attached_files/{i}"),
                ));
            }
            a.len()
        });
    if n_files > 0 {
        lines.push(format!(
            "*{n_files} attached file{}*",
            if n_files == 1 { "" } else { "s" }
        ));
    }
    lines
        .join("\n\n")
        .lines()
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

/// What a message's annotations add to its text: a link, a Drive file or
/// a video the text does not already show, a Meet link, a call.
#[derive(Default)]
struct Shown {
    lines: Vec<String>,
    call: Option<String>,
}

impl Shown {
    fn read(
        annotations: Option<&Value>,
        text: &str,
        path: &str,
        problems: &mut Vec<Problem>,
    ) -> Self {
        let mut out = Shown::default();
        let listed = annotations.and_then(Value::as_array);
        for (i, a) in listed.into_iter().flatten().enumerate() {
            let path = format!("{path}/{i}");
            problems.extend(unread_keys(a, ANNOTATION_KEYS, &path));
            if let Some(u) = a.get("url_metadata") {
                problems.extend(unread_keys(u, URL_KEYS, &format!("{path}/url_metadata")));
                let url = u.get("url").and_then(only_string);
                let title = u
                    .get("title")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|t| !t.is_empty());
                if let Some(url) = url.filter(|url| !text.contains(url)) {
                    out.lines.push(match title {
                        Some(t) => format!("🔗 [{}]({})", escape_md_inline(t), md_link_dest(url)),
                        None => format!("🔗 [{}]({})", escape_md_inline(url), md_link_dest(url)),
                    });
                }
            }
            if let Some(d) = a.get("drive_metadata") {
                problems.extend(unread_keys(
                    d,
                    DRIVE_KEYS,
                    &format!("{path}/drive_metadata"),
                ));
                if let Some(id) = d
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !text.contains(id))
                {
                    let title = d
                        .get("title")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|t| !t.is_empty())
                        .unwrap_or("A Drive file");
                    let url = format!("https://drive.google.com/open?id={id}");
                    out.lines.push(format!(
                        "📄 [{}]({})",
                        escape_md_inline(title),
                        md_link_dest(&url)
                    ));
                }
            }
            if let Some(y) = a.get("youtube_metadata") {
                problems.extend(unread_keys(
                    y,
                    YOUTUBE_KEYS,
                    &format!("{path}/youtube_metadata"),
                ));
                if let Some(id) = y
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !text.contains(id))
                {
                    let url = format!("https://www.youtube.com/watch?v={id}");
                    out.lines.push(format!("▶ [{url}]({url})"));
                }
            }
            if let Some(f) = a.get("format_metadata") {
                problems.extend(unread_keys(
                    f,
                    FORMAT_KEYS,
                    &format!("{path}/format_metadata"),
                ));
            }
            if let Some(g) = a.get("gsuite_integration_metadata") {
                let p = format!("{path}/gsuite_integration_metadata");
                problems.extend(unread_keys(g, &["call_data"], &p));
                if let Some(c) = g.get("call_data") {
                    problems.extend(unread_keys(c, &["call_status"], &format!("{p}/call_data")));
                    let status = c.get("call_status").and_then(Value::as_str);
                    out.call = Some(match status.filter(|s| is_enum_word(s)) {
                        Some(s) => enum_words(s),
                        None => "A call".to_string(),
                    });
                }
            }
            if let Some(v) = a.get("video_call_metadata") {
                let p = format!("{path}/video_call_metadata");
                problems.extend(unread_keys(v, &["meeting_space"], &p));
                if let Some(space) = v.get("meeting_space") {
                    problems.extend(unread_keys(
                        space,
                        &["meeting_url"],
                        &format!("{p}/meeting_space"),
                    ));
                    if let Some(url) = space
                        .get("meeting_url")
                        .and_then(Value::as_str)
                        .filter(|u| !text.contains(u))
                    {
                        out.lines.push(format!(
                            "📹 [{}]({})",
                            escape_md_inline(url),
                            md_link_dest(url)
                        ));
                    }
                }
            }
        }
        out
    }
}

/// The one string an object holds: Chat wraps a preview's URL in an
/// object whose single key says not to use it directly.
fn only_string(v: &Value) -> Option<&str> {
    match v {
        Value::String(s) => Some(s),
        Value::Object(m) if m.len() == 1 => m.values().next()?.as_str(),
        _ => None,
    }
}

fn files(
    ctx: &Ctx,
    listed: Option<&Value>,
    path: &str,
    problems: &mut Vec<Problem>,
) -> Vec<NormalizedAttachment> {
    let mut out = Vec::new();
    for (i, f) in listed
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .enumerate()
    {
        let path = format!("{path}/{i}");
        problems.extend(unread_keys(f, FILE_KEYS, &path));
        let Some(export) = f
            .get("export_name")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
        else {
            problems.push(
                Problem::field("export_name", Reason::UncoveredType, &shape_of(f))
                    .at(path)
                    .severity(Severity::Warning),
            );
            continue;
        };
        let shown = f
            .get("original_name")
            .and_then(Value::as_str)
            .filter(|n| !n.is_empty())
            .unwrap_or(export);
        out.push(NormalizedAttachment {
            href: None,
            file_name: Some(shown.to_string()),
            mime_type: mime_of(export),
            byte_len: None,
            source_url: None,
            ref_id: Some(ChatAttachmentRow::pk_recipe(ctx.message_id, export)),
        });
    }
    out
}

/// The attachment edges a message's files are stored under, for loading
/// their bytes: its own, its earlier versions' and the quoted message's.
pub fn attachment_ids(m: &Value) -> Vec<String> {
    let Some(message_id) = m.get("message_id").and_then(Value::as_str) else {
        return Vec::new();
    };
    let versions = m
        .get("previous_message_versions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten();
    std::iter::once(m)
        .chain(versions)
        .filter_map(|v| v.get("attached_files").and_then(Value::as_array))
        .flatten()
        .filter_map(|f| f.get("export_name").and_then(Value::as_str))
        .filter(|n| !n.is_empty())
        .map(|n| ChatAttachmentRow::pk_recipe(message_id, n))
        .collect()
}

/// The owning space id for a message. Takeout `message_id`s are
/// `"<space>/<topic>/<message>"`, so the first path segment is the
/// space. Falls back to the whole id when there's no `/`.
pub fn space_of(message_id: &str) -> String {
    message_id
        .split('/')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(message_id)
        .to_string()
}

/// The space id in a `chat_groups` directory name (`"DM <id>"`, `"Space
/// <id>"`) or a `membership_info` group id (`"Space <id>"`): its last
/// word, the value [`space_of`] parses out of a `message_id`.
pub fn space_of_dir(dir: &str) -> String {
    dir.rsplit(' ')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(dir)
        .to_string()
}

/// Google Chat's `Tuesday, February 11, 2025 at 11:33:35 AM UTC`, read
/// the way the ingest reads it, so a zone it knows is one the render
/// knows.
pub fn parse_date_ms(s: &str) -> Option<i64> {
    let rfc3339 = datalib_etl_google_takeout::ingest::time::parse_chat_long_form(s)?;
    datalib_time::parse_strict(&rfc3339)
        .ok()
        .map(|t| t.to_unix_millis())
}

fn day_of(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SENT: &str = "Tuesday, February 11, 2364 at 11:33:35 AM UTC";
    const EDITED: &str = "Tuesday, February 11, 2364 at 11:40:00 AM UTC";

    fn group() -> (String, Value) {
        (
            "Space AAAA-BRIDGE".to_string(),
            json!({"name": "Bridge crew", "members": [
                {"name": "Jean-Luc Picard", "email": "picard@enterprise.starfleet", "user_type": "Human"},
                {"name": "Data", "email": "data@enterprise.starfleet", "user_type": "Human"},
            ]}),
        )
    }

    fn msg(id: &str, topic: &str, extra: Value) -> (String, Value) {
        let mut m = json!({
            "creator": {"name": "Data", "email": "data@enterprise.starfleet", "user_type": "Human"},
            "created_date": SENT,
            "text": "Spot is well.",
            "topic_id": topic,
            "message_id": format!("AAAA-BRIDGE/{topic}/{id}"),
        });
        m.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        (format!("AAAA-BRIDGE/{topic}/{id}"), m)
    }

    fn items(chats: &[NormalizedChat]) -> Vec<&NormalizedChatItem> {
        chats
            .iter()
            .flat_map(|c| &c.buckets)
            .flat_map(|b| &b.items)
            .collect()
    }

    #[test]
    fn a_named_space_is_titled_by_its_name() {
        let chats = build("src", &[msg("m1", "t1", json!({}))], &[group()], &[]);
        assert_eq!(chats[0].display, "Bridge crew");
        assert_eq!(chats[0].title.as_deref(), Some("Bridge crew"));
        assert!(chats[0].named_after.is_empty());
    }

    /// Earlier versions fold in above the message, oldest first, and the
    /// message says it was edited.
    #[test]
    fn earlier_versions_fold_in_above_the_message() {
        let m = msg(
            "m1",
            "t1",
            json!({
                "text": "Spot is well fed.",
                "updated_date": EDITED,
                "previous_message_versions": [
                    {"text": "Spot is well.", "created_date": SENT},
                ],
            }),
        );
        let chats = build("src", &[m], &[group()], &[]);
        let got = items(&chats);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].branch, ["versions:AAAA-BRIDGE/t1/m1"]);
        assert_eq!(got[0].text.as_deref(), Some("Spot is well."));
        assert!(got[1].branch.is_empty());
        assert!(got[1]
            .text
            .as_deref()
            .unwrap()
            .ends_with("*Edited 2364-02-11*"));
        assert!(got.iter().all(|i| i.problems.is_empty()), "{got:?}");
    }

    #[test]
    fn a_reply_quotes_and_a_thread_reply_says_whose_thread() {
        let first = msg("m1", "t1", json!({}));
        let reply = msg(
            "m2",
            "t1",
            json!({
                "created_date": EDITED,
                "creator": {"name": "Jean-Luc Picard", "email": "picard@enterprise.starfleet", "user_type": "Human"},
                "text": "Make it so.",
                "quoted_message_metadata": {
                    "creator": {"name": "Data", "email": "data@enterprise.starfleet", "user_type": "Human"},
                    "text": "Shall I feed Spot?",
                },
            }),
        );
        let chats = build("src", &[first, reply], &[group()], &[]);
        let text = items(&chats)[1].text.clone().unwrap();
        assert!(text.starts_with("↪ *In the thread Data started*"), "{text}");
        assert!(
            text.contains("> **Data**:\n>\n> Shall I feed Spot?"),
            "{text}"
        );
        assert!(text.ends_with("Make it so."));
    }

    #[test]
    fn reactions_are_one_per_reactor_named_from_the_members() {
        let m = msg(
            "m1",
            "t1",
            json!({"reactions": [{"emoji": {"unicode": "🖖"}, "reactor_emails": ["picard@enterprise.starfleet", "q@continuum.example"]}]}),
        );
        let chats = build("src", &[m], &[group()], &[]);
        let r = &items(&chats)[0].reactions;
        let who: Vec<&str> = r.iter().map(|r| r.reactor_display.as_str()).collect();
        assert_eq!(who, ["Jean-Luc Picard", "q@continuum.example"]);
    }

    #[test]
    fn a_link_the_text_does_not_show_is_drawn_and_a_call_is_a_system_item() {
        let link = msg(
            "m1",
            "t1",
            json!({"text": "Briefing notes", "annotations": [
                {"start_index": 0, "length": 8, "url_metadata": {
                    "url": {"private_do_not_access_or_else_safe_url_do_not_use": "https://memory-alpha.example/wiki/Spot"},
                    "title": "Spot", "snippet": "A cat.", "image_url": ""}},
                {"start_index": 9, "length": 5, "drive_metadata": {"id": "1DRIVEID", "title": "Duty roster", "thumbnail_url": ""}},
                {"start_index": 0, "length": 8, "format_metadata": {"format_type": "BOLD"}},
            ]}),
        );
        let call = msg(
            "m2",
            "t2",
            json!({"text": "", "annotations": [{"start_index": 0, "length": 1,
                "gsuite_integration_metadata": {"call_data": {"call_status": "CALL_ENDED"}}}]}),
        );
        let chats = build("src", &[link, call], &[group()], &[]);
        let got = items(&chats);
        let text = got[0].text.clone().unwrap();
        assert!(
            text.contains("🔗 [Spot](https://memory-alpha.example/wiki/Spot)"),
            "{text}"
        );
        assert!(text.contains("📄 [Duty roster](https://drive.google.com/open?id=1DRIVEID)"));
        assert!(got[0].problems.is_empty(), "{:?}", got[0].problems);
        assert_eq!(got[1].kind, ItemKind::System);
        assert_eq!(got[1].system_note.as_deref(), Some("Call ended"));
    }

    /// A call that names its Meet link is a message, so the link shows.
    #[test]
    fn a_call_with_a_meet_link_keeps_the_link() {
        let call = msg(
            "m1",
            "t1",
            json!({"text": "", "annotations": [
                {"start_index": 0, "length": 1, "gsuite_integration_metadata": {"call_data": {"call_status": "CALL_ENDED"}}},
                {"start_index": 0, "length": 1, "video_call_metadata": {"meeting_space": {"meeting_url": "https://meet.google.com/tng-tenf-wrd"}}},
            ]}),
        );
        let chats = build("src", &[call], &[group()], &[]);
        let item = items(&chats)[0];
        assert_eq!(item.kind, ItemKind::Text);
        assert_eq!(
            item.text.as_deref(),
            Some("📹 [https://meet.google.com/tng-tenf-wrd](https://meet.google.com/tng-tenf-wrd)\n\n*Call ended*")
        );
    }

    #[test]
    fn a_starred_message_is_labelled_and_an_unread_key_is_reported_by_shape() {
        let m = msg("m1", "t1", json!({"future_field": {"a": 1}}));
        let user = (
            "User 1".to_string(),
            json!({"user": {"name": "Data", "email": "data@enterprise.starfleet", "user_type": "Human"},
                   "starred_message_ids": ["AAAA-BRIDGE/t1/m1"],
                   "unsent_drafts": [1, 2]}),
        );
        let chats = build("src", &[m], &[group()], &[user]);
        let item = items(&chats)[0];
        assert_eq!(item.labels, ["Starred"]);
        let fields: Vec<(String, String)> = item
            .problems
            .iter()
            .map(|p| (p.field.clone().unwrap(), p.sample.clone()))
            .collect();
        assert!(fields.contains(&("future_field".into(), "object{a}".into())));
        assert!(fields.contains(&("unsent_drafts".into(), "array(2)".into())));
    }

    #[test]
    fn attachments_name_their_edge() {
        let m = msg(
            "m1",
            "t1",
            json!({"attached_files": [{"original_name": "spot.png", "export_name": "spot.png"}]}),
        );
        let chats = build("src", std::slice::from_ref(&m), &[group()], &[]);
        assert_eq!(items(&chats)[0].kind, ItemKind::Attachment);
        let a = &items(&chats)[0].attachments[0];
        assert_eq!(a.file_name.as_deref(), Some("spot.png"));
        assert_eq!(a.mime_type.as_deref(), Some("image/png"));
        assert_eq!(attachment_ids(&m.1), [a.ref_id.clone().unwrap()]);
    }

    #[test]
    fn spaces_are_read_out_of_ids_and_dirs() {
        assert_eq!(space_of("AAAA/abc/def"), "AAAA");
        assert_eq!(space_of("no-slash"), "no-slash");
        assert_eq!(space_of_dir("DM AAAA"), "AAAA");
        assert_eq!(space_of_dir("Space BBBB"), "BBBB");
    }
}
