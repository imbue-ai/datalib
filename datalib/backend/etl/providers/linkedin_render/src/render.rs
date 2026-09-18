//! Render LinkedIn's message-shaped feeds into markdown via the shared
//! chat renderer.

use std::collections::BTreeMap;
use std::collections::HashMap;

use anyhow::Result;
use datalib_etl::blob_cas::BlobBundle;
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::render::{
    render_all as cc_render_all, RenderProfile, ENTITY_KIND_CONVERSATION,
};
use datalib_etl_chat_common::types::{ItemKind, NormalizedChat, NormalizedChatItem, NormalizedDoc};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::{changed_rows, Bucket, Input, Inputs};
use serde_json::Value;

use datalib_etl_linkedin::ingest::schema_raw::{message_tables, ns_id as uuid5};
use datalib_etl_linkedin::ingest::{db_path_for, RawDb};

use crate::processor::{FeedOutcome, Source};
use datalib_schema::providers::Provider;

/// Bump when the item-shape / column mapping changes meaningfully.
/// v3: `account` is the export owner's primary email (else profile
/// name) on every row, in place of the source name on connections.
pub const RENDER_VERSION: u32 = 3;

fn profile() -> RenderProfile {
    RenderProfile {
        stamp_precision: datalib_etl_chat_common::RecordStampPrecision::Seconds,
        provider: Provider::Linkedin,
        source_label: "LinkedIn".to_string(),
        chat_kind: "LinkedIn Chat".to_string(),
        message_kind: "LinkedIn Message".to_string(),
        reaction_kind: "LinkedIn Reaction".to_string(),
        chat_entity_kind: ENTITY_KIND_CONVERSATION,
        render_version: RENDER_VERSION,
    }
}

/// Render every message-shaped table under `raw_dir` into `out_dir`.
/// No-op if the raw store is absent; each individual table is skipped if
/// it's missing or empty. Conversations from different feeds keep
/// distinct ids (namespaced by table) so they never collide.
pub fn render(
    source: &Source<'_>,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<FeedOutcome> {
    let Source {
        raw_dir,
        out_dir,
        name: source_id,
        account,
        account_inputs,
        range,
    } = *source;
    let db_path = db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(FeedOutcome::default());
    }

    // One open for every table, not one per table: reopening a doltlite
    // store while the last connection is still closing is what makes a
    // later `dolt_commit` fail.
    let Some((by_table, changed, new_head)) = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            // Read at a commit: this store belongs to the download step, and
            // nothing committed means nothing to render from.
            let Some(db) = RawDb::open_reader(&db_path, range.pin).await? else {
                return Ok(None);
            };
            let pin = db.pin().expect("a reader is pinned at open").clone();
            let mut loaded = Vec::new();
            for table in message_tables() {
                // A feed the user didn't export has no table; treat a
                // load error as "absent" rather than failing the render.
                loaded.push((
                    table,
                    datalib_etl::doltlite_raw::load_payloads_with_id(
                        db.pool(),
                        datalib_etl::pin::Reads::At(&pin),
                        table,
                    )
                    .await
                    .unwrap_or_default(),
                ));
            }
            let changed = changed_rows(db.pool(), range, &pin, &message_tables()).await?;
            db.close().await;
            Ok::<_, anyhow::Error>(Some((loaded, changed, pin.commit().to_string())))
        })
    })?
    else {
        return Ok(FeedOutcome::default());
    };

    let mut chats: Vec<NormalizedChat> = Vec::new();
    for (table, rows) in &by_table {
        chats.extend(build_chats(table, rows, account, account_inputs));
    }

    // What to render: the chats the driver found stale, plus the ones a
    // new or changed row maps to — through the rows just loaded, since
    // the conversation id lives inside the payload. A removed row's chat
    // reaches here through the driver, having declared the row.
    let forward = changed.map(|changed| {
        let mut keys: std::collections::HashSet<String> = std::collections::HashSet::new();
        for chat in &chats {
            if chat
                .inputs
                .iter()
                .any(|i| changed.get(&i.table).is_some_and(|ids| ids.contains(&i.id)))
            {
                keys.insert(chat.chat_uuid.clone());
            }
        }
        keys
    });
    let render = range.narrow(forward.as_ref());
    let mut outcome = FeedOutcome {
        new_head: Some(new_head),
        buckets: render
            .iter()
            .flatten()
            .map(|key| Bucket {
                key: key.clone(),
                inputs: Vec::new(),
            })
            .collect(),
    };
    if let Some(render) = &render {
        chats.retain(|c| render.contains(&c.chat_uuid));
    }

    let blobs: HashMap<String, BlobBundle> = HashMap::new();
    let s = cc_render_all(
        &profile(),
        &chats,
        out_dir,
        source_id,
        &blobs,
        progress,
        on_doc_complete,
    )?;
    outcome.buckets.extend(s.buckets);
    Ok(outcome)
}

/// Rows as `(row id, payload)`: the id is what the conversation declares
/// it read, beside the account rows every document carries.
fn build_chats(
    table: &str,
    rows: &[(String, Value)],
    account: Option<&str>,
    account_inputs: &[Input],
) -> Vec<NormalizedChat> {
    // BTreeMap keeps conversation order stable across runs.
    let mut by_conv: BTreeMap<String, (Vec<&Value>, Inputs)> = BTreeMap::new();
    for (row_id, p) in rows {
        let conv = field(p, "CONVERSATION ID");
        let (rows, inputs) = by_conv.entry(conv.to_string()).or_default();
        inputs.read(table, row_id);
        rows.push(p);
    }

    let mut chats = Vec::with_capacity(by_conv.len());
    for (conv, (rows, inputs)) in by_conv {
        for input in account_inputs {
            inputs.read(&input.table, &input.id);
        }
        let mut items: Vec<NormalizedChatItem> = rows
            .iter()
            .map(|p| {
                let from = field(p, "FROM");
                let date = field(p, "DATE");
                let content = field(p, "CONTENT");
                NormalizedChatItem {
                    message_uuid: uuid5(&format!("msg:{table}:{conv}:{date}:{from}:{content}")),
                    author_id: nonempty(field(p, "SENDER PROFILE URL"))
                        .unwrap_or(from)
                        .to_string(),
                    author_display: nonempty(from).unwrap_or("Unknown").to_string(),
                    date_ms: parse_date_ms(date),
                    text: nonempty(content).map(str::to_string),
                    kind: ItemKind::Text,
                    attachments: Vec::new(),
                    reactions: Vec::new(),
                    system_note: None,
                    source_url: None,
                    kind_label: None,
                    source_ref: None,
                    is_aside: false,
                }
            })
            .collect();
        items.sort_by_key(|i| i.date_ms);

        let display = nonempty(field(rows[0], "CONVERSATION TITLE"))
            .map(str::to_string)
            .unwrap_or_else(|| participants(&rows));

        chats.push(NormalizedChat {
            inputs: inputs.declared(),
            path_prefix: None,
            id: format!("{table}:{conv}"),
            chat_uuid: uuid5(&format!("chat:{table}:{conv}")),
            display,
            title: None,
            author: None,
            account: account.map(str::to_string),
            project: None,
            external_id: Some(conv.clone()),
            // No public per-conversation URL in the message export.
            source_url: None,
            upstream_scope: None,
            org_uuid: None,
            org_name: None,
            buckets: vec![NormalizedDoc {
                orphan_reactions: Vec::new(),
                period_key: "all".to_string(),
                markdown_uuid: uuid5(&format!("doc:{table}:{conv}:all")),
                items,
            }],
        });
    }
    chats
}

/// Distinct participant names across a conversation, in first-seen
/// order, joined for the page title when LinkedIn gave no explicit one.
fn participants(rows: &[&Value]) -> String {
    let mut seen = Vec::new();
    for p in rows {
        for key in ["FROM", "TO"] {
            if let Some(name) = nonempty(field(p, key)) {
                if !seen.iter().any(|n| n == name) {
                    seen.push(name.to_string());
                }
            }
        }
    }
    if seen.is_empty() {
        "LinkedIn conversation".to_string()
    } else {
        seen.join(", ")
    }
}

fn field<'a>(p: &'a Value, key: &str) -> &'a str {
    p.get(key).and_then(Value::as_str).unwrap_or("")
}

fn nonempty(s: &str) -> Option<&str> {
    let t = s.trim();
    (!t.is_empty()).then_some(t)
}

/// TODO(problem-sink): a shape we don't recognize is dropped silently.
/// `None` is the right *value* for `created_at`, but nothing anywhere
/// records that we discarded something upstream actually sent — that is
/// only half of R1 ("drop, count, log; never abort, never hide"). The
/// problem sink is `problems` (`datalib_schema::problems`);
/// report `{field, reason: CoercionFailed, sample}` there as well as
/// returning `None`. Grep `TODO(problem-sink)` for every such site.
/// Parse LinkedIn's `2026-06-16 22:11:33 UTC` timestamp to unix millis,
/// or `None` on any shape we don't recognize.
pub(crate) fn parse_date_ms(s: &str) -> Option<i64> {
    let s = s.trim().trim_end_matches(" UTC").trim();
    datalib_time::parse_custom_strftime_assumed_utc(s, "%Y-%m-%d %H:%M:%S")
        .ok()
        .map(|t| t.to_unix_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn with_ids(rows: &[Value]) -> Vec<(String, Value)> {
        rows.iter()
            .enumerate()
            .map(|(i, v)| (format!("row-{i}"), v.clone()))
            .collect()
    }

    fn msg(conv: &str, from: &str, to: &str, date: &str, content: &str) -> Value {
        json!({
            "CONVERSATION ID": conv, "CONVERSATION TITLE": "",
            "FROM": from, "TO": to, "DATE": date, "CONTENT": content,
            "SENDER PROFILE URL": "",
        })
    }

    #[test]
    fn groups_by_conversation_and_sorts() {
        let payloads = vec![
            msg("c1", "A", "B", "2026-06-16 22:11:33 UTC", "second"),
            msg("c1", "B", "A", "2026-06-16 04:58:21 UTC", "first"),
            msg("c2", "A", "C", "2026-01-01 00:00:00 UTC", "other"),
        ];
        let chats = build_chats("messages", &with_ids(&payloads), None, &[]);
        assert_eq!(chats.len(), 2);
        let c1 = chats.iter().find(|c| c.id == "messages:c1").unwrap();
        assert_eq!(c1.buckets[0].items.len(), 2);
        assert_eq!(c1.buckets[0].items[0].text.as_deref(), Some("first"));
        assert_eq!(c1.display, "A, B");
    }

    #[test]
    fn parses_timestamp() {
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 6, 16)
            .unwrap()
            .and_hms_opt(22, 11, 33)
            .unwrap()
            .and_utc()
            .timestamp_millis();
        assert_eq!(parse_date_ms("2026-06-16 22:11:33 UTC"), Some(expected));
        // The `posts` feed sometimes omits the trailing " UTC".
        assert_eq!(parse_date_ms("2026-06-16 22:11:33"), Some(expected));
    }

    /// A date we can't read must produce no timestamp, not the epoch.
    /// The old helper's own doc comment admitted the consequence:
    /// "sorts such rows to the top."
    #[test]
    fn unparseable_dates_yield_none_not_the_epoch() {
        for bad in ["", "   ", "not a date", "2026-06-16", "16/06/2026 22:11:33"] {
            assert_eq!(
                parse_date_ms(bad),
                None,
                "parse_date_ms({bad:?}) fabricated a stamp"
            );
        }
    }

    #[test]
    fn undated_message_gets_a_null_timestamp() {
        let payloads = vec![msg("c1", "A", "B", "", "undated")];
        let chats = build_chats("messages", &with_ids(&payloads), None, &[]);
        assert_eq!(chats[0].buckets[0].items[0].date_ms, None);
    }
}
