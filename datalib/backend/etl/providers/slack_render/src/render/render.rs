//! Slack render: convert parsed thread buckets into the shared
//! `chat-common` normalized model and delegate the markdown / grid-row /
//! grid-row plumbing to [`datalib_etl_chat_common::render::render_all`].

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use datalib_etl::progress::Progress;
use datalib_etl_chat_common::render::{Buckets, ChatRenderer, RenderProfile};
use datalib_etl_chat_common::types::{
    own_stamp_ms, AuthorKind, ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem,
    NormalizedDoc, NormalizedReaction, UpstreamRef,
};
use datalib_etl_chat_common::TextFormat;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_handle::Handle;

use super::mrkdwn::{emojize_shortcodes, mentioned_users, resolve_mentions, to_commonmark, Labels};
use super::parse::{ParsedSlack, Shared, SlackStore, SlackThreadBucket};
use super::{ids, slack_link, ts_to_ms, Message};
use datalib_etl_render::inputs::Lookup;
use datalib_etl_render::pages::{batches, PageSize};
use datalib_etl_render::processor::Unparsed;
use datalib_etl_slack::ingest::schema_raw::split_key;
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
pub const RENDER_VERSION: u32 = 13;

#[derive(Debug, Default)]
pub struct RenderSummary {
    pub threads_total: usize,
    pub threads_rendered: usize,
    /// Every thread rendered, by uuid — what the processor declares
    /// through `RenderCtx::declare_bucket`.
    pub buckets: Buckets,
}

/// How one page of a store's render ended, for the processor to declare
/// before the next page is read.
pub struct PageEnd {
    /// The stale threads this page looked at, by bucket key: declared
    /// with nothing first, so one with no message left goes.
    pub looked_at: Vec<String>,
    pub buckets: Buckets,
    /// `messages` rows of the page that would not read as JSON.
    pub unparsed: Vec<Unparsed>,
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
#[allow(clippy::too_many_arguments)]
pub fn render_store(
    store: &SlackStore,
    out_dir: &Path,
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
        out_dir,
        source_id,
        store.threads,
        progress,
        on_doc_complete,
    );
    let mut pages = store.pages(size.buckets);
    while let Some(page) = store.next_page(&mut pages)? {
        render_threads(
            &lookups,
            &page.threads,
            Some(store),
            size.bytes,
            source_id,
            &mut renderer,
        )?;
        let looked_at = page
            .looked_at
            .iter()
            .filter_map(|key| {
                let (team, channel, ts) = split_key(key)?;
                Some(ids::thread(source_id, team, channel, ts).uuid)
            })
            .collect();
        on_page(PageEnd {
            looked_at,
            buckets: renderer.take_buckets(),
            unparsed: page.unparsed,
        })?;
    }
    Ok(())
}

/// Renders every thread in `parsed`, its attachments already loaded.
pub fn render_all(
    parsed: &ParsedSlack,
    out_dir: &Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let lookups = Lookups::new(&parsed.shared);
    let profile = profile();
    let mut renderer = ChatRenderer::new(
        &profile,
        out_dir,
        source_id,
        parsed.threads.len(),
        progress,
        on_doc_complete,
    );
    render_threads(
        &lookups,
        &parsed.threads,
        None,
        u64::MAX,
        source_id,
        &mut renderer,
    )?;
    let cc = renderer.finish();
    Ok(RenderSummary {
        threads_total: parsed.threads.len(),
        threads_rendered: cc.docs_rendered,
        buckets: cc.buckets,
    })
}

/// One document per thread, each built just before it renders. Its
/// attachments are read from `blobs` a batch of at most `blob_bytes`
/// ahead.
fn render_threads(
    lookups: &Lookups<'_>,
    threads: &[SlackThreadBucket],
    blobs: Option<&SlackStore>,
    blob_bytes: u64,
    source_id: &str,
    renderer: &mut ChatRenderer<'_>,
) -> Result<()> {
    let weights: Vec<u64> = threads.iter().map(|t| t.attachment_bytes()).collect();
    for range in batches(&weights, blob_bytes) {
        let batch: Vec<&SlackThreadBucket> = threads[range].iter().collect();
        let loaded;
        let batch: Vec<&SlackThreadBucket> = match blobs {
            Some(store) => {
                loaded = store.load_blobs(&batch)?;
                loaded.iter().collect()
            }
            None => batch,
        };
        for bucket in batch {
            let chat = build_chat(lookups, bucket, source_id);
            renderer
                .render_chat(&chat, &bucket.blobs)
                .context("slack chat-common render")?;
        }
    }
    Ok(())
}

fn build_chat(
    lookups: &Lookups<'_>,
    bucket: &SlackThreadBucket,
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
    // bucket as it is asked for, so the thread declares exactly what
    // it read.
    let labels = Labels {
        users: bucket.inputs.lookup("users", &lookups.user_labels),
        channels: bucket.inputs.lookup("channels", &lookups.channel_labels),
        team_id: bucket.messages.first().map_or("", |m| m.team_id.as_str()),
    };
    let channels = bucket.inputs.lookup("channels", &shared.channels);
    let root: &Message = bucket
        .messages
        .iter()
        .find(|m| m.is_thread_root)
        .unwrap_or_else(|| bucket.messages.first().expect("non-empty thread bucket"));
    // `#general` for a channel, `@Jean-Luc Picard` for a DM, and
    // `#<channel_id>` when the channel row never arrived — the same
    // fallback this line has always had. See `Channel::display`.
    let cname = match channels.get(&root.channel_id) {
        Some(c) => c.display(labels.users, self_user_id),
        None => format!("#{}", root.channel_id),
    };
    let named_after = channels
        .get(&root.channel_id)
        .map(|c| c.named_after(labels.users, self_user_id, &root.team_id))
        .unwrap_or_default();
    let thread = ids::thread(source_id, &root.team_id, &root.channel_id, &root.ts);
    let thread_uuid = thread.uuid.clone();

    // A top-level message is read up to its conversation's mark, a
    // reply up to its thread's, which Slack keeps only for a thread
    // the account follows. The account's own messages are never
    // unread, whatever the mark says.
    let channel_mark = shared.read_marks.channels.get(&root.channel_id);
    let thread_mark = shared.read_marks.threads.get(&bucket.thread_key);
    let unread = |m: &Message| {
        let mark = if m.is_thread_root {
            channel_mark
        } else {
            thread_mark
        };
        m.user_id.as_deref() != self_user_id
            && mark.is_some_and(|last_read| ids::after_mark(&m.ts, last_read))
    };
    let items: Vec<NormalizedChatItem> = bucket
        .messages
        .iter()
        .map(|m| build_item(source_id, m, root, team_url, labels, unread(m)))
        .collect();

    // "#channel: <root snippet>" preserves the old scannable H1; the
    // bare "#channel" remains the conversation_name (via `display`).
    let title = format!("{cname}: {}", thread_title(&root.text, labels));

    NormalizedChat {
        named_after,
        path_prefix: None,
        id: thread_uuid.clone(),
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
        buckets: vec![NormalizedDoc {
            orphan_reactions: Vec::new(),
            period_key: "all".to_string(),
            markdown_uuid: thread_uuid,
            source_ref: None,
            items,
        }],
        // Each author's profile, read the way their label was.
        contacts: authors_of(bucket)
            .filter_map(|uid| bucket.inputs.lookup("users", &shared.users).get(uid))
            .filter_map(|u| u.contact(source_id))
            .collect(),
        inputs: bucket.inputs.declared(),
    }
}

/// Each user who wrote in the thread, once, in the order they first did.
fn authors_of(bucket: &SlackThreadBucket) -> impl Iterator<Item = &str> {
    let mut seen = std::collections::HashSet::new();
    bucket
        .messages
        .iter()
        .filter_map(|m| m.user_id.as_deref())
        .filter(move |u| seen.insert(*u))
}

fn build_item(
    source_id: &str,
    m: &Message,
    root: &Message,
    team_url: Option<&str>,
    labels: Labels<'_>,
    unread: bool,
) -> NormalizedChatItem {
    let author_display = m
        .user_id
        .as_deref()
        .and_then(|u| labels.users.get(u).cloned())
        .unwrap_or_else(|| m.user_id.clone().unwrap_or_else(|| "unknown".into()));
    let body = to_commonmark(m.text.trim_end(), labels);
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
    NormalizedChatItem {
        message_uuid: msg_id.uuid.clone(),
        author_handle: m
            .user_id
            .as_deref()
            .and_then(|u| Handle::slack(&m.team_id, u)),
        author_kind: AuthorKind::Named,
        author_display,
        date_ms,
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
        recipients: Vec::new(),
        mentions: mentioned_users(&m.text, &m.team_id),
        problems,
    }
}

fn thread_title(root_text: &str, labels: Labels<'_>) -> String {
    let resolved = resolve_mentions(root_text, labels);
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
        let emoji = emojize_shortcodes(&format!(":{name}:"));
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
