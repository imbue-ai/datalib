//! Render LinkedIn's message-shaped feeds into markdown via the shared
//! chat renderer.

use datalib_etl::processor::RenderPass;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use datalib_etl::blob_cas::BlobBundle;
use datalib_etl::grid_index::RenderedMarkdown;
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::render::{
    render_all as cc_render_all, RenderProfile, ENTITY_KIND_CONVERSATION,
};
use datalib_etl_chat_common::types::{ItemKind, NormalizedChat, NormalizedChatItem, NormalizedDoc};
use serde_json::Value;

use crate::download::schema_raw::{message_tables, ns_id as uuid5};
use crate::download::{db_path_for, RawDb};
use datalib_schema::providers::Provider;

/// Bump when the item-shape / column mapping changes meaningfully.
pub const RENDER_VERSION: u32 = 2;

fn profile() -> RenderProfile {
    RenderProfile {
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
    raw_dir: &Path,
    out_dir: &Path,
    source_name: &str,
    progress: &Progress,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
    // Every document this render considered, skipped ones included — the
    // caller hands it to `RunCtx::retain_documents`, which drops whatever
    // the store holds and this does not name.
    seen: &mut std::collections::HashSet<String>,
) -> Result<RenderPass> {
    let db_path = db_path_for(raw_dir);
    if !db_path.exists() {
        return Ok(RenderPass::Skipped);
    }

    // One open for every table, not one per table: reopening a doltlite
    // store while the last connection is still closing is what makes a
    // later `dolt_commit` fail.
    let Some(by_table) = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let db = RawDb::open_reader(&db_path).await?;
            // Read at a commit: this store belongs to the download step, and
            // nothing committed means nothing to render from.
            let Some(pin) = datalib_etl::pin::head(db.pool()).await? else {
                db.close().await;
                // `None` all the way out, not an empty value: an empty load is
                // indistinguishable from a source with nothing in it, and the
                // caller sweeps every document this pass did not name.
                return Ok(None);
            };
            datalib_etl::pin::install_views(db.pool(), &pin).await?;
            let mut loaded = Vec::new();
            for table in message_tables() {
                // A feed the user didn't export has no table; treat a
                // load error as "absent" rather than failing the render.
                loaded.push((
                    table,
                    db.load_payloads(datalib_etl::pin::Reads::At(&pin), table)
                        .await
                        .unwrap_or_default(),
                ));
            }
            db.close().await;
            Ok::<_, anyhow::Error>(Some(loaded))
        })
    })?
    else {
        // Nothing committed to read: this pass did not walk, so it must not
        // reach the retain sweep.
        return Ok(RenderPass::Skipped);
    };

    let mut chats: Vec<NormalizedChat> = Vec::new();
    for (table, payloads) in &by_table {
        chats.extend(build_chats(table, payloads));
    }

    let blobs: HashMap<String, BlobBundle> = HashMap::new();
    let s = cc_render_all(
        &profile(),
        &chats,
        out_dir,
        source_name,
        &blobs,
        progress,
        prior_fingerprints,
        on_doc_complete,
    )?;
    seen.extend(s.documents);
    Ok(RenderPass::Walked)
}

fn build_chats(table: &str, payloads: &[Value]) -> Vec<NormalizedChat> {
    // BTreeMap keeps conversation order stable across runs.
    let mut by_conv: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
    for p in payloads {
        let conv = field(p, "CONVERSATION ID");
        by_conv.entry(conv.to_string()).or_default().push(p);
    }

    let mut chats = Vec::with_capacity(by_conv.len());
    for (conv, rows) in by_conv {
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
                }
            })
            .collect();
        items.sort_by_key(|i| i.date_ms);

        let display = nonempty(field(rows[0], "CONVERSATION TITLE"))
            .map(str::to_string)
            .unwrap_or_else(|| participants(&rows));

        chats.push(NormalizedChat {
            id: format!("{table}:{conv}"),
            chat_uuid: uuid5(&format!("chat:{table}:{conv}")),
            display,
            title: None,
            account: None,
            project: None,
            external_id: Some(conv.clone()),
            // No public per-conversation URL in the message export.
            source_url: None,
            upstream_scope: None,
            org_uuid: None,
            org_name: None,
            buckets: vec![NormalizedDoc {
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
/// `None` is the right *value* for `when_ts`, but nothing anywhere
/// records that we discarded something upstream actually sent — that is
/// only half of R1 ("drop, count, log; never abort, never hide"). When
/// the problem sink exists (see
/// `docs/dev/data_lib_as_a_library/render_audit_2026_09_03.md` §4),
/// report `{field, reason: CoercionFailed, sample}` here as well as
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
        let chats = build_chats("messages", &payloads);
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
        let chats = build_chats("messages", &payloads);
        assert_eq!(chats[0].buckets[0].items[0].date_ms, None);
    }
}
