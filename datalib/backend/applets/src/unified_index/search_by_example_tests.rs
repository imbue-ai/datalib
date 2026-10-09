//! Search, by example: a small TNG cast rendered through chat-common and
//! contact-common, how each row is filed (the snapshot), and which queries
//! find which rows (the table in the test). Read this first to learn the
//! search bar's keys; `docs/dev/contacts.md` §"Searching for a person"
//! says the rules in prose. Each row's id is a readable name where a
//! provider would mint a uuid.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use datalib_contact_schema::{ContactHandle, ContactKind, NormalizedContact};
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::render::ENTITY_KIND_CONVERSATION;
use datalib_etl_chat_common::types::{
    ItemKind, NormalizedChat, NormalizedChatItem, NormalizedDoc, Recipient, RecipientRole,
};
use datalib_etl_chat_common::{RecordStampPrecision, RenderProfile, TextFormat};
use datalib_etl_contact_common::{ContactDoc, ContactRenderProfile};
use datalib_etl_render::grid_index::{apply_one, open_index, RenderedMarkdown, WriteLock};
use datalib_handle::Handle;
use datalib_schema::providers::Provider;
use datalib_schema::search_terms::SearchTermKind;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection};

use super::tests::{index_over, search, sync_terms, uuids};

/// 2369-04-15 08:30:00 UTC.
const T0: i64 = 12602794200000;
const HOUR: i64 = 3_600_000;

fn email(addr: &str) -> Handle {
    Handle::email(addr).unwrap()
}

fn slack_user(user_id: &str) -> Handle {
    Handle::slack("T1701", user_id).unwrap()
}

fn message(uuid: &str, author: &str, handle: Handle, at: i64, text: &str) -> NormalizedChatItem {
    NormalizedChatItem {
        message_uuid: uuid.into(),
        recipients: Vec::new(),
        mentions: Vec::new(),
        author_handle: Some(handle),
        author_display: author.into(),
        date_ms: Some(at),
        text: Some(text.into()),
        kind: ItemKind::Text,
        attachments: Vec::new(),
        reactions: Vec::new(),
        labels: Vec::new(),
        system_note: None,
        source_url: None,
        kind_label: None,
        source_ref: None,
        is_aside: false,
        branch: Vec::new(),
        unread: false,
        problems: Vec::new(),
    }
}

/// A chat rendered whole into one document, whose id is the chat's own.
fn chat(
    uuid: &str,
    display: &str,
    account: Option<&str>,
    item: NormalizedChatItem,
) -> NormalizedChat {
    NormalizedChat {
        id: uuid.into(),
        chat_uuid: uuid.into(),
        display: display.into(),
        title: None,
        account: account.map(String::from),
        author: None,
        project: None,
        external_id: None,
        upstream_account: None,
        source_url: None,
        org_uuid: None,
        org_name: None,
        path_prefix: None,
        buckets: vec![NormalizedDoc {
            period_key: "all".into(),
            markdown_uuid: uuid.into(),
            source_ref: None,
            items: vec![item],
            orphan_reactions: Vec::new(),
        }],
        contacts: Vec::new(),
        inputs: Vec::new(),
    }
}

fn chat_profile(provider: Provider, label: &str, thread: &str, message: &str) -> RenderProfile {
    RenderProfile {
        provider,
        source_label: label.into(),
        chat_kind: thread.into(),
        message_kind: message.into(),
        reaction_kind: format!("{label} Reaction"),
        chat_entity_kind: ENTITY_KIND_CONVERSATION,
        stamp_precision: RecordStampPrecision::Seconds,
        render_version: 1,
        text_format: TextFormat::Plain,
    }
}

/// Riker writes to Picard, copying Troi, in Picard's mailbox.
fn riker_email() -> NormalizedChat {
    let mut item = message(
        "riker-email",
        "William Riker",
        email("riker@enterprise.org"),
        T0,
        "Data and Worf will join the away team.",
    );
    item.recipients = vec![
        Recipient {
            role: RecipientRole::To,
            display: "Jean-Luc Picard".into(),
            handle: Some(email("picard@enterprise.org")),
        },
        Recipient {
            role: RecipientRole::Cc,
            display: "Deanna Troi".into(),
            handle: Some(email("troi@enterprise.org")),
        },
    ];
    item.labels = vec!["Inbox".into(), "Away Missions".into()];
    chat(
        "riker-thread",
        "Away team roster",
        Some("picard@enterprise.org"),
        item,
    )
}

/// Worf asks Data, by an @-mention, for a diagnostic.
fn worf_slack() -> NormalizedChat {
    let mut item = message(
        "worf-slack",
        "Worf",
        slack_user("U0000003"),
        T0 + HOUR,
        "@Data, run a level-one diagnostic on the warp core.",
    );
    item.mentions = vec![slack_user("U0000002")];
    chat("bridge-thread", "#bridge", None, item)
}

fn card(uuid: &str, kind: ContactKind, name: &str, addresses: &[&str]) -> ContactDoc {
    let mut contact = NormalizedContact::new("contacts", uuid, kind);
    contact.names = vec![name.into()];
    contact.handles = addresses
        .iter()
        .map(|a| ContactHandle::email(None, *a))
        .collect();
    ContactDoc {
        contact,
        doc_uuid: uuid.into(),
        group_label: "Address Book".into(),
        member_handles: Vec::new(),
        upstream_account: None,
        inputs: Vec::new(),
    }
}

/// Picard's card, with two addresses and filed in the group "Senior
/// Staff", and that group's own card.
fn cards() -> Vec<ContactDoc> {
    let mut picard = card(
        "picard-card",
        ContactKind::Person,
        "Jean-Luc Picard",
        &["picard@enterprise.org", "jean-luc@chateau-picard.example"],
    );
    picard.contact.groups = vec!["Senior Staff".into()];
    let mut staff = card(
        "senior-staff-card",
        ContactKind::Group,
        "Senior Staff",
        &["senior-staff@enterprise.org"],
    );
    let members = [
        ("Jean-Luc Picard", "picard"),
        ("William Riker", "riker"),
        ("Deanna Troi", "troi"),
    ];
    staff.contact.members = members.iter().map(|(name, _)| name.to_string()).collect();
    staff.member_handles = members
        .iter()
        .map(|(_, user)| Some(email(&format!("{user}@enterprise.org"))))
        .collect();
    vec![picard, staff]
}

/// Renders the cast and commits every document to the root's grid index
/// the way the `grid_index` step does; returns each supplied term as
/// `(row, kind, value)`.
async fn render_cast(root: &Path) -> HashSet<(String, SearchTermKind, String)> {
    let mut docs: Vec<RenderedMarkdown> = Vec::new();
    let mut keep = |md: RenderedMarkdown| -> anyhow::Result<()> {
        docs.push(md);
        Ok(())
    };
    let none = HashMap::new();
    let progress = Progress::noop();
    datalib_etl_chat_common::render_all(
        &chat_profile(Provider::Email, "Mail", "Email Thread", "Email"),
        &[riker_email()],
        root,
        "mail",
        &none,
        &progress,
        &mut keep,
    )
    .unwrap();
    datalib_etl_chat_common::render_all(
        &chat_profile(Provider::Slack, "Slack", "Slack Thread", "Slack Message"),
        &[worf_slack()],
        root,
        "slack",
        &none,
        &progress,
        &mut keep,
    )
    .unwrap();
    // The real contacts source renders a person and a group under two
    // profiles that differ only in the kind.
    for (kind, doc) in ["Contact", "Contact group"].into_iter().zip(cards()) {
        let profile = ContactRenderProfile {
            provider: Provider::Contacts,
            source_label: "Contacts".into(),
            contact_kind: kind.into(),
            contact_entity_kind: "contact",
            account: None,
            render_version: 1,
        };
        datalib_etl_contact_common::render_all(
            &profile,
            &[doc],
            root,
            "contacts",
            &progress,
            &mut keep,
        )
        .unwrap();
    }

    let supplied = docs
        .iter()
        .flat_map(|md| &md.search_terms)
        .map(|t| (t.uuid.clone(), t.kind, t.value.clone()))
        .collect();
    let pool = open_index(&datalib_runtime::layout::grid_index_db(root))
        .await
        .unwrap();
    let lock = WriteLock::new(pool.clone());
    for md in &docs {
        apply_one(&lock, root, md).await.unwrap();
    }
    datalib_etl::doltlite_raw::commit_run(&pool, "the cast")
        .await
        .unwrap();
    pool.close().await;
    sync_terms(root).await;
    supplied
}

/// Each row's search-key columns (and the author its `from` and `author`
/// terms come from), then every search term it answers to, marked
/// `supplied` where a render handed it in and `derived` where
/// `search_terms_of` read it off the row's columns.
async fn how_it_is_filed(
    root: &Path,
    supplied: &HashSet<(String, SearchTermKind, String)>,
) -> String {
    const COLUMNS: [&str; 8] = [
        "kind",
        "channel",
        "account",
        "contact",
        "email",
        "phone",
        "author",
        "author_handle",
    ];
    let read_only = |path| SqliteConnectOptions::new().filename(path).read_only(true);
    let mut grid = read_only(datalib_runtime::layout::grid_index_db(root))
        .connect()
        .await
        .unwrap();
    let mut columns: BTreeMap<String, Vec<(&str, String)>> = BTreeMap::new();
    for col in COLUMNS {
        // Audited: `col` is one of the `grid_rows` column names above.
        let values: Vec<(String, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT uuid, {col} FROM grid_rows"
        )))
        .fetch_all(&mut grid)
        .await
        .unwrap();
        for (uuid, value) in values {
            let shown = columns.entry(uuid).or_default();
            if let Some(v) = value.filter(|v| !v.is_empty()) {
                shown.push((col, v));
            }
        }
    }
    grid.close().await.unwrap();

    let mut file = read_only(datalib_runtime::layout::search_terms_db(root))
        .connect()
        .await
        .unwrap();
    let terms: Vec<(String, i64, String)> = sqlx::query_as(
        "SELECT r.uuid, t.kind, v.value FROM terms t \
         JOIN rows r ON r.row_id = t.row_id JOIN vals v ON v.val_id = t.val_id \
         ORDER BY r.uuid, t.kind, v.value",
    )
    .fetch_all(&mut file)
    .await
    .unwrap();
    file.close().await.unwrap();

    let mut out = String::new();
    for (uuid, shown) in &columns {
        out.push_str(&format!("{uuid}\n  columns\n"));
        for (col, v) in shown {
            out.push_str(&format!("    {col:<14} {v}\n"));
        }
        out.push_str("  terms\n");
        for (_, code, value) in terms.iter().filter(|(u, _, _)| u == uuid) {
            let kind = SearchTermKind::from_code(*code).unwrap();
            let from = if supplied.contains(&(uuid.clone(), kind, value.clone())) {
                "supplied"
            } else {
                "derived"
            };
            out.push_str(&format!("    {:<10} {value:<40} {from}\n", kind.as_str()));
        }
        out.push('\n');
    }
    out
}

/// How rows are filed, and which queries find them: the rules of
/// `terms_keys.rs` and the grid's column keys, each shown on one row of
/// a small cast.
#[tokio::test]
async fn how_rows_are_filed_and_which_searches_find_them() {
    let tmp = tempfile::tempdir().unwrap();
    let supplied = render_cast(tmp.path()).await;
    let filed = how_it_is_filed(tmp.path(), &supplied).await;
    insta::with_settings!({
        description => "Each row's search-key columns (and the author its from and author \
                        terms come from), then every search term it answers to: derived from \
                        its columns by search_terms_of, or supplied by its render.",
        omit_expression => true,
    }, {
        insta::assert_snapshot!(filed);
    });

    let s = index_over(tmp.path()).await;
    for (q, want) in [
        // from: reads the author's handle and the name they were shown
        // under. An address is a handle whether or not it says `email:`.
        ("from:riker", vec!["riker-email"]),
        ("from:riker@enterprise.org", vec!["riker-email"]),
        ("from:email:riker@enterprise.org", vec!["riker-email"]),
        // Unquoted, any part of a name or handle.
        ("from:will", vec!["riker-email"]),
        // Quoted, the whole name, case-blind, and never a part of it.
        (r#"from:"william riker""#, vec!["riker-email"]),
        (r#"from:"Riker""#, vec![]),
        // Nobody wrote a contact's card.
        ("from:picard", vec![]),
        ("to:picard@enterprise.org", vec!["riker-email"]),
        // Troi was copied, not addressed; recipient: is To, Cc and Bcc.
        ("to:troi", vec![]),
        ("cc:troi", vec!["riker-email"]),
        ("recipient:troi", vec!["riker-email"]),
        ("mention:slack:T1701/U0000002", vec!["worf-slack"]),
        // A mention holds Data's handle and not his name, and the "Data"
        // written in Riker's email is text, which no key reads.
        ("mention:data", vec![]),
        // with: is any role on a message, or who a card is about.
        ("with:picard", vec!["picard-card", "riker-email"]),
        (
            "with:picard@enterprise.org",
            vec!["picard-card", "riker-email"],
        ),
        ("with:troi", vec!["riker-email"]),
        // The card's second address is in its `about` terms and nowhere
        // else; the email column holds only the first (below).
        ("with:jean-luc@chateau-picard.example", vec!["picard-card"]),
        // A group's card is about the group: its members are not its
        // terms, so with:troi above does not find it.
        (r#"with:"Senior Staff""#, vec!["senior-staff-card"]),
        // Any person at all. A thread's own row has none: its people
        // are on its messages.
        (
            "with:*",
            vec![
                "picard-card",
                "riker-email",
                "senior-staff-card",
                "worf-slack",
            ],
        ),
        (
            "-with:picard",
            vec![
                "bridge-thread",
                "riker-thread",
                "senior-staff-card",
                "worf-slack",
            ],
        ),
        ("label:away", vec!["riker-email"]),
        (r#"label:"Away Missions""#, vec!["riker-email"]),
        // A pasted address with no key is looked up as a handle in every
        // role. The mailbox's account is a `name` term, not a handle, so
        // riker-thread is not found.
        ("picard@enterprise.org", vec!["picard-card", "riker-email"]),
        // A column key is the whole value as stored, case and all: unlike
        // the terms keys, never in part and never case-blind.
        ("kind:Email", vec!["riker-email"]),
        ("kind:email", vec![]),
        (r#"kind:"Email Thread""#, vec!["riker-thread"]),
        ("channel:#bridge", vec!["bridge-thread", "worf-slack"]),
        (
            r#"channel:"Address Book""#,
            vec!["picard-card", "senior-staff-card"],
        ),
        (
            "account:picard@enterprise.org",
            vec!["riker-email", "riker-thread"],
        ),
        (r#"contact:"Jean-Luc Picard""#, vec!["picard-card"]),
        ("contact:Picard", vec![]),
        ("contact:*", vec!["picard-card", "senior-staff-card"]),
        ("email:picard@enterprise.org", vec!["picard-card"]),
        ("email:jean-luc@chateau-picard.example", vec![]),
        // Keys combine with AND: Troi's one role on the email is Cc.
        ("with:troi kind:Email -cc:troi", vec![]),
    ] {
        let r = search(&s, q, None, 50, None).await;
        assert!(r.refused.is_empty() && r.errors.is_empty(), "{q}: {r:?}");
        let mut got = uuids(&r);
        got.sort();
        assert_eq!(got, want, "{q}");
    }
}
