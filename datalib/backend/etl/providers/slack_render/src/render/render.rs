//! Slack render: convert parsed thread render units into the shared
//! `chat-common` normalized model and delegate the markdown / grid-row /
//! grid-row plumbing to [`datalib_etl_chat_common::render::render_all`].

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde_json::Value;

use datalib_etl::blob_cas::BlobBundle;
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::render::{ChatRenderer, RenderProfile, RenderUnits};
use datalib_etl_chat_common::types::{
    own_stamp_ms, AuthorKind, ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem,
    NormalizedDoc, NormalizedReaction, Replies, UpstreamRef,
};
use datalib_etl_chat_common::unread::{is_unread, Marks};
use datalib_etl_chat_common::{render_paged, Page, PageEnd, PagedChats, TextFormat};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_handle::Handle;

use super::mrkdwn::{mentioned_users, resolve_mentions, to_commonmark, Labels};
use super::parse::{ParsedSlack, Shared, SlackStore, SlackThreadRenderUnit};
use super::{cards, emoji};
use super::{ids, slack_link, ts_to_ms, Message};
use datalib_etl_render::inputs::Lookup;
use datalib_etl_render::pages::{PageSize, RenderUnitPages};
use datalib_etl_slack::ingest::schema_raw::split_key;
use datalib_schema::data_nodes::DataNodeRow;
use datalib_schema::problems::{Problem, Reason, Severity};
use datalib_schema::providers::Provider;

/// Bump when the on-disk render layout changes in a way that must
/// invalidate stale docs. v3: render via chat-common.
/// v4: ids are minted through `datalib_id` (#216). Slack's scoping was
///     already right, but the recipes changed their join character, so
///     every uuid moved — `chat_uuid` among them, which names the
///     output directory. A v3 tree cannot be updated in place; the
///     render step discards it wholesale. See
///     `DataProcessor::render_version`.
/// v5: a `ts` we cannot parse is a null `created_at` instead of a
///     real-looking `1970-01-01T00:00:00` — see
///     `docs/dev/data_architecture_parse_and_render.md` §6. Any document
///     holding such a row renders differently, so stale docs must go.
/// v6: `account` is the login's email rather than the `T…` workspace
///     id; the workspace moves to `org_name` / `org_uuid`.
/// v7: every id carries its row's `created_at` in its leading bits
///     (`datalib_id`'s v8 layout) and the configured source in its
///     recipe. The raw store keys messages and threads by
///     `{team}#{channel}#{ts}`, so an existing root resets and downloads
///     again.
/// v9: the author span carries the author's handle as `data-handle`.
/// v10: each thread carries its authors' Slack profiles (title, email).
/// v11: a `<@U…>` mention in a body is a chip link to the user.
/// v12: a mention in code is `@Name`, not a link's markdown, and a `!`
///     typed before a mention or link no longer makes it an image.
/// v13: a permalink is on the workspace's own host (`auth.test`'s `url`),
///     so it opens in that workspace, not the browser's current one.
/// v14: emoji shortcodes are Slack's own names, skin tones included, and
///     code keeps its shortcodes as typed.
/// v15: the account's own profile is marked as the account, so search
///     reads its handles as `me`.
/// v16: a thread's root says how many replies Slack counts, and how many
///     of them are not mirrored yet.
/// v17: the read marks are read from `read_marks`. The output is the
///     same, but the first render across the rung that made the table
///     cannot tell which marks moved in that range (`read_marks::moved`),
///     so every thread renders once.
/// v18: a document says when its messages hold cards, link previews or
///     forwards that are not shown.
/// v19: a message's time is on its author's profile clock (`tz`).
/// v20: a ``` block with code on its fence lines is closed, so it no
///     longer turns the messages after it into code.
/// v21: a thread sits in its channel, in its workspace.
/// v22: a message's time names no clock (`Z`): a profile's zone is where
///     its author is now, not where they were when they wrote it.
/// v23: a message's blocks and attachments are drawn — a forward, a link
///     preview, a bot's card — and only what is still not drawn is noted.
/// v24: a bot's thread is titled by what its blocks show, and an image
///     block whose address is not a web one keeps its label.
/// v25: a bot's message is named by the name it posted under or its
///     bot's, not "unknown".
pub const RENDER_VERSION: u32 = 25;

#[derive(Debug, Default)]
pub struct RenderSummary {
    pub threads_total: usize,
    pub threads_rendered: usize,
    /// Every thread rendered, by uuid — what the processor declares
    /// through `RenderCtx::declare_render_unit`.
    pub render_units: RenderUnits,
}

fn profile() -> RenderProfile {
    RenderProfile {
        stamp_precision: ids::STAMP_PRECISION,
        provider: Provider::Slack,
        source_label: "Slack".to_string(),
        chat_kind: "Slack Thread".to_string(),
        message_kind: "Slack Message".to_string(),
        reaction_kind: "Slack Reaction".to_string(),
        chat_entity_kind: ids::KIND_THREAD,
        render_version: RENDER_VERSION,
        text_format: TextFormat::Markdown,
        layout: datalib_etl_chat_common::Layout::Chat,
    }
}

/// What every thread's render looks up.
struct Lookups<'a> {
    shared: &'a Shared,
    user_labels: BTreeMap<String, String>,
    channel_labels: BTreeMap<String, String>,
    account: Option<String>,
    org_name: Option<String>,
}

impl<'a> Lookups<'a> {
    fn new(shared: &'a Shared) -> Self {
        Lookups {
            shared,
            user_labels: shared
                .users
                .iter()
                .map(|(id, u)| (id.clone(), u.label()))
                .collect(),
            channel_labels: shared
                .channels
                .iter()
                .filter_map(|(id, c)| Some((id.clone(), c.name.clone()?)))
                .collect(),
            account: shared.account_label(),
            // The workspace is the organization this login lives in,
            // which is what `org_name` means for Claude's Team workspaces
            // too.
            org_name: shared
                .workspace
                .as_ref()
                .and_then(|w| w.team_name.clone())
                .filter(|n| !n.trim().is_empty()),
        }
    }
}

/// Renders every thread `store` hands out, a page at a time; `on_page`
/// hears how each page ended before the next is read.
pub fn render_store(
    store: &SlackStore,
    source_id: &str,
    size: PageSize,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
    on_page: &mut dyn FnMut(PageEnd) -> Result<()>,
) -> Result<()> {
    let elapsed_ms = store.scan.scan_elapsed.map(|d| d.as_millis() as u64);
    tracing::info!(
        source = source_id,
        scan_elapsed_ms = elapsed_ms,
        threads_to_render = store.threads,
        cold_start = store.scan.render.is_none(),
        "[render] slack dolt_diff scan"
    );
    let lookups = Lookups::new(&store.shared);
    let profile = profile();
    let mut renderer = ChatRenderer::new(
        &profile,
        source_id,
        store.threads,
        progress,
        on_doc_complete,
    );
    let pages = store.pages(size.render_units);
    let chats = Chats {
        store,
        lookups,
        source_id,
    };
    render_paged(&chats, pages, size.bytes, &mut renderer, on_page)
}

struct Chats<'a> {
    store: &'a SlackStore,
    lookups: Lookups<'a>,
    source_id: &'a str,
}

impl PagedChats for Chats<'_> {
    type Conversation = SlackThreadRenderUnit;

    fn next_page(
        &self,
        pages: &mut RenderUnitPages,
    ) -> Result<Option<Page<SlackThreadRenderUnit>>> {
        let page = self.store.next_page(pages)?;
        Ok(page.map(|page| Page {
            looked_at: page
                .looked_at
                .iter()
                .filter_map(|key| {
                    let (team, channel, ts) = split_key(key)?;
                    Some(ids::thread(self.source_id, team, channel, ts).uuid)
                })
                .collect(),
            ..page
        }))
    }

    fn attachment_bytes(&self, thread: &SlackThreadRenderUnit) -> u64 {
        thread.attachment_bytes()
    }

    fn load_blobs(&self, batch: &[&SlackThreadRenderUnit]) -> Result<Vec<SlackThreadRenderUnit>> {
        self.store.load_blobs(batch)
    }

    fn chat(&self, thread: &SlackThreadRenderUnit) -> NormalizedChat {
        build_chat(&self.lookups, thread, self.source_id)
    }

    fn blobs<'c>(&self, thread: &'c SlackThreadRenderUnit) -> &'c BlobBundle {
        &thread.blobs
    }
}

/// Renders every thread in `parsed`, its attachments already loaded.
pub fn render_all(
    parsed: &ParsedSlack,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let lookups = Lookups::new(&parsed.shared);
    let profile = profile();
    let mut renderer = ChatRenderer::new(
        &profile,
        source_id,
        parsed.threads.len(),
        progress,
        on_doc_complete,
    );
    for thread in &parsed.threads {
        renderer
            .render_chat(&build_chat(&lookups, thread, source_id), &thread.blobs)
            .context("slack chat-common render")?;
    }
    let cc = renderer.finish();
    Ok(RenderSummary {
        threads_total: parsed.threads.len(),
        threads_rendered: cc.docs_rendered,
        render_units: cc.render_units,
    })
}

fn build_chat(
    lookups: &Lookups<'_>,
    render_unit: &SlackThreadRenderUnit,
    source_id: &str,
) -> NormalizedChat {
    let shared = lookups.shared;
    // Who this mirror belongs to, from `auth.test`. Used to subtract
    // the account itself out of a group DM's participant list when
    // naming it — see `Channel::display`.
    let self_user_id = shared
        .workspace
        .as_ref()
        .and_then(|w| w.self_user_id.as_deref());
    let team_url = shared
        .workspace
        .as_ref()
        .and_then(|w| w.team_url.as_deref());
    // Every user and channel this thread resolves is recorded on the
    // render unit as it is asked for, so the thread declares exactly what
    // it read.
    let labels = Labels {
        users: render_unit.inputs.lookup("users", &lookups.user_labels),
        channels: render_unit
            .inputs
            .lookup("channels", &lookups.channel_labels),
        team_id: render_unit
            .messages
            .first()
            .map_or("", |m| m.team_id.as_str()),
    };
    let channels = render_unit.inputs.lookup("channels", &shared.channels);
    let bots = render_unit.inputs.lookup("bots", &shared.bots);
    let root: &Message = render_unit
        .messages
        .iter()
        .find(|m| m.is_thread_root)
        .unwrap_or_else(|| {
            render_unit
                .messages
                .first()
                .expect("non-empty thread render unit")
        });
    // `#general` for a channel, `@Jean-Luc Picard` for a DM, and
    // `#<channel_id>` when the channel row never arrived. See
    // `Channel::display`.
    let cname = match channels.get(&root.channel_id) {
        Some(c) => c.display(labels.users, self_user_id),
        None => format!("#{}", root.channel_id),
    };
    let named_after = channels
        .get(&root.channel_id)
        .map(|c| c.named_after(labels.users, self_user_id, &root.team_id))
        .unwrap_or_default();
    let is_dm = channels.get(&root.channel_id).is_some_and(|c| c.is_dm);
    let containers = containers_of(
        source_id,
        root,
        lookups.org_name.clone(),
        cname.clone(),
        is_dm,
    );
    let thread = ids::thread(source_id, &root.team_id, &root.channel_id, &root.ts);
    let thread_uuid = thread.uuid.clone();

    // A top-level message is read up to its conversation's mark, a
    // reply up to its thread's, which Slack keeps only for a thread
    // the account follows. The account's own messages are never
    // unread, whatever the mark says.
    let channel_mark = shared.read_marks.channels.get(&root.channel_id);
    let thread_mark = shared.read_marks.threads.get(&render_unit.thread_key);
    let unread = |m: &Message| {
        let conversation = if m.is_thread_root {
            channel_mark
        } else {
            thread_mark
        };
        let from_me = m.user_id.as_deref() == self_user_id;
        let marks = Marks {
            own: None,
            conversation,
        };
        is_unread(&m.ts, from_me, marks, ids::ts_order)
    };
    // What the page shows under the root, a broadcast reply among them.
    let held_replies = render_unit
        .messages
        .iter()
        .filter(|m| !m.is_thread_root)
        .count() as u64;
    let mut not_shown: Vec<Vec<String>> = Vec::with_capacity(render_unit.messages.len());
    let mut items: Vec<NormalizedChatItem> = render_unit
        .messages
        .iter()
        .map(|m| {
            let (mut item, parts) =
                build_item(source_id, m, root, team_url, labels, bots, unread(m));
            item.replies = replies_of(m, held_replies, &mut item.problems);
            not_shown.push(parts);
            item
        })
        .collect();
    note_what_is_not_shown(&not_shown, &mut items);

    // "#channel: <root snippet>" keeps the H1 scannable; the bare
    // "#channel" is the conversation_name (via `display`).
    // A bot's `text` is only its notification's summary of the blocks
    // the page shows instead.
    let headline = cards::draw(&root.raw_json, labels).headline;
    let title = format!(
        "{cname}: {}",
        thread_title(headline.as_deref().unwrap_or(&root.text), labels)
    );

    NormalizedChat {
        named_after,
        id: thread_uuid.clone(),
        containers,
        filed_under: Vec::new(),
        chat_uuid: thread_uuid.clone(),
        display: cname,
        title: Some(title),
        author: None,
        account: lookups.account.clone(),
        project: None,
        // The exact natural key the thread's `uuid` was minted
        // from — `{channel_id}#{thread_ts}`, the tuple
        // `conversations.replies` takes back. Must be this
        // spelling, not a prettier one: the round-trip check
        // recomputes `uuid` from it.
        external_id: Some(thread.natural_key),
        // Thread permalink → chat-level `↗` + chat grid source_url.
        source_url: Some(slack_link(
            team_url,
            &root.team_id,
            &root.channel_id,
            &root.ts,
            None,
        )),
        // Every row in this thread was minted under
        // `Some(team_id)`; the round-trip check
        // recomputes `uuid` from this exact string.
        upstream_account: Some(root.team_id.clone()),
        org_uuid: Some(root.team_id.clone()),
        org_name: lookups.org_name.clone(),
        render_units: vec![NormalizedDoc {
            orphan_reactions: Vec::new(),
            markdown_uuid: thread_uuid,
            source_ref: None,
            items,
        }],
        // Each author's profile, read the way their label was.
        contacts: authors_of(render_unit)
            .filter_map(|uid| {
                let user = render_unit.inputs.lookup("users", &shared.users).get(uid)?;
                let mut contact = user.contact(source_id)?;
                contact.account = Some(uid) == self_user_id;
                Some(contact)
            })
            .collect(),
        inputs: render_unit.inputs.declared(),
    }
}

/// The workspace, then the channel or direct message: both places a
/// person browses, each keyed on Slack's own id.
fn containers_of(
    source_id: &str,
    root: &Message,
    workspace_name: Option<String>,
    channel_name: String,
    is_dm: bool,
) -> Vec<DataNodeRow> {
    let workspace = ids::workspace(source_id, &root.team_id);
    let channel = ids::channel(source_id, &root.team_id, &root.channel_id);
    let channel_kind = if is_dm { "Slack DM" } else { "Slack Channel" };
    vec![
        DataNodeRow::new("", workspace.uuid, source_id, "Slack Workspace")
            .named(workspace_name)
            .upstream(
                workspace.entity_kind,
                workspace.natural_key,
                root.team_id.clone(),
            )
            .place(),
        DataNodeRow::new("", channel.uuid, source_id, channel_kind)
            .named(Some(channel_name))
            .upstream(
                channel.entity_kind,
                channel.natural_key,
                root.team_id.clone(),
            )
            .place(),
    ]
}

/// Each user who wrote in the thread, once, in the order they first did.
fn authors_of(render_unit: &SlackThreadRenderUnit) -> impl Iterator<Item = &str> {
    let mut seen = std::collections::HashSet::new();
    render_unit
        .messages
        .iter()
        .filter_map(|m| m.user_id.as_deref())
        .filter(move |u| seen.insert(*u))
}

/// One row a document, on the first message holding a part nothing draws
/// (`not_shown`, each message's), naming the kinds. At `info`: it shows on
/// the page, and is no fault of the sync.
fn note_what_is_not_shown(not_shown: &[Vec<String>], items: &mut [NormalizedChatItem]) {
    let holding: Vec<usize> = (0..not_shown.len())
        .filter(|&i| !not_shown[i].is_empty())
        .collect();
    let (Some(&first), count) = (holding.first(), holding.len()) else {
        return;
    };
    let mut kinds: Vec<&str> = Vec::new();
    for kind in holding.iter().flat_map(|&i| &not_shown[i]) {
        if !kinds.contains(&kind.as_str()) {
            kinds.push(kind);
        }
    }
    let field = if kinds.iter().all(|k| k.ends_with(" block")) {
        "blocks"
    } else {
        "attachments"
    };
    let (many, hold) = if count == 1 {
        ("message", "holds")
    } else {
        ("messages", "hold")
    };
    items[first].problems.push(
        Problem::explained(
            Reason::UncoveredType,
            Some(field.to_string()),
            &format!(
                "{count} {many} here {hold} what is not shown: {}",
                kinds.join(", ")
            ),
        )
        .severity(Severity::Info),
    );
}

fn build_item(
    source_id: &str,
    m: &Message,
    root: &Message,
    team_url: Option<&str>,
    labels: Labels<'_>,
    bots: Lookup<'_, BTreeMap<String, String>>,
    unread: bool,
) -> (NormalizedChatItem, Vec<String>) {
    let (author_display, author_kind) = author_of(m, labels.users, bots);
    let drawn = cards::draw(&m.raw_json, labels);
    let body = drawn.body(to_commonmark(m.text.trim_end(), labels));
    let attachments = build_attachments(&m.raw_json);
    let reactions = build_reactions(source_id, &m.raw_json, m, labels.users);
    let kind = if attachments.is_empty() {
        ItemKind::Text
    } else {
        ItemKind::Attachment
    };
    let msg_id = ids::message(source_id, &m.team_id, &m.channel_id, &m.ts);
    // A `ts` is the message's identity as well as its time; one that
    // will not parse is a record with a name and no place in time.
    let mut problems = Vec::new();
    let date_ms = own_stamp_ms(Some(&m.ts), "ts", ts_to_ms, &mut problems);
    let item = NormalizedChatItem {
        reply_to: None,
        message_uuid: msg_id.uuid.clone(),
        author_handle: m
            .user_id
            .as_deref()
            .and_then(|u| Handle::slack(&m.team_id, u)),
        author_kind,
        author_display,
        date_ms,
        // A `ts` names no clock, and a profile's `tz` is where its
        // author is now, not where they were when they wrote this.
        date_offset_secs: None,
        text: (!body.trim().is_empty()).then_some(body),
        kind,
        attachments,
        reactions,
        labels: Vec::new(),
        system_note: None,
        // Per-message permalink (with thread_ts for replies).
        source_url: Some(slack_link(
            team_url,
            &m.team_id,
            &m.channel_id,
            &m.ts,
            Some(&root.ts),
        )),
        kind_label: None,
        source_ref: Some(UpstreamRef::new(
            msg_id.entity_kind,
            msg_id.natural_key.clone(),
        )),
        is_aside: false,
        branch: Vec::new(),
        unread,
        replies: None,
        recipients: Vec::new(),
        mentions: mentioned_users(&m.text, &m.team_id),
        problems,
    };
    (item, drawn.not_shown)
}

/// Who wrote `m`: a user by their profile; else a bot by the name it
/// posted under (`username`, which an integration sets per message, or
/// its app's `bot_profile`), else by the name `bots.info` gave it; else
/// whichever id the message carries. Nobody, where it carries none.
fn author_of(
    m: &Message,
    users: Lookup<'_, BTreeMap<String, String>>,
    bots: Lookup<'_, BTreeMap<String, String>>,
) -> (String, AuthorKind) {
    let named = |v: Option<&Value>| {
        v.and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let raw = &m.raw_json;
    let bot_id = named(raw.get("bot_id"));
    m.user_id
        .as_deref()
        .and_then(|u| users.get(u).cloned())
        .or_else(|| named(raw.get("username")))
        .or_else(|| named(raw.pointer("/bot_profile/name")))
        .or_else(|| bots.get(bot_id.as_deref()?).cloned())
        .or_else(|| m.user_id.clone())
        .or(bot_id)
        .map_or_else(
            || ("Unknown".to_string(), AuthorKind::Unknown),
            |name| (name, AuthorKind::Named),
        )
}

fn replies_of(m: &Message, held: u64, problems: &mut Vec<Problem>) -> Option<Replies> {
    if !m.is_thread_root {
        return None;
    }
    let raw = m.raw_json.get("reply_count")?;
    let Some(upstream) = raw.as_u64() else {
        problems.push(Problem::field(
            "reply_count",
            Reason::CoercionFailed,
            &raw.to_string(),
        ));
        return None;
    };
    Some(Replies { upstream, held })
}

fn thread_title(root_text: &str, labels: Labels<'_>) -> String {
    // A ``` ends the line it is on, as the body draws it.
    let resolved = resolve_mentions(root_text, labels).replace("```", "\n");
    let first = resolved
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("(empty thread)")
        .to_string();
    first.chars().take(80).collect()
}

// File / reaction extraction from raw_json → normalized model.

/// Map a Slack message's `files[]` into [`NormalizedAttachment`]s. The
/// real `mimetype` flows through so chat-common picks `<img>` / `<audio>`
/// / `<video>` / generic per attachment. Local bytes resolve via
/// `ref_id` (the Slack `file_id`) against the thread's bundle; externals
/// / tombstones carry no `ref_id` and fall back to the upstream URL.
fn build_attachments(raw: &Value) -> Vec<NormalizedAttachment> {
    raw.get("files")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|f| {
                    let filetype = f.get("filetype").and_then(|v| v.as_str()).unwrap_or("");
                    let mime_type = f
                        .get("mimetype")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .or_else(|| image_mime_for(filetype));
                    let file_name = f
                        .get("title")
                        .and_then(|v| v.as_str())
                        .or_else(|| f.get("name").and_then(|v| v.as_str()))
                        .map(str::to_string);
                    let external = f
                        .get("is_external")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                        || f.get("mode").and_then(|v| v.as_str()) == Some("tombstone");
                    let source_url = f
                        .get("url_private")
                        .or_else(|| f.get("permalink"))
                        .and_then(|v| v.as_str())
                        .map(str::to_string);
                    let ref_id = if external {
                        None
                    } else {
                        f.get("id").and_then(|v| v.as_str()).map(str::to_string)
                    };
                    NormalizedAttachment {
                        href: None,
                        file_name,
                        mime_type,
                        byte_len: f.get("size").and_then(|v| v.as_i64()),
                        source_url,
                        ref_id,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn image_mime_for(filetype: &str) -> Option<String> {
    let m = match filetype {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        _ => return None,
    };
    Some(m.to_string())
}

/// Expand a message's `reactions[]` into one [`NormalizedReaction`] per
/// reacting user (resolved to a display label), so each is its own
/// searchable grid row. A count-only reaction (no `users` list) yields a
/// single row labelled with the count.
fn build_reactions(
    source_id: &str,
    raw: &Value,
    m: &Message,
    user_labels: Lookup<'_, BTreeMap<String, String>>,
) -> Vec<NormalizedReaction> {
    let date_ms = ts_to_ms(&m.ts);
    let mut out = Vec::new();
    let Some(arr) = raw.get("reactions").and_then(|v| v.as_array()) else {
        return out;
    };
    for r in arr {
        let Some(name) = r.get("name").and_then(|v| v.as_str()) else {
            continue;
        };
        let emoji = emoji::lookup(name).unwrap_or_else(|| format!(":{name}:"));
        let users: Vec<&str> = r
            .get("users")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|u| u.as_str()).collect())
            .unwrap_or_default();
        if users.is_empty() {
            let count = r.get("count").and_then(|v| v.as_u64()).unwrap_or(1);
            // No per-user breakdown: one aggregate row, keyed with an
            // empty user component.
            let id = ids::reaction(source_id, &m.team_id, &m.channel_id, &m.ts, name, "");
            out.push(NormalizedReaction {
                count: 1,
                reaction_uuid: id.uuid.clone(),
                reactor_handle: None,
                // A count, not a person.
                reactor_kind: AuthorKind::Unknown,
                reactor_display: format!("{count}"),
                emoji,
                date_ms,
                source_ref: Some(UpstreamRef::new(id.entity_kind, id.natural_key)),
            });
        } else {
            for u in users {
                let id = ids::reaction(source_id, &m.team_id, &m.channel_id, &m.ts, name, u);
                out.push(NormalizedReaction {
                    count: 1,
                    reaction_uuid: id.uuid.clone(),
                    reactor_handle: Handle::slack(&m.team_id, u),
                    reactor_kind: AuthorKind::Named,
                    reactor_display: user_labels.get(u).cloned().unwrap_or_else(|| u.to_string()),
                    emoji: emoji.clone(),
                    date_ms,
                    source_ref: Some(UpstreamRef::new(id.entity_kind, id.natural_key)),
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod reaction_tests {
    use super::super::User;
    use super::*;
    use datalib_etl_render::inputs::Inputs;
    use serde_json::json;

    fn reacted(reactions: Value) -> Message {
        Message {
            team_id: "T1".into(),
            channel_id: "C1".into(),
            ts: "12604000100.000100".into(),
            thread_ts: None,
            effective_thread_ts: "12604000100.000100".into(),
            is_thread_root: true,
            user_id: Some("U1".into()),
            text: "Engage.".into(),
            ts_iso: None,
            raw_json: json!({ "reactions": reactions }),
        }
    }

    /// The regression: a toned reaction (Slack names it `+1::skin-tone-2`)
    /// showed as `👍:skin-tone-2:`. A workspace's own emoji keeps its name.
    #[test]
    fn a_reaction_shows_its_tone_and_a_custom_one_its_name() {
        let m = reacted(json!([
            { "name": "+1::skin-tone-2", "users": ["U2"], "count": 1 },
            { "name": "thinking_face", "count": 3 },
            { "name": "ferengi-dance", "users": ["U3"], "count": 1 },
        ]));
        let inputs = Inputs::default();
        let users = BTreeMap::new();
        let shown: Vec<String> =
            build_reactions("src", &m.raw_json, &m, inputs.lookup("users", &users))
                .into_iter()
                .map(|r| r.emoji)
                .collect();
        assert_eq!(shown, ["👍🏻", "🤔", ":ferengi-dance:"]);
    }

    /// A document says which parts of its messages nothing draws, once,
    /// on the first message holding one; a forward or a bot's section is
    /// drawn and says nothing.
    #[test]
    fn a_document_names_what_its_messages_hold_that_is_not_shown() {
        let with = |raw: Value| Message {
            raw_json: raw,
            ..reacted(json!([]))
        };
        let messages = [
            with(json!({"text": "Engage."})),
            with(json!({"blocks": [{"type": "call", "call_id": "R1"}]})),
            with(json!({"attachments": [{"is_share": true, "text": "Make it so."}]})),
            with(
                json!({"blocks": [{"type": "section", "text": {"type": "mrkdwn", "text": "Red alert"}}]}),
            ),
            with(json!({"attachments": [{"actions": [{"type": "button"}]}]})),
        ];
        let shared = Shared::default();
        let lookups = Lookups {
            shared: &shared,
            user_labels: BTreeMap::new(),
            channel_labels: BTreeMap::new(),
            account: None,
            org_name: None,
        };
        let render_unit = SlackThreadRenderUnit {
            thread_key: "T1#C1#12604000100.000100".to_string(),
            messages: messages.to_vec(),
            blobs: Default::default(),
            inputs: Inputs::default(),
        };
        let chat = build_chat(&lookups, &render_unit, "src");
        let items = &chat.render_units[0].items;
        let rows: Vec<usize> = items.iter().map(|item| item.problems.len()).collect();
        assert_eq!(rows, [0, 1, 0, 0, 0], "one row a document");
        let row = &items[1].problems[0];
        assert_eq!(row.severity, Some(Severity::Info));
        assert_eq!(
            row.sample,
            "2 messages here hold what is not shown: call block, attachment with nothing to show"
        );
        assert_eq!(items[2].text.as_deref(), Some("Engage.\n\n> Make it so."));
        assert_eq!(items[3].text.as_deref(), Some("Red alert"));
    }

    /// A message was put on its author's profile clock, which is where
    /// they are now: one from before a move read at the wrong hour.
    #[test]
    fn a_message_names_no_clock_whatever_its_authors_profile_says() {
        let user = |id: &str, tz: Option<&str>| User {
            user_id: id.into(),
            team_id: "T1".into(),
            name: None,
            real_name: None,
            display_name: None,
            email: None,
            title: None,
            avatar_url: None,
            tz: tz.map(str::to_string),
        };
        let mut shared = Shared::default();
        for u in [
            user("U1", Some("Europe/Paris")),
            user("U2", None),
            user("U3", Some("Not/A_Zone")),
        ] {
            shared.users.insert(u.user_id.clone(), u);
        }
        let by = |uid: &str, ts: &str| Message {
            user_id: Some(uid.into()),
            ts: ts.into(),
            ..reacted(json!([]))
        };
        let render_unit = SlackThreadRenderUnit {
            thread_key: "T1#C1#1767603600.000100".to_string(),
            messages: vec![
                by("U1", "1767603600.000100"),
                by("U1", "1783242000.000100"),
                by("U2", "1767603600.000200"),
                by("U3", "1767603600.000300"),
            ],
            blobs: Default::default(),
            inputs: Inputs::default(),
        };
        let chat = build_chat(&Lookups::new(&shared), &render_unit, "src");
        let offsets: Vec<_> = chat.render_units[0]
            .items
            .iter()
            .map(|i| i.date_offset_secs)
            .collect();
        assert_eq!(offsets, [None, None, None, None]);
    }

    /// A bot's message showed "unknown" for its author: most carry a
    /// `bot_id` and no user. It is named by what it posted under, else by
    /// what `bots.info` said, which the thread declares it read.
    #[test]
    fn a_bots_message_is_named_by_what_it_posted_under_or_its_bot() {
        let users = BTreeMap::from([("U1".to_string(), "Jean-Luc Picard".to_string())]);
        let bots = BTreeMap::from([("B_SENSORS".to_string(), "Sensor Array".to_string())]);
        let inputs = Inputs::default();
        let author = |user: Option<&str>, raw: Value| {
            let m = Message {
                user_id: user.map(str::to_string),
                raw_json: raw,
                ..reacted(json!([]))
            };
            let (name, kind) = author_of(
                &m,
                inputs.lookup("users", &users),
                inputs.lookup("bots", &bots),
            );
            (name, kind == AuthorKind::Unknown)
        };
        let named = |s: &str| (s.to_string(), false);
        assert_eq!(author(Some("U1"), json!({})), named("Jean-Luc Picard"));
        assert_eq!(
            author(
                None,
                json!({"bot_id": "B_SENSORS", "username": "Red Alert"})
            ),
            named("Red Alert"),
            "an integration's per-message name is what Slack shows"
        );
        assert_eq!(
            author(
                None,
                json!({"bot_id": "B_X", "bot_profile": {"name": "Holodeck"}})
            ),
            named("Holodeck")
        );
        assert_eq!(
            author(None, json!({"bot_id": "B_SENSORS"})),
            named("Sensor Array")
        );
        assert_eq!(author(None, json!({"bot_id": "B_GONE"})), named("B_GONE"));
        assert_eq!(author(Some("U_NEW"), json!({})), named("U_NEW"));
        assert_eq!(author(None, json!({})), ("Unknown".to_string(), true));
        let declared: Vec<(String, String)> = inputs
            .declared()
            .into_iter()
            .filter(|i| i.table == "bots")
            .map(|i| (i.table, i.id))
            .collect();
        assert!(
            declared.contains(&("bots".to_string(), "B_SENSORS".to_string())),
            "a thread whose bot is renamed renders again: {declared:?}"
        );
    }

    /// Only a root carries Slack's count, and one that is not a number
    /// is a problem on the root rather than a line that lies.
    #[test]
    fn a_root_counts_its_replies_and_a_count_that_is_no_number_is_a_problem() {
        let mut problems = Vec::new();
        let mut root = reacted(json!([]));
        root.raw_json["reply_count"] = json!(3);
        assert_eq!(
            replies_of(&root, 1, &mut problems),
            Some(Replies {
                upstream: 3,
                held: 1
            })
        );
        let reply = Message {
            is_thread_root: false,
            ..root.clone()
        };
        assert_eq!(replies_of(&reply, 1, &mut problems), None);
        assert!(problems.is_empty());

        root.raw_json["reply_count"] = json!("three");
        assert_eq!(replies_of(&root, 1, &mut problems), None);
        assert_eq!(problems.len(), 1, "{problems:?}");
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use datalib_etl_render::inputs::Inputs;

    use super::*;

    /// A thread sits in its channel, and the channel in its workspace:
    /// both places, keyed on Slack's own ids. The thread is what is read
    /// together, and a reply is a row inside it, not a reply edge.
    #[test]
    fn a_thread_sits_in_its_channel_in_its_workspace() {
        let message = |ts: &str, root: bool| Message {
            team_id: "T1".into(),
            channel_id: "C1".into(),
            ts: ts.into(),
            thread_ts: Some("12604000100.000100".into()),
            effective_thread_ts: "12604000100.000100".into(),
            is_thread_root: root,
            user_id: Some("U1".into()),
            text: "Engage.".into(),
            ts_iso: None,
            raw_json: serde_json::json!({}),
        };
        let mut shared = Shared {
            workspace: Some(super::super::Workspace {
                row_id: "T1".into(),
                team_id: "T1".into(),
                team_name: Some("Enterprise".into()),
                team_url: None,
                self_user_id: None,
            }),
            ..Default::default()
        };
        shared.channels.insert(
            "C1".into(),
            super::super::Channel {
                channel_id: "C1".into(),
                name: Some("bridge".into()),
                ..Default::default()
            },
        );
        let parsed = ParsedSlack {
            shared,
            threads: vec![SlackThreadRenderUnit {
                thread_key: "T1#C1#12604000100.000100".into(),
                messages: vec![
                    message("12604000100.000100", true),
                    message("12604000200.000100", false),
                ],
                blobs: Default::default(),
                inputs: Inputs::default(),
            }],
        };
        let mut docs = Vec::new();
        render_all(&parsed, "src", &Progress::noop(), &mut |d| {
            docs.push(d);
            Ok(())
        })
        .unwrap();
        let [doc] = docs.as_slice() else {
            panic!("one thread, one document")
        };
        let workspace = ids::workspace("src", "T1").uuid;
        let channel = ids::channel("src", "T1", "C1").uuid;
        let thread = ids::thread("src", "T1", "C1", "12604000100.000100").uuid;
        let tree: Vec<_> = doc
            .data_nodes
            .iter()
            .map(|n| {
                (
                    n.uuid.as_str(),
                    n.parent_uuid.as_deref(),
                    n.is_place,
                    n.read_together,
                    n.upstream_id.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            tree,
            [
                (workspace.as_str(), None, true, false, Some("T1")),
                (
                    channel.as_str(),
                    Some(workspace.as_str()),
                    true,
                    false,
                    Some("C1")
                ),
                (
                    thread.as_str(),
                    Some(channel.as_str()),
                    false,
                    true,
                    Some("C1#12604000100.000100")
                ),
            ]
        );
        assert_eq!(doc.data_nodes[0].name.as_deref(), Some("Enterprise"));
        assert_eq!(doc.data_nodes[1].name.as_deref(), Some("#bridge"));
        let messages: Vec<_> = doc.rows.iter().filter(|r| r.uuid != thread).collect();
        assert_eq!(messages.len(), 2);
        for row in messages {
            assert_eq!(row.parent_uuid.as_deref(), Some(thread.as_str()));
            assert_eq!(row.read_uuid.as_deref(), Some(thread.as_str()));
            assert_eq!(row.place_uuid.as_deref(), Some(channel.as_str()));
        }
        assert!(doc.edges.is_empty(), "{:?}", doc.edges);
    }

    /// A root opening with a ``` block was titled "```Kitchen".
    #[test]
    fn a_title_leaves_out_a_code_fence() {
        let inputs = Inputs::default();
        let none = BTreeMap::new();
        let labels = Labels {
            users: inputs.lookup("users", &none),
            channels: inputs.lookup("channels", &none),
            team_id: "T01",
        };
        assert_eq!(thread_title("```Kitchen\nx = 1```", labels), "Kitchen");
    }
}
