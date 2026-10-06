//! Post-pass enrichment from `~/Library/Application
//! Support/BeeperTexts/local-<bridge>/megabridge.db`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::{debug, info};

use datalib_etl::run_problems::RunProblems;
use serde_json::Value;

use super::db::RawDb;
use super::index_db::query_json;
use super::FetchSummary;

/// Maps the suffix of a `local-<X>` directory name to the canonical
/// network name our config uses. Mirrors
/// `index_db::account_patterns_for` but inverted: takes the bridge
/// tag, returns the network. Bridges not listed here are skipped.
fn network_for_local_bridge(local_suffix: &str) -> Option<&'static str> {
    Some(match local_suffix {
        "signal" => "signal",
        "whatsapp" => "whatsapp",
        "telegram" => "telegram",
        "discord" => "discord",
        "linkedin" => "linkedin",
        "twitter" => "twitter",
        "instagram" => "instagram",
        "facebook" => "facebook",
        "gmessages" => "sms",
        "imessage" => "imessage",
        "googlechat" => "googlechat",
        "slack" => "slack",
        _ => return None,
    })
}

#[derive(Debug, Default)]
pub struct EnrichSummary {
    /// Number of `events.external_event_id` cells we filled in.
    pub events_enriched: usize,
    /// Per-megabridge.db rows we *would* have inserted (no matching
    /// `events.native_event_id` in our doltlite). Surfaces gaps
    /// where megabridge has more than index.db.
    pub events_orphaned: usize,
}

/// A bridge database that will not read, or a data directory that will
/// not list, is reported to `found`: its events keep whatever
/// `external_event_id` an earlier run gave them.
pub async fn enrich(
    beeper_data_dir: &Path,
    dst: &RawDb,
    networks: &[String],
    summary: &mut FetchSummary,
    found: &RunProblems,
) -> Result<EnrichSummary> {
    let mut enrich = EnrichSummary::default();

    let mut entries = match tokio::fs::read_dir(beeper_data_dir).await {
        Ok(e) => e,
        Err(e) => {
            found.phase("megabridge", format!("{}: {e}", beeper_data_dir.display()));
            return Ok(enrich);
        }
    };

    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(e) => {
                found.phase("megabridge", format!("{}: {e}", beeper_data_dir.display()));
                break;
            }
        };
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        let Some(suffix) = name_str.strip_prefix("local-") else {
            continue;
        };
        let Some(network) = network_for_local_bridge(suffix) else {
            debug!(event = "beeper_megabridge_unknown_bridge", dir = %name_str, "a directory names a bridge this build does not know");
            continue;
        };
        if !networks.iter().any(|n| n == network) {
            // The user didn't ask for this network — skip even
            // though the megabridge.db exists.
            debug!(
                event = "beeper_megabridge_network_disabled",
                network = network,
                "this network is disabled in the config; not enriching from it"
            );
            continue;
        }
        let mb_path: PathBuf = entry.path().join("megabridge.db");
        if !mb_path.is_file() {
            debug!(event = "beeper_megabridge_no_db", dir = %name_str, "this bridge directory has no database");
            continue;
        }

        let rows = match read_bridge(&mb_path).await {
            Ok(rows) => rows,
            Err(e) => {
                found.phase(&format!("megabridge {network}"), format!("{e:#}"));
                continue;
            }
        };
        let per_bridge = apply_bridge(&rows, dst, network).await?;
        info!(
            event = "beeper_megabridge_enriched",
            network = network,
            enriched = per_bridge.events_enriched,
            orphaned = per_bridge.events_orphaned,
            "enriched events from a bridge database"
        );
        enrich.events_enriched += per_bridge.events_enriched;
        enrich.events_orphaned += per_bridge.events_orphaned;
    }
    summary.events_enriched = enrich.events_enriched;
    summary.events_orphaned = enrich.events_orphaned;
    Ok(enrich)
}

/// What one bridge database holds that the store wants: read whole
/// before anything is written, so a bridge that fails to read leaves
/// the store as it was.
struct BridgeRows {
    messages: Vec<Value>,
    reactions: Vec<Value>,
}

async fn read_bridge(mb_path: &Path) -> Result<BridgeRows> {
    // The UNIQUE (bridge_id, mxid) constraint on the message table
    // guarantees no fan-out.
    let messages = query_json(mb_path, "SELECT mxid, id, part_id FROM message;")
        .await
        .context("query megabridge.message")?;
    let reactions = query_json(
        mb_path,
        "SELECT mxid, message_id, message_part_id, emoji, emoji_id
         FROM reaction;",
    )
    .await
    .context("query megabridge.reaction")?;
    Ok(BridgeRows {
        messages,
        reactions,
    })
}

async fn apply_bridge(rows: &BridgeRows, dst: &RawDb, network: &str) -> Result<EnrichSummary> {
    let mut out = EnrichSummary::default();

    // ── messages ────────────────────────────────────────────────
    // For each message in the bridge's local store: pair its
    // bridge-native id (`id`, plus `:part_id` for multi-part) with
    // the Matrix event id (`mxid`).
    let pool = dst.pool();
    for r in &rows.messages {
        let mxid = r
            .get("mxid")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if mxid.is_empty() {
            continue;
        }
        let id = r
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if id.is_empty() {
            continue;
        }
        let part_id = r.get("part_id").and_then(|v| v.as_str()).unwrap_or("");
        let external = if part_id.is_empty() {
            id
        } else {
            format!("{id}:{part_id}")
        };

        let affected = sqlx::query(
            "UPDATE events
                SET external_event_id = ?
              WHERE native_event_id = ?
                AND source = 'beeper_index'
                AND network = ?",
        )
        .bind(&external)
        .bind(&mxid)
        .bind(network)
        .execute(pool)
        .await
        .with_context(|| format!("update events for mxid {mxid}"))?
        .rows_affected();

        if affected == 0 {
            out.events_orphaned += 1;
            debug!(
                event = "beeper_megabridge_orphan",
                kind = "message",
                network = network,
                mxid = %mxid,
                "a bridge row names an event the store does not have"
            );
        } else {
            out.events_enriched += affected as usize;
        }
    }

    // ── reactions ───────────────────────────────────────────────
    // megabridge stores reactions in their own table with their
    // own `mxid` (the Matrix event id of the reaction event
    // itself). We don't get a single bridge-native reaction UUID —
    // Signal-side, a reaction is identified by the composite
    // (sender, target message, emoji). Pack those as the
    // external_event_id so reactions are non-NULL on the same
    // column as messages.
    for r in &rows.reactions {
        let mxid = r
            .get("mxid")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        if mxid.is_empty() {
            continue;
        }
        let msg_id = r
            .get("message_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let msg_part = r
            .get("message_part_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        // Prefer `emoji_id` (bridge-internal stable identifier;
        // for plain unicode emojis it's the same as `emoji`).
        // Fall back to the unicode `emoji` text if `emoji_id` is
        // empty.
        let emoji_id = r
            .get("emoji_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| r.get("emoji").and_then(|v| v.as_str()))
            .unwrap_or_default();
        let target = if msg_part.is_empty() {
            msg_id.to_string()
        } else {
            format!("{msg_id}:{msg_part}")
        };
        let external = format!("{target}#{emoji_id}");

        let affected = sqlx::query(
            "UPDATE events
                SET external_event_id = ?
              WHERE native_event_id = ?
                AND source = 'beeper_index'
                AND network = ?
                AND event_type = 'REACTION'",
        )
        .bind(&external)
        .bind(&mxid)
        .bind(network)
        .execute(pool)
        .await
        .with_context(|| format!("update reaction events for mxid {mxid}"))?
        .rows_affected();

        if affected == 0 {
            out.events_orphaned += 1;
            debug!(
                event = "beeper_megabridge_orphan",
                kind = "reaction",
                network = network,
                mxid = %mxid,
                "a bridge row names an event the store does not have"
            );
        } else {
            out.events_enriched += affected as usize;
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_for_local_bridge_mapping() {
        assert_eq!(network_for_local_bridge("signal"), Some("signal"));
        assert_eq!(network_for_local_bridge("whatsapp"), Some("whatsapp"));
        assert_eq!(network_for_local_bridge("gmessages"), Some("sms"));
        assert_eq!(network_for_local_bridge("unknown"), None);
    }
}
