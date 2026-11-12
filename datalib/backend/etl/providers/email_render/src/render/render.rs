//! Email (JMAP) render: convert parsed threads into the shared
//! `chat-common` normalized model and delegate markdown / grid-row /
//! grid-row plumbing to [`datalib_etl_chat_common::render::ChatRenderer`].

use std::collections::{BTreeSet, HashMap, HashSet};

use super::ids;
use super::parse::{EmailStore, EmailThreadBucket, Shared};
use anyhow::{Context, Result};
use datalib_contact_schema::NormalizedContact;
use datalib_etl::blob_cas::{blake3_hex, BlobBundle, Described};
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::normalize::iso_to_ms;
use datalib_etl_chat_common::people;
use datalib_etl_chat_common::render::{Buckets, ChatRenderer, RenderProfile};
use datalib_etl_chat_common::types::{
    own_stamp_ms, AuthorKind, ItemKind, NormalizedChat, NormalizedChatItem, NormalizedDoc,
    Recipient, RecipientRole, UpstreamRef,
};
use datalib_etl_chat_common::unread::{is_unread, Marks};
use datalib_etl_chat_common::TextFormat;
use datalib_etl_email::ingest::db::{LoadedAttachment, LoadedEmail};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::html::{
    code_span_parts, escape_md_block, escape_md_inline, escape_text, md_code_block,
    without_bidi_controls,
};
use datalib_etl_render::inputs::Lookup;
use datalib_etl_render::pages::{batches, PageSize};
use datalib_handle::Handle;
use datalib_schema::plain_text::plain_text;
use datalib_schema::problems::{Outcome, Problem, ProblemRow, Reason, Scope, Severity, Stage};
use datalib_schema::providers::Provider;
use mail_parser::{AddressList, MessageParser, MessagePart, PartKind};

/// Bump when the item-shape / column mapping changes meaningfully.
/// v3: render via chat-common (+ quoted-text folding, label chips).
/// v4: an email with a missing or malformed `Date` header gets a null
///     `created_at` instead of a real-looking `1970-01-01T00:00:00` — see
///     `docs/dev/data_architecture_parse_and_render.md` §6. Malformed
///     `Date` headers are common in real mail, so this changes real
///     output and stale docs must be re-rendered.
/// v6: `account` is the mailbox's address rather than the JMAP account
///     id (`u432643a7`).
/// v7: v6 only managed that for mbox and Gmail — a JMAP account object
///     has no `id` field, so the lookup missed every Fastmail row.
/// v8: ids are minted through `datalib_id` under `Upstream(account_id)`,
///     every row carries its backpointer, and an email's id carries its
///     `received_at` in its leading bits (`datalib_id`'s v8 layout).
///     Every uuid moved, `chat_uuid` among them.
/// v10: the label line leads with Starred and Important, and leaves out
///     Gmail's All Mail.
/// v11: the author span carries the author's handle as `data-handle`.
/// v12: a recipients line (To, Cc) under the header, each with its handle.
/// v13: Bcc on the recipients line, and an `@` mention drawn as a chip.
/// v14: attachments and inline images are linked inside the stored
///     `.eml`, and no `blobs/` is written beside the page.
/// v16: the account's own address is a source contact marked as the
///     account, so search reads it as `me`.
/// v17: mail-parser 1.0: base64 without its final `=` decodes whole,
///     RFC 8621's body lists, and a `Fr om:` line is no longer From.
/// v18: whether an email is unread comes from its read mark, and every
///     thread declares the marks it reads.
/// v19: an email attached to an email is listed among the attachments.
/// v20: an email's stamp keeps the offset its `Date` was written in.
/// v21: a quote's attribution line is plain text, and an HTML body's
///     `<pre>` and `&amp;lt;` read as the sender wrote them.
/// v22: a body part is HTML only when its Content-Type says so: a
///     plain-text email went through the HTML converter, losing every
///     `<…>` and running its lines together.
pub const RENDER_VERSION: u32 = 22;

/// Which webmail to build each email's `↗` outlink for. Mirrors
/// `datalib_core::config::EmailOutlink`; the orchestrator maps the
/// config enum onto this one (the provider crate doesn't depend on core).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutlinkFormat {
    Gmail,
    Fastmail,
}

fn email_outlink(
    fmt: Option<OutlinkFormat>,
    em: &LoadedEmail,
    mailbox_labels: &[String],
) -> Option<String> {
    match fmt? {
        OutlinkFormat::Gmail => gmail_outlink(em.message_id.as_deref()?),
        OutlinkFormat::Fastmail => Some(fastmail_outlink(
            primary_mailbox(mailbox_labels),
            &em.id,
            &em.thread_id,
        )),
    }
}

fn gmail_outlink(message_id: &str) -> Option<String> {
    let id = message_id
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    if id.is_empty() {
        return None;
    }
    Some(format!(
        "https://mail.google.com/mail/u/0/#search/rfc822msgid:{}",
        percent_encode(id)
    ))
}

fn fastmail_outlink(mailbox: &str, email_id: &str, thread_id: &str) -> String {
    format!(
        "https://app.fastmail.com/mail/{}/{thread_id}.{email_id}",
        percent_encode(mailbox)
    )
}

fn primary_mailbox(labels: &[String]) -> &str {
    labels
        .iter()
        .find(|n| n.eq_ignore_ascii_case("inbox"))
        .or_else(|| labels.first())
        .map(String::as_str)
        .unwrap_or("Inbox")
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn profile() -> RenderProfile {
    RenderProfile {
        stamp_precision: ids::STAMP_PRECISION,
        provider: Provider::Email,
        source_label: "Mail".to_string(),
        chat_kind: "Email Thread".to_string(),
        message_kind: "Email".to_string(),
        reaction_kind: "Email Reaction".to_string(),
        chat_entity_kind: ids::KIND_THREAD,
        render_version: RENDER_VERSION,
        text_format: TextFormat::Markdown,
    }
}

/// The render params recorded alongside the cursor. Both knobs change
/// what the tree should contain for documents the upstream diff would
/// never surface, so a change to either re-renders everything.
/// The config key the render's label filter is written under.
const K_ONLY_RENDER_LABELS: &str = "only_render_labels";

pub fn render_params(outlink: Option<OutlinkFormat>, only_labels: &[String]) -> serde_json::Value {
    // Sorted so a reordered config list isn't mistaken for a change.
    let mut labels: Vec<&str> = only_labels.iter().map(String::as_str).collect();
    labels.sort_unstable();
    serde_json::json!({
        "outlink_format": outlink.map(|o| match o {
            OutlinkFormat::Gmail => "gmail",
            OutlinkFormat::Fastmail => "fastmail",
        }),
        "only_render_labels": labels,
    })
}

/// What [`render_all`] did with the threads it was handed.
pub struct Rendered {
    pub buckets: Buckets,
    /// The threads the label filter keeps out, by bucket key: left out
    /// on purpose, so their documents go.
    pub excluded: Vec<String>,
}

/// How one page of a store's render ended, for the processor to declare
/// before the next page is read.
pub struct PageEnd {
    /// The stale threads this page looked at, by bucket key: declared
    /// with nothing first, so one that builds no chat goes once the diff
    /// says its emails left.
    pub looked_at: Vec<String>,
    pub rendered: Rendered,
}

/// What every thread's render looks up: mailbox and account names, and
/// the mailboxes the label filter lets through.
struct Lookups {
    mailbox_name: HashMap<String, String>,
    account_label: HashMap<String, String>,
    /// `None` renders every thread.
    label_allow: Option<HashSet<String>>,
}

/// The configured label paths against the mailbox tree the chips use.
/// Resolution and exact-match semantics are shared with the download
/// filter via [`datalib_etl_email::mailbox_labels`], so a given label
/// means the same thing at both phases and across JMAP / mbox sources.
fn resolve_labels(
    shared: &Shared,
    only_labels: &[String],
) -> datalib_etl_email::mailbox_labels::Resolved {
    let nodes: Vec<datalib_etl_email::mailbox_labels::MailboxNode> = shared
        .mailboxes
        .iter()
        .filter_map(datalib_etl_email::mailbox_labels::MailboxNode::from_payload)
        .collect();
    datalib_etl_email::mailbox_labels::resolve(&nodes, only_labels)
}

/// Each configured label no mailbox has. It selects no thread, and if
/// every label is misspelled the source renders nothing, so a person has
/// to hear of it. Keyed under `config:`, as a download's configured
/// entries are, and reported whole every run, so correcting the config
/// clears it.
pub fn unmatched_label_rows(
    shared: &Shared,
    source_id: &str,
    only_labels: &[String],
    render_version: u32,
) -> Vec<ProblemRow> {
    resolve_labels(shared, only_labels)
        .unmatched
        .iter()
        .map(|label| {
            ProblemRow::new(
                source_id,
                Stage::Render,
                Scope::Entity(&format!("config:{K_ONLY_RENDER_LABELS}:{label}")),
                None,
                Outcome::Dropped,
                Problem::explained(
                    Reason::NotFound,
                    Some(K_ONLY_RENDER_LABELS.to_string()),
                    &format!(
                        "{label:?}: no mailbox with this label path, so it selects no thread; \
                         check spelling / parent path"
                    ),
                )
                .severity(Severity::Warning),
                Some(render_version),
            )
        })
        .collect()
}

impl Lookups {
    fn new(shared: &Shared, only_labels: &[String]) -> Self {
        // mailbox id → the name its label chip shows, empty for none.
        let mailbox_name = shared
            .mailboxes
            .iter()
            .filter_map(|m| {
                let id = m.get("id")?.as_str()?.to_string();
                Some((id, chip_name(m)))
            })
            .collect();
        let account_label = shared
            .accounts
            .iter()
            .filter_map(|(id, a)| Some((id.clone(), account_label_from_payload(id, a)?)))
            .collect();
        let label_allow =
            (!only_labels.is_empty()).then(|| resolve_labels(shared, only_labels).ids);
        Lookups {
            mailbox_name,
            account_label,
            label_allow,
        }
    }

    /// Thread-level inclusion: keep the whole thread if ANY of its
    /// emails is filed under an allowed mailbox, so conversations
    /// aren't fragmented across the filter boundary.
    fn allows(&self, bucket: &EmailThreadBucket) -> bool {
        self.label_allow.as_ref().is_none_or(|allow| {
            bucket.emails.iter().any(|em| {
                bucket
                    .joins
                    .mailboxes
                    .get(&em.id)
                    .is_some_and(|ids| ids.iter().any(|m| allow.contains(m)))
            })
        })
    }
}

/// Renders every thread `store` hands out, a page at a time; `on_page`
/// hears how each page ended before the next is read.
#[allow(clippy::too_many_arguments)]
pub fn render_store(
    store: &EmailStore,
    root: &std::path::Path,
    source_id: &str,
    outlink: Option<OutlinkFormat>,
    only_labels: &[String],
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
        "[render] email dolt_diff scan"
    );
    let lookups = Lookups::new(&store.shared, only_labels);
    let profile = profile();
    let mut renderer = ChatRenderer::new(
        &profile,
        root,
        source_id,
        store.threads,
        progress,
        on_doc_complete,
    );
    let mut pages = store.pages(size.buckets);
    while let Some(page) = store.next_page(&mut pages)? {
        let excluded = render_page(
            &lookups,
            &page.docs,
            Some(store),
            size.bytes,
            source_id,
            outlink,
            &mut renderer,
        )?;
        progress.inc(excluded.len() as u64);
        let looked_at = page
            .looked_at
            .iter()
            .map(|(a, t)| ids::thread(source_id, a, t).uuid)
            .collect();
        on_page(PageEnd {
            looked_at,
            rendered: Rendered {
                buckets: renderer.take_buckets(),
                excluded,
            },
        })?;
    }
    Ok(())
}

/// Renders `docs` as one page, their `.eml`s already in their bundles.
#[allow(clippy::too_many_arguments)]
pub fn render_all(
    shared: &Shared,
    docs: &[EmailThreadBucket],
    root: &std::path::Path,
    source_id: &str,
    outlink: Option<OutlinkFormat>,
    only_labels: &[String],
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<Rendered> {
    let lookups = Lookups::new(shared, only_labels);
    let profile = profile();
    let mut renderer = ChatRenderer::new(
        &profile,
        root,
        source_id,
        docs.len(),
        progress,
        on_doc_complete,
    );
    let excluded = render_page(
        &lookups,
        docs,
        None,
        u64::MAX,
        source_id,
        outlink,
        &mut renderer,
    )?;
    Ok(Rendered {
        buckets: renderer.finish().buckets,
        excluded,
    })
}

/// One document per thread the label filter lets through; the bucket
/// keys of the ones it keeps out. Each thread's `.eml`s are read from
/// `emls` a batch of at most `eml_bytes` ahead of its render: reading,
/// parsing and hashing them is most of the work.
fn render_page(
    lookups: &Lookups,
    docs: &[EmailThreadBucket],
    emls: Option<&EmailStore>,
    eml_bytes: u64,
    source_id: &str,
    outlink: Option<OutlinkFormat>,
    renderer: &mut ChatRenderer<'_>,
) -> Result<Vec<String>> {
    let (threads, filtered): (Vec<&EmailThreadBucket>, Vec<_>) = docs
        .iter()
        .filter(|bucket| !bucket.emails.is_empty())
        .partition(|bucket| lookups.allows(bucket));
    let excluded = filtered
        .iter()
        .map(|b| ids::thread(source_id, &b.account_id, &b.thread_id).uuid)
        .collect();
    let weights: Vec<u64> = threads.iter().map(|b| b.eml_bytes()).collect();
    for range in batches(&weights, eml_bytes) {
        let batch = &threads[range];
        let loaded;
        let batch: Vec<&EmailThreadBucket> = match emls {
            Some(emls) => {
                loaded = emls.load_emls(batch)?;
                loaded.iter().collect()
            }
            None => batch.to_vec(),
        };
        for bucket in batch {
            let (chat, bundle) = build_chat(
                source_id,
                bucket,
                &lookups.mailbox_name,
                &lookups.account_label,
                outlink,
            );
            renderer
                .render_chat(&chat, &bundle)
                .context("email chat-common render")?;
        }
    }
    Ok(excluded)
}

/// The `accounts` row is a JMAP `Account` object, a Gmail stand-in
/// shaped like one, or what the mbox config said. Only mbox and Gmail
/// carry an explicit address; JMAP's `name` is, per RFC 8620, "the
/// email address representing the owner of the account".
fn account_label_from_payload(id: &str, payload: &serde_json::Value) -> Option<String> {
    let s = |k: &str| payload.get(k).and_then(|v| v.as_str());
    datalib_etl_chat_common::account_label(id, s("emailAddress").or_else(|| s("email")), s("name"))
}

/// The account's address as a source contact marked as the account, so
/// a thread the owner wrote in or was sent carries it. A label that is
/// not an address (a JMAP name that is not one, a bare id) gives none.
fn account_contact(source_id: &str, label: Option<&String>) -> Option<NormalizedContact> {
    people::account_contact(source_id, Handle::email(label?))
}

fn build_chat(
    source_id: &str,
    bucket: &EmailThreadBucket,
    mailbox_name: &HashMap<String, String>,
    account_label: &HashMap<String, String>,
    outlink: Option<OutlinkFormat>,
) -> (NormalizedChat, BlobBundle) {
    let account_id = &bucket.account_id;
    let thread_id = ids::thread(source_id, account_id, &bucket.thread_id);
    let tuid = thread_id.uuid.clone();
    // The account row and each mailbox row are looked up per thread,
    // and declared as they are.
    let mailbox_name = bucket.inputs.lookup("mailboxes", mailbox_name);
    let account_label = bucket.inputs.lookup("accounts", account_label);
    let subject = bucket
        .emails
        .first()
        .and_then(|e| e.subject.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(no subject)".to_string());

    // The parts each page links, inside the `.eml` that holds them.
    let mut render_bundle = BlobBundle::new();
    let mut items: Vec<NormalizedChatItem> = Vec::with_capacity(bucket.emails.len());

    for em in &bucket.emails {
        let eml = bucket.blobs.get(&em.blob_id);
        let parsed_eml = eml
            .map(|b| ParsedEml::from_eml_bytes(b.bytes.as_deref().unwrap_or_default()))
            .unwrap_or_default();

        // Real downloadable attachments → bundle. Filled BEFORE the
        // inline parts so the inline pass can recognize a part that is
        // byte-for-byte one of them — Google Calendar ships the same
        // iCalendar payload both ways on every invite (an inline
        // `text/calendar; method=REQUEST` part plus an `invite.ics`
        // attachment part).
        let atts: &[LoadedAttachment] = bucket
            .joins
            .attachments
            .get(&em.id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        // `listed_hashes` holds only the ones that reach the "###
        // Attachments" list below — inline-dispositioned attachments are
        // filtered out of that list, so their bytes still need a body
        // reference or nothing would link them at all.
        let mut listed_hashes: HashSet<String> = HashSet::new();
        for a in atts {
            if !is_inline_attachment(a) {
                listed_hashes.insert(a.blob_id.clone());
            }
            if let Some(eml) = eml {
                let part = Described {
                    blake3: a.blob_id.clone(),
                    byte_len: a.size.unwrap_or(0).max(0) as u64,
                    content_type: a.content_type.clone(),
                };
                let name = a.name.as_deref().map(without_bidi_controls);
                render_bundle.add_part(a.blob_id.clone(), eml, part, name);
            }
        }

        // Inline `.eml` body parts → bundle, linked inside this email's
        // own `.eml`: a reply that repeats an image links its own copy.
        let mut inline_cid_to_href: HashMap<String, String> = HashMap::new();
        let mut loose_inline: Vec<String> = Vec::new();
        for inline in &parsed_eml.inline_parts {
            let b3 = blake3_hex(&inline.bytes);
            let ref_id = format!("inline:{b3}");
            let part = Described {
                blake3: b3.clone(),
                byte_len: inline.bytes.len() as u64,
                content_type: inline.content_type.clone(),
            };
            if !eml.is_some_and(|eml| render_bundle.add_part(ref_id.clone(), eml, part, None)) {
                continue;
            }
            match &inline.cid {
                Some(cid) => {
                    if let Some(href) = render_bundle.href_for(&ref_id) {
                        inline_cid_to_href.entry(cid.clone()).or_insert(href);
                    }
                }
                None => {
                    // A non-image part that the attachment list already
                    // links adds nothing as a second body link — and
                    // that is the shape a calendar invite takes. Images
                    // keep their inline preview even when also listed;
                    // seeing the picture beats a second link to it.
                    let is_image = inline
                        .content_type
                        .as_deref()
                        .is_some_and(|ct| ct.starts_with("image/"));
                    if (!is_image && listed_hashes.contains(&b3)) || loose_inline.contains(&ref_id)
                    {
                        continue;
                    }
                    loose_inline.push(ref_id);
                }
            }
        }

        let linked: HashMap<String, String> = atts
            .iter()
            .filter_map(|a| Some((a.blob_id.clone(), render_bundle.href_for(&a.blob_id)?)))
            .collect();

        // Body markdown (HTML → md, cid → the part's address), then fold
        // the quoted reply history.
        let body = email_body_markdown(&parsed_eml, atts, &linked, &inline_cid_to_href)
            .unwrap_or_default();
        let (fresh, quoted) = split_quoted(&body);
        // Only the fresh part: a quoted reply repeats the mentions of the
        // message it quotes.
        let (fresh, mentions) = super::mentions::mention_chips(&fresh);

        // Its flags and the mailboxes it is filed under: drawn above the
        // body, and kept out of the row's Contents.
        let mailboxes = mailboxes_for_email(em, bucket, mailbox_name);
        let labels: Vec<String> = flag_labels(bucket.joins.keywords.get(&em.id))
            .into_iter()
            .chain(mailboxes.iter().cloned())
            .collect();
        let mut text = fresh.trim_end().to_string();
        if let Some(q) = quoted {
            text.push_str("\n\n");
            text.push_str(&q);
        }
        // Loose inline parts the body never referenced.
        for ref_id in &loose_inline {
            let (Some(link), Some(fname)) = (
                render_bundle.href_for(ref_id),
                render_bundle.filename_for(ref_id),
            ) else {
                continue;
            };
            if text.contains(&link) {
                continue;
            }
            // Image syntax only for images. A non-image part emitted as
            // `![](…)` renders as a broken image rather than something
            // the reader can open.
            let is_image = render_bundle
                .get(ref_id)
                .and_then(|b| b.content_type.as_deref())
                .is_some_and(|ct| ct.starts_with("image/"));
            if is_image {
                text.push_str(&format!("\n\n![]({link})"));
            } else {
                text.push_str(&format!("\n\n[\\[file\\] {fname}]({link})"));
            }
        }
        // Attachment list (excluding inline parts already in the body).
        let trailing: Vec<&LoadedAttachment> =
            atts.iter().filter(|a| !is_inline_attachment(a)).collect();
        if !trailing.is_empty() {
            text.push_str("\n\n### Attachments\n");
            for a in trailing {
                let name = without_bidi_controls(a.name.as_deref().unwrap_or(&a.part_id));
                let label = escape_md_inline(&name);
                match linked.get(&a.blob_id) {
                    Some(href) => text.push_str(&format!("\n- [{label}]({href})")),
                    None => text.push_str(&format!("\n- {label} _(blob not materialized)_")),
                }
            }
        }

        // No `Date` header, or one we cannot parse, is a null
        // `created_at` — not the epoch — and the second is recorded.
        // There is no parent stamp to inherit here: an email thread's
        // items are the emails.
        let mut problems = Vec::new();
        let date_ms = own_stamp_ms(
            em.received_at.as_deref(),
            "received_at",
            iso_to_ms,
            &mut problems,
        );
        let email_id = ids::email(source_id, &em.account_id, &em.id, date_ms);
        // Every download path keeps a `Read` mark per email. Mail the
        // account sent is not exempted: it is unread only when upstream
        // says so, which a mail server rarely does.
        let unread = is_unread(
            &em.id,
            false,
            Marks {
                own: bucket.marks.get(&em.id),
                conversation: None,
            },
            |_, _| None,
        );
        items.push(NormalizedChatItem {
            message_uuid: email_id.uuid.clone(),
            author_handle: parsed_eml.from_handle.clone(),
            author_kind: AuthorKind::Named,
            author_display: if parsed_eml.from_display.is_empty() {
                "(unknown sender)".to_string()
            } else {
                parsed_eml.from_display.clone()
            },
            date_ms,
            // The sender's clock: Fastmail's `receivedAt` is the server's
            // UTC, and its `sentAt` keeps the `Date:` header's offset.
            date_offset_secs: [em.sent_at.as_deref(), em.received_at.as_deref()]
                .into_iter()
                .flatten()
                .find_map(datalib_time::written_offset_secs),
            text: (!text.trim().is_empty()).then_some(text),
            kind: ItemKind::Text,
            attachments: Vec::new(),
            reactions: Vec::new(),
            labels: labels.clone(),
            system_note: None,
            // Per-email `↗` outlink into the source webmail.
            source_url: email_outlink(outlink, em, &mailboxes),
            kind_label: None,
            source_ref: Some(UpstreamRef::new(email_id.entity_kind, email_id.natural_key)),
            is_aside: false,
            branch: Vec::new(),
            unread,
            replies: None,
            recipients: parsed_eml.recipients.clone(),
            mentions,
            problems,
        });
    }

    // The thread's title `↗` points at the root (first) email's outlink.
    let thread_source_url = items.first().and_then(|i| i.source_url.clone());
    let mut chat = NormalizedChat {
        named_after: Vec::new(),
        contacts: account_contact(source_id, account_label.get(account_id))
            .into_iter()
            .collect(),
        inputs: Vec::new(),
        id: tuid.clone(),
        chat_uuid: tuid.clone(),
        display: subject.clone(),
        title: Some(subject),
        author: None,
        account: Some(
            account_label
                .get(account_id)
                .cloned()
                .unwrap_or_else(|| account_id.clone()),
        ),
        project: None,
        external_id: Some(thread_id.natural_key),
        source_url: thread_source_url,
        upstream_account: Some(account_id.clone()),
        org_uuid: None,
        org_name: None,
        buckets: vec![NormalizedDoc {
            orphan_reactions: Vec::new(),
            period_key: "all".to_string(),
            markdown_uuid: tuid,
            source_ref: None,
            items,
        }],
    };
    // After the account lookup above, so it is declared too.
    chat.inputs = bucket.inputs.declared();
    (chat, render_bundle)
}

/// The name a mailbox's label chip shows; empty for one that says
/// nothing on a single email. Gmail's All Mail (a Takeout label) holds
/// every message; Fastmail's Archive, the same role, is a real folder.
fn chip_name(mailbox: &serde_json::Value) -> String {
    let field = |k: &str| mailbox.get(k).and_then(|v| v.as_str());
    match (field("name"), field("role")) {
        (Some("All Mail"), Some("archive")) => String::new(),
        (name, _) => name.unwrap_or("").to_string(),
    }
}

/// The JMAP keywords a person reads as labels, in a fixed order: Gmail's
/// Starred and Important arrive as `$flagged` and `$important`.
fn flag_labels(keywords: Option<&Vec<String>>) -> Vec<String> {
    let has = |k: &str| keywords.is_some_and(|kws| kws.iter().any(|x| x == k));
    [("$flagged", "Starred"), ("$important", "Important")]
        .into_iter()
        .filter(|(k, _)| has(k))
        .map(|(_, label)| label.to_string())
        .collect()
}

fn mailboxes_for_email(
    em: &LoadedEmail,
    bucket: &EmailThreadBucket,
    mailbox_name: Lookup<'_, HashMap<String, String>>,
) -> Vec<String> {
    bucket
        .joins
        .mailboxes
        .get(&em.id)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .map(|mid| {
            mailbox_name
                .get(mid)
                .cloned()
                .unwrap_or_else(|| mid.clone())
        })
        .filter(|s| !s.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

// Quoted-text folding (the Gmail-style trimmed-quote view)

/// Split a rendered email body into (fresh, quoted) where `quoted` is
/// the reply history to collapse into a `<details>`. Conservative: only
/// folds when a recognizable quote marker is found, and never when the
/// fresh part would be empty (don't hide the whole message).
fn split_quoted(body: &str) -> (String, Option<String>) {
    let lines: Vec<&str> = body.lines().collect();
    let mut cut: Option<usize> = None;

    for (i, raw) in lines.iter().enumerate() {
        let l = raw.trim();
        let is_attr = (l.starts_with("On ") && l.ends_with("wrote:"))
            || (l.starts_with("Le ") && l.ends_with("a écrit :"))
            || l.contains("-----Original Message-----")
            || l.starts_with("> On ") && l.ends_with("wrote:");
        if is_attr {
            cut = Some(i);
            break;
        }
    }

    // Fallback: a trailing contiguous run of blockquote lines (>= 2)
    // with nothing but blanks/quotes to the end of the body.
    if cut.is_none() {
        let mut start: Option<usize> = None;
        for (i, raw) in lines.iter().enumerate() {
            let l = raw.trim_start();
            if l.starts_with('>') {
                if start.is_none() {
                    start = Some(i);
                }
            } else if !l.is_empty() {
                start = None; // run broken by real content
            }
        }
        if let Some(s) = start {
            // Require the run to be more than one line to be worth folding.
            let quoted_lines = lines[s..].iter().filter(|l| !l.trim().is_empty()).count();
            if quoted_lines >= 2 {
                cut = Some(s);
            }
        }
    }

    let Some(cut) = cut else {
        return (body.to_string(), None);
    };
    let fresh = lines[..cut].join("\n");
    if fresh.trim().is_empty() {
        // The whole body is quoted — show it rather than hide everything.
        return (body.to_string(), None);
    }
    let quoted_body = lines[cut..].join("\n");
    // The attribution line is markdown; `<summary>` is raw HTML, where
    // markdown is not read, so it goes in as the text it shows.
    let summary = plain_text(lines[cut].trim().trim_start_matches('>'), usize::MAX);
    let summary = if summary.is_empty() || summary.len() > 120 {
        "Quoted text".to_string()
    } else {
        escape_text(&summary)
    };
    let details = format!("<details><summary>{summary}</summary>\n\n{quoted_body}\n\n</details>");
    (fresh, Some(details))
}

// .eml parsing (mail-parser) — body, inline parts, addresses.

/// A binary body part embedded in the `.eml` (cid-referenced inline
/// image, or an Apple-Mail-style "loose" inline part).
#[derive(Default, Clone)]
struct InlinePart {
    cid: Option<String>,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

/// Everything the renderer needs from the RFC 5322 `.eml`, extracted in
/// one mail-parser pass.
#[derive(Default)]
struct ParsedEml {
    from_display: String,
    from_handle: Option<Handle>,
    recipients: Vec<Recipient>,
    text_body: Vec<BodyPart>,
    html_body: Vec<BodyPart>,
    inline_parts: Vec<InlinePart>,
}

/// One text part of a body list, as its Content-Type says it is.
/// mail-parser's lists follow RFC 8621, where a `text/plain` part with
/// no HTML alternative is in the HTML list too: a list is not one kind.
enum BodyPart {
    Plain(String),
    Html(String),
}

/// The text parts of a body list. An inline image in the list is not
/// text: its bytes go to the blob CAS, not into the body string.
fn body_parts<'m>(parts: impl Iterator<Item = MessagePart<'m>>) -> Vec<BodyPart> {
    parts
        .filter_map(|part| {
            let text = part.text()?.into_owned();
            match part.kind() {
                PartKind::Html => Some(BodyPart::Html(text)),
                _ => Some(BodyPart::Plain(text)),
            }
        })
        .collect()
}

impl ParsedEml {
    fn from_eml_bytes(bytes: &[u8]) -> Self {
        let Some(msg) = MessageParser::default().parse(bytes) else {
            return Self::default();
        };
        let from_display = format_address(msg.from());
        let from_handle = msg
            .from()
            .and_then(|a| a.first())
            .and_then(|a| a.address())
            .and_then(Handle::email);
        let recipients = [
            (RecipientRole::To, msg.to()),
            (RecipientRole::Cc, msg.cc()),
            (RecipientRole::Bcc, msg.bcc()),
        ]
        .into_iter()
        .flat_map(|(role, addrs)| {
            addrs
                .into_iter()
                .flat_map(|a| a.mailboxes())
                .filter_map(move |a| {
                    let address = a.address().unwrap_or_default();
                    let name = a.name().unwrap_or_default();
                    let display = if name.is_empty() { address } else { name };
                    (!display.is_empty()).then(|| Recipient {
                        role,
                        display: display.to_string(),
                        handle: Handle::email(address),
                        drawn: true,
                    })
                })
        })
        .collect();
        let text_body = body_parts(msg.text_body());
        let html_body = body_parts(msg.html_body());
        // RFC 8621 lists an image shown in the body there and not again
        // among the attachments, so all three lists are read.
        let mut seen = HashSet::new();
        let mut inline_parts: Vec<InlinePart> = Vec::new();
        for part in msg
            .attachments()
            .chain(msg.text_body())
            .chain(msg.html_body())
        {
            if !matches!(part.kind(), PartKind::Binary | PartKind::InlineBinary)
                || !seen.insert(part.id())
            {
                continue;
            }
            let bytes = part.decoded().into_owned();
            let cid = part.content_id().map(str::to_string);
            let content_type = part.content_type().map(|ct| match ct.subtype() {
                Some(sub) => format!("{}/{}", ct.ctype(), sub),
                None => ct.ctype().to_string(),
            });
            inline_parts.push(InlinePart {
                cid,
                content_type,
                bytes,
            });
        }
        Self {
            from_display,
            from_handle,
            recipients,
            text_body,
            html_body,
            inline_parts,
        }
    }
}

fn format_address(addr: Option<AddressList<'_>>) -> String {
    let Some(addr) = addr else {
        return "(unknown sender)".to_string();
    };
    let mut parts: Vec<String> = Vec::new();
    for a in addr.mailboxes() {
        let email = a.address().unwrap_or_default();
        let name = a.name().unwrap_or_default();
        if !name.is_empty() {
            parts.push(format!("{name} <{email}>"));
        } else if !email.is_empty() {
            parts.push(email.to_string());
        }
    }
    if parts.is_empty() {
        "(unknown sender)".to_string()
    } else {
        parts.join(", ")
    }
}

fn is_inline_attachment(a: &LoadedAttachment) -> bool {
    a.disposition.as_deref() == Some("inline") || a.cid.is_some()
}

/// Render one email's body to markdown. Prefers the HTML body list when
/// it holds a real `text/html` part; falls back to the plain-text list.
fn email_body_markdown(
    parsed: &ParsedEml,
    attachments: &[LoadedAttachment],
    linked: &HashMap<String, String>,
    inline_cid_to_href: &HashMap<String, String>,
) -> Option<String> {
    let mut cid_to_blob = inline_cid_to_href.clone();
    for a in attachments {
        let (Some(cid), Some(href)) = (a.cid.as_deref(), linked.get(&a.blob_id)) else {
            continue;
        };
        cid_to_blob.insert(cid.to_string(), href.clone());
    }

    let has_html = parsed
        .html_body
        .iter()
        .any(|p| matches!(p, BodyPart::Html(_)));
    if has_html {
        let md = body_list_markdown(&parsed.html_body, &cid_to_blob);
        if !md.trim().is_empty() {
            return Some(md);
        }
    }
    let md = body_list_markdown(&parsed.text_body, &cid_to_blob);
    (!md.trim().is_empty()).then_some(md)
}

/// Each part as markdown by its own kind: HTML through htmd after its
/// `cid:` srcs point at the parts' addresses, plain text escaped.
fn body_list_markdown(parts: &[BodyPart], cid_to_blob: &HashMap<String, String>) -> String {
    parts
        .iter()
        .map(|part| match part {
            BodyPart::Html(html) => html_markdown(&rewrite_cid_srcs(html, cid_to_blob)),
            BodyPart::Plain(text) => plain_body_markdown(text),
        })
        .filter(|md| !md.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// An HTML body as markdown, its text reading as the sender wrote it.
/// Two of htmd's outputs need help: a `<pre>` with no `<code>` comes out
/// as its decoded text, so a `&lt;div&gt;` in it became a tag, and text
/// holding `&amp;lt;` comes out as `&lt;`, which markdown reads as `<`.
pub fn html_markdown(html: &str) -> String {
    let md = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "head"])
        .add_handler(vec!["pre"], pre_as_code_block)
        .build()
        .convert(html)
        .unwrap_or_default();
    entity_ampersands_escaped(&md)
}

/// A `<pre>` as a code block. One holding a `<code>` already comes out
/// fenced; one without is fenced here. htmd's one escape inside, a `\`
/// before a leading backtick or tilde, is taken off: a code block would
/// show it.
fn pre_as_code_block(
    handlers: &dyn htmd::element_handler::Handlers,
    element: htmd::Element,
) -> Option<htmd::element_handler::HandlerResult> {
    let inner = handlers.walk_children(element.node).content;
    let inner = inner.trim_matches('\n');
    if inner.starts_with("```") || inner.starts_with("~~~") {
        return Some(format!("\n\n{inner}\n\n").into());
    }
    let text = inner
        .strip_prefix('\\')
        .filter(|t| t.starts_with(['`', '~']))
        .unwrap_or(inner);
    Some(format!("\n\n{}\n\n", md_code_block("", text)).into())
}

/// htmd writes no character reference of its own, so one in its output
/// outside code is text the sender wrote (`&amp;lt;` in the HTML). Its
/// `&` is escaped, or markdown would read the reference as the character.
fn entity_ampersands_escaped(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut fence: Option<(char, usize)> = None;
    for line in md.split_inclusive('\n') {
        let trimmed = line.trim();
        let marker = ['`', '~']
            .into_iter()
            .map(|c| (c, trimmed.chars().take_while(|&x| x == c).count()))
            .find(|&(_, n)| n >= 3);
        match (fence, marker) {
            (Some((c, n)), Some((m, k))) if m == c && k >= n && trimmed.len() == k => {
                fence = None;
                out.push_str(line);
            }
            (Some(_), _) => out.push_str(line),
            (None, Some(open)) => {
                fence = Some(open);
                out.push_str(line);
            }
            (None, None) => {
                for (part, is_code) in code_span_parts(line) {
                    if is_code {
                        out.push_str(part);
                    } else {
                        push_entity_ampersands_escaped(&mut out, part);
                    }
                }
            }
        }
    }
    out
}

fn push_entity_ampersands_escaped(out: &mut String, text: &str) {
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let name_len = after
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '#'))
            .unwrap_or(after.len());
        let is_reference = name_len > 0 && after[name_len..].starts_with(';');
        out.push_str(if is_reference { "&amp;" } else { "&" });
        rest = after;
    }
    out.push_str(rest);
}

/// A `text/plain` body as markdown that reads as the sender typed it:
/// every line escaped as plain text, bare URLs made links, and the
/// leading `>` marks of a quoted reply kept as markdown quotes — they
/// mean one, and they are what [`split_quoted`] folds.
fn plain_body_markdown(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            let rest = line.trim_start_matches(['>', ' ', '\t']);
            let quote_marks = &line[..line.len() - rest.len()];
            format!("{quote_marks}{}", autolink_bare_urls(rest))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn rewrite_cid_srcs(html: &str, cid_to_blob: &HashMap<String, String>) -> String {
    if cid_to_blob.is_empty() {
        return html.to_string();
    }
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while i < html.len() {
        let Some(rel) = lower[i..].find("cid:") else {
            out.push_str(&html[i..]);
            break;
        };
        let pos = i + rel;
        let prev_char = html[..pos].chars().last();
        if !matches!(prev_char, Some('"') | Some('\'')) {
            out.push_str(&html[i..pos + 4]);
            i = pos + 4;
            continue;
        }
        let quote = prev_char.unwrap();
        let after = &html[pos + 4..];
        let Some(end_rel) = after.find(quote) else {
            out.push_str(&html[i..]);
            break;
        };
        let cid = &after[..end_rel];
        out.push_str(&html[i..pos]);
        if let Some(path) = cid_to_blob.get(cid) {
            out.push_str(path);
        } else {
            out.push_str("cid:");
            out.push_str(cid);
        }
        i = pos + 4 + end_rel;
    }
    out
}

fn autolink_bare_urls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        let Some(pos) = rest.find("http://").or_else(|| rest.find("https://")) else {
            out.push_str(&escape_md_block(rest));
            break;
        };
        out.push_str(&escape_md_block(&rest[..pos]));
        let after = &rest[pos..];
        let end = after
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\''))
            .unwrap_or(after.len());
        let mut url = &after[..end];
        while let Some(last) = url.chars().last() {
            if matches!(last, '.' | ',' | ')' | ']' | '!' | '?' | ';' | ':') {
                url = &url[..url.len() - last.len_utf8()];
            } else {
                break;
            }
        }
        if url.is_empty() {
            out.push_str(&escape_md_block(&after[..end]));
        } else {
            out.push('<');
            out.push_str(url);
            out.push('>');
            out.push_str(&escape_md_block(&after[url.len()..end]));
        }
        i += pos + end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A misspelled `only_render_labels` entry could render nothing, and
    /// only the log said why. The label a mailbox has is not reported, and
    /// a path resolves through its parent, as the download filter's does.
    #[test]
    fn a_label_no_mailbox_has_is_a_warning_naming_it() {
        let shared = Shared {
            accounts: Vec::new(),
            mailboxes: vec![
                serde_json::json!({"id": "M1", "name": "Bridge"}),
                serde_json::json!({"id": "M2", "name": "Logs", "parentId": "M1"}),
            ],
        };
        let labels = ["Bridge/Logs".to_string(), "Holodek".to_string()];
        let rows = unmatched_label_rows(&shared, "mail", &labels, 9);
        assert_eq!(rows.len(), 1, "{rows:?}");
        let row = &rows[0];
        assert_eq!(row.scope_key, "config:only_render_labels:Holodek");
        assert_eq!(row.reason, Reason::NotFound);
        assert_eq!(row.severity, Severity::Warning);
        assert_eq!(row.stage, Stage::Render);
        assert_eq!(row.field.as_deref(), Some("only_render_labels"));
        assert!(row.sample.contains("\"Holodek\""), "{}", row.sample);
        assert_eq!(unmatched_label_rows(&shared, "mail", &[], 9), vec![]);
    }

    /// A `text/plain` email is what the sender typed: a `<b>` in it is
    /// those three characters, not bold, and a URL in it is still a link.
    #[test]
    fn a_plain_text_body_reads_as_typed() {
        let typed = "<script>x</script> & co\n\
                     # not a heading\n\
                     see https://e.invalid/log?a=1&b=2.\n\
                     \n\
                     On Tue, Apr 14, Data <data@enterprise.invalid> wrote:\n\
                     > The warp core is at <99.7%>.\n\
                     > Proceed?";
        let md = plain_body_markdown(typed);
        assert_eq!(
            md,
            "&lt;script&gt;x&lt;/script&gt; &amp; co\n\
             \\# not a heading\n\
             see <https://e.invalid/log?a=1&b=2>.\n\
             \n\
             On Tue, Apr 14, Data &lt;data@enterprise.invalid&gt; wrote:\n\
             > The warp core is at &lt;99.7%&gt;.\n\
             > Proceed?"
        );
        let (fresh, quoted) = split_quoted(&md);
        assert!(
            fresh
                .trim_end()
                .ends_with("see <https://e.invalid/log?a=1&b=2>."),
            "{fresh}"
        );
        let quoted = quoted.expect("the reply history still folds");
        assert!(
            quoted.contains("> The warp core is at &lt;99.7%&gt;."),
            "{quoted}"
        );
    }

    /// An image placed between two text parts is in RFC 8621's body
    /// lists and not among its attachments; it still has to reach the
    /// page.
    #[test]
    fn an_image_between_two_text_parts_is_an_inline_part() {
        let eml = b"From: Data <data@enterprise.ufp>\r\n\
MIME-Version: 1.0\r\n\
Content-Type: multipart/mixed; boundary=\"b\"\r\n\r\n\
--b\r\nContent-Type: text/plain\r\n\r\nBefore.\r\n\
--b\r\nContent-Type: image/png\r\nContent-Disposition: inline\r\n\
Content-Transfer-Encoding: base64\r\n\r\niVBORw0KGgo=\r\n\
--b\r\nContent-Type: text/plain\r\n\r\nAfter.\r\n\
--b--\r\n";
        let parsed = ParsedEml::from_eml_bytes(eml);
        let images: Vec<_> = parsed
            .inline_parts
            .iter()
            .map(|p| (p.content_type.as_deref(), p.bytes.as_slice()))
            .collect();
        assert_eq!(
            images,
            [(Some("image/png"), b"\x89PNG\r\n\x1a\n".as_slice())]
        );
    }

    #[test]
    fn split_quoted_folds_attribution_and_keeps_fresh() {
        let body = "Sounds good, let's do it.\n\nOn Tue, Apr 14, Data wrote:\n> The warp core is at 99.7%.\n> Proceed?";
        let (fresh, quoted) = split_quoted(body);
        assert_eq!(fresh.trim(), "Sounds good, let's do it.");
        let q = quoted.expect("quoted folded");
        assert!(q.contains("<details>"));
        assert!(q.contains("On Tue, Apr 14, Data wrote:"));
        assert!(q.contains("The warp core is at 99.7%."));
    }

    /// The attribution line becomes the fold's `<summary>`, raw HTML: a
    /// tag in it hid everything after it on the page. It shows as the
    /// text it reads as, less the tag.
    #[test]
    fn a_tag_in_the_attribution_line_shows_as_typed() {
        let body = "ok\n\nOn Tue, \\*Q\\* <div style=\"display:none\"> wrote:\n> a\n> b";
        let (_, quoted) = split_quoted(body);
        let quoted = quoted.expect("quoted folded");
        assert!(
            quoted.starts_with("<details><summary>On Tue, *Q* wrote:</summary>"),
            "{quoted}"
        );
        let typed = plain_body_markdown("ok\n\nOn Tue, Data <data@e.invalid> wrote:\n> a\n> b");
        let (_, quoted) = split_quoted(&typed);
        assert!(quoted
            .expect("quoted folded")
            .starts_with("<details><summary>On Tue, Data &lt;data@e.invalid&gt; wrote:</summary>"),);
    }

    /// htmd wrote a `<pre>` without a `<code>` as its decoded text, so an
    /// escaped tag in it became a tag, and `&amp;lt;` as `&lt;`, which the
    /// page shows as `<`.
    #[test]
    fn an_html_body_reads_as_the_sender_wrote_it() {
        assert_eq!(
            html_markdown(
                "<p>The console printed:</p><pre>&lt;div style=&quot;display:none&quot;&gt;phaser&lt;/div&gt;</pre>"
            ),
            "The console printed:\n\n```\n<div style=\"display:none\">phaser</div>\n```"
        );
        assert_eq!(
            html_markdown("<pre><code class=\"language-rust\">a &lt; b</code></pre>"),
            "```rust\na < b\n```"
        );
        assert_eq!(
            html_markdown("<p>Type &amp;lt; or &amp;#60; for it. Tea &amp; biscuits.</p>"),
            "Type &amp;lt; or &amp;#60; for it. Tea & biscuits."
        );
        assert_eq!(
            html_markdown("<p><code>&amp;lt;</code></p>"),
            "`&lt;`",
            "code keeps its text"
        );
    }

    #[test]
    fn split_quoted_no_marker_leaves_body_whole() {
        let body = "Just a plain message with no quoting.";
        let (fresh, quoted) = split_quoted(body);
        assert_eq!(fresh, body);
        assert!(quoted.is_none());
    }

    #[test]
    fn split_quoted_all_quoted_is_not_hidden() {
        let body = "On Mon, X wrote:\n> everything is quoted";
        let (fresh, quoted) = split_quoted(body);
        assert_eq!(fresh, body);
        assert!(quoted.is_none());
    }

    #[test]
    fn gmail_outlink_uses_rfc822msgid_search() {
        let url = gmail_outlink("<abc.123@mail.example.com>").unwrap();
        assert_eq!(
            url,
            "https://mail.google.com/mail/u/0/#search/rfc822msgid:abc.123%40mail.example.com"
        );
        assert!(gmail_outlink("   ").is_none());
    }

    #[test]
    fn fastmail_outlink_is_mailbox_thread_email() {
        assert_eq!(
            fastmail_outlink("Inbox", "Em1", "Th1"),
            "https://app.fastmail.com/mail/Inbox/Th1.Em1"
        );
        // mailbox names are path-encoded
        assert_eq!(
            fastmail_outlink("Sent Items", "E", "T"),
            "https://app.fastmail.com/mail/Sent%20Items/T.E"
        );
    }

    #[test]
    fn primary_mailbox_prefers_inbox() {
        assert_eq!(
            primary_mailbox(&["Archive".into(), "Inbox".into()]),
            "Inbox"
        );
        assert_eq!(primary_mailbox(&["Work".into()]), "Work");
        assert_eq!(primary_mailbox(&[]), "Inbox");
    }
}

#[cfg(test)]
mod render_params_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn starred_and_important_lead_in_a_fixed_order() {
        let kws = vec!["$answered".into(), "$important".into(), "$flagged".into()];
        assert_eq!(flag_labels(Some(&kws)), vec!["Starred", "Important"]);
        assert!(flag_labels(Some(&vec!["$answered".into()])).is_empty());
        assert!(flag_labels(None).is_empty());
    }

    /// Gmail's All Mail says nothing about one email; Fastmail's Archive
    /// carries the same role and is a folder a person filed it in.
    #[test]
    fn all_mail_has_no_chip_and_archive_keeps_its_own() {
        assert_eq!(
            chip_name(&json!({"name": "All Mail", "role": "archive"})),
            ""
        );
        assert_eq!(
            chip_name(&json!({"name": "Archive", "role": "archive"})),
            "Archive"
        );
        assert_eq!(
            chip_name(&json!({"name": "Work/Projects"})),
            "Work/Projects"
        );
    }

    #[test]
    fn label_order_is_not_a_change() {
        // A reordered config list must not read as a config change and
        // re-render the whole tree.
        let a = render_params(None, &["b".into(), "a".into()]);
        let b = render_params(None, &["a".into(), "b".into()]);
        assert_eq!(a, b);
    }

    #[test]
    fn outlink_spellings_are_stable() {
        // These literals live in on-disk cursors; changing one would
        // invalidate every rendered tree in the field.
        assert_eq!(
            render_params(Some(OutlinkFormat::Gmail), &[])["outlink_format"],
            json!("gmail")
        );
        assert_eq!(
            render_params(Some(OutlinkFormat::Fastmail), &[])["outlink_format"],
            json!("fastmail")
        );
        assert_eq!(render_params(None, &[])["outlink_format"], json!(null));
    }

    #[test]
    fn distinct_configs_are_distinct_params() {
        assert_ne!(render_params(None, &[]), render_params(None, &["a".into()]));
        assert_ne!(
            render_params(None, &[]),
            render_params(Some(OutlinkFormat::Gmail), &[])
        );
    }
}

#[cfg(test)]
mod account_label_tests {
    use super::{account_contact, account_label_from_payload};
    use serde_json::json;

    /// The mailbox's address is the account's own handle, and a label
    /// that is only a name or an id names no handle at all.
    #[test]
    fn the_account_is_its_address_and_nothing_else() {
        let picard = account_contact("mail", Some(&"Picard@Enterprise.org".to_string())).unwrap();
        assert!(picard.account);
        let handles: Vec<&str> = picard
            .handles
            .iter()
            .filter_map(|h| h.handle.as_ref().map(|h| h.as_str()))
            .collect();
        assert_eq!(handles, ["email:picard@enterprise.org"]);
        assert_eq!(account_contact("mail", Some(&"Takeout".to_string())), None);
        assert_eq!(account_contact("mail", None), None);
    }

    /// The three shapes the `accounts` table holds. The JMAP one is the
    /// trap: it has no `id` (the id is the session map's key) and its
    /// address is in `name`, so keying on `payload.id` left every
    /// Fastmail row showing `u432643a7` — which the fixture, being mbox
    /// and Gmail, could not have caught.
    #[test]
    fn jmap_gmail_and_mbox_all_resolve_to_an_address() {
        let jmap =
            json!({"name": "me@fastmail.test", "isPersonal": true, "accountCapabilities": {}});
        assert_eq!(
            account_label_from_payload("u432643a7", &jmap).as_deref(),
            Some("me@fastmail.test")
        );
        let gmail =
            json!({"id": "me@gmail.test", "email": "me@gmail.test", "name": "me@gmail.test"});
        assert_eq!(
            account_label_from_payload("me@gmail.test", &gmail).as_deref(),
            Some("me@gmail.test")
        );
        let mbox = json!({"id": "takeout", "name": "Takeout", "emailAddress": "me@x.test"});
        assert_eq!(
            account_label_from_payload("takeout", &mbox).as_deref(),
            Some("me@x.test")
        );
        assert_eq!(
            account_label_from_payload("acct-1", &json!({})).as_deref(),
            Some("acct-1")
        );
    }
}
