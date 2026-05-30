//! Post-pass enrichment from `~/Library/Application
//! Support/BeeperTexts/local-<bridge>/megabridge.db`.
//!
//! `index.db` gives us a bridge-agnostic message cache but strips
//! the upstream system's per-message ids (Signal message UUID,
//! WhatsApp internal id, …). For **local** bridges, those ids live
//! verbatim in the bridge's own `megabridge.db.message` table, which
//! also carries a `mxid` column that's exactly the Matrix event id
//! we already stored as `events.native_event_id`. A straight join
//! gives us `events.external_event_id` for free.
//!
//! This module runs *after* [`super::index_db::ingest`] and only
//! UPDATEs existing rows. Messages that exist in megabridge but
//! never made it into index.db are reported as a count and left
//! alone for now — a future pass can insert them with
//! `source = "beeper_megabridge"` once we decide what to do about
//! UUID coexistence.
//!
//! Cloud bridges (slackgo, googlechat, …) have no local megabridge
//! file. We just skip them silently.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{debug, info, warn};

use super::db::RawDb;
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

fn sqlite3_bin() -> String {
    std::env::var("BEEPER_SQLITE3").unwrap_or_else(|_| "sqlite3".to_string())
}

async fn query_json(db_path: &Path, sql: &str) -> Result<Vec<Value>> {
    // See index_db::query_json for why we use plain path + -readonly
    // rather than a `file:?immutable=1` URI — the latter silently
    // hides WAL contents, which Beeper Texts (a live writer) is
    // actively populating.
    let mut child = Command::new(sqlite3_bin())
        .arg("-json")
        .arg("-readonly")
        .arg(db_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn sqlite3")?;
    {
        let mut stdin = child.stdin.take().expect("stdin piped");
        stdin
            .write_all(sql.as_bytes())
            .await
            .context("write SQL to sqlite3 stdin")?;
    }
    let output = child.wait_with_output().await.context("wait sqlite3")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "sqlite3 failed (exit={:?}): {}",
            output.status.code(),
            stderr.trim()
        );
    }
    if output.stdout.is_empty() {
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("parse sqlite3 -json output ({} bytes)", output.stdout.len()))?;
    Ok(value.as_array().cloned().unwrap_or_default())
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

/// Walk every `local-*/megabridge.db` under `beeper_data_dir`, and
/// for each one whose canonical network is in `networks`, populate
/// `events.external_event_id` for matching rows in `dst`.
pub async fn enrich(
    beeper_data_dir: &Path,
    dst: &RawDb,
    networks: &[String],
    summary: &mut FetchSummary,
) -> Result<EnrichSummary> {
    let mut enrich = EnrichSummary::default();

    let mut entries = match tokio::fs::read_dir(beeper_data_dir).await {
        Ok(e) => e,
        Err(e) => {
            warn!(
                event = "beeper_megabridge_dir_read_failed",
                dir = %beeper_data_dir.display(),
                error = %e
            );
            return Ok(enrich);
        }
    };

    while let Some(entry) = entries.next_entry().await.context("read_dir next")? {
        let name = entry.file_name();
        let name_str = name.to_string_lossy().to_string();
        let Some(suffix) = name_str.strip_prefix("local-") else {
            continue;
        };
        let Some(network) = network_for_local_bridge(suffix) else {
            debug!(event = "beeper_megabridge_unknown_bridge", dir = %name_str);
            continue;
        };
        if !networks.iter().any(|n| n == network) {
            // The user didn't ask for this network — skip even
            // though the megabridge.db exists.
            debug!(event = "beeper_megabridge_network_disabled", network = network);
            continue;
        }
        let mb_path: PathBuf = entry.path().join("megabridge.db");
        if !mb_path.is_file() {
            debug!(event = "beeper_megabridge_no_db", dir = %name_str);
            continue;
        }

        match enrich_one(&mb_path, dst, network).await {
            Ok(per_bridge) => {
                info!(
                    event = "beeper_megabridge_enriched",
                    network = network,
                    enriched = per_bridge.events_enriched,
                    orphaned = per_bridge.events_orphaned,
                );
                enrich.events_enriched += per_bridge.events_enriched;
                enrich.events_orphaned += per_bridge.events_orphaned;
            }
            Err(e) => {
                warn!(
                    event = "beeper_megabridge_failed",
                    network = network,
                    db = %mb_path.display(),
                    error = %format!("{e:#}")
                );
            }
        }
    }
    summary.events_enriched = enrich.events_enriched;
    summary.events_orphaned = enrich.events_orphaned;
    Ok(enrich)
}

async fn enrich_one(
    mb_path: &Path,
    dst: &RawDb,
    network: &str,
) -> Result<EnrichSummary> {
    let mut out = EnrichSummary::default();

    // ── messages ────────────────────────────────────────────────
    // For each message in the bridge's local store: pair its
    // bridge-native id (`id`, plus `:part_id` for multi-part) with
    // the Matrix event id (`mxid`). The UNIQUE (bridge_id, mxid)
    // constraint on the message table guarantees no fan-out.
    let rows = query_json(mb_path, "SELECT mxid, id, part_id FROM message;")
        .await
        .context("query megabridge.message")?;

    let pool = dst.pool();
    for r in &rows {
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
                mxid = %mxid
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
    let rows = query_json(
        mb_path,
        "SELECT mxid, message_id, message_part_id, emoji, emoji_id
         FROM reaction;",
    )
    .await
    .context("query megabridge.reaction")?;
    for r in &rows {
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
                mxid = %mxid
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
