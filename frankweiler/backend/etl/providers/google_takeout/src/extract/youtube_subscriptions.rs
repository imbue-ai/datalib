//! `YouTube and YouTube Music/subscriptions/subscriptions.csv` walker.
//!
//! Three-column CSV: `Channel Id,Channel Url,Channel Title`. PK is
//! `Channel Id` verbatim. Not event-shaped; `when_ts` stays NULL.

use std::path::Path;

use anyhow::{Context, Result};
use frankweiler_etl::bulk::bulk_upsert_in_tx;
use frankweiler_etl::file_checkpoint::{self, FileFingerprint};
use frankweiler_etl::progress::Progress;
use frankweiler_time::IsoOffsetTimestamp;
use serde_json::json;
use tracing::warn;

use super::db::RawDb;
use super::schema_raw::YoutubeSubscriptionRow;
use frankweiler_etl::doltlite_raw::WirePayload;

const FILE_REL: &str = "YouTube and YouTube Music/subscriptions/subscriptions.csv";
const SCOPE: &str = "google_takeout/youtube_subscriptions";

pub async fn ingest(db: &RawDb, root: &Path, progress: &Progress) -> Result<usize> {
    let path = root.join(FILE_REL);
    if !path.exists() {
        return Ok(0);
    }
    let fp = FileFingerprint::of(&path)?;
    let stamped = file_checkpoint::load(db.pool(), SCOPE).await?;
    if file_checkpoint::should_skip(&stamped, &fp) {
        return Ok(0);
    }
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut rows: Vec<YoutubeSubscriptionRow> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if i == 0 || line.trim().is_empty() {
            continue;
        }
        let cells = split_csv_row(line);
        if cells.len() < 3 {
            warn!(event = "youtube_subscriptions_short_row", row = i, line);
            continue;
        }
        let channel_id = cells[0].trim().to_string();
        let channel_url = cells[1].trim().to_string();
        let channel_title = cells[2].trim().to_string();
        if channel_id.is_empty() {
            continue;
        }
        let payload = json!({
            "channelId": channel_id,
            "channelUrl": channel_url,
            "channelTitle": channel_title,
        });
        rows.push(YoutubeSubscriptionRow {
            id_and_payload: WirePayload {
                id: channel_id,
                payload: payload.to_string(),
            },
            channel_title: Some(channel_title),
        });
    }
    let n = rows.len();
    progress.set_message(&format!("youtube_subscriptions: {n}"));

    let now = IsoOffsetTimestamp::now_local().to_rfc3339();
    let mut tx = db
        .pool()
        .begin()
        .await
        .context("begin youtube_subscriptions tx")?;
    bulk_upsert_in_tx(&mut tx, &rows, &now).await?;
    file_checkpoint::record_finished(&mut tx, SCOPE, &fp).await?;
    tx.commit()
        .await
        .context("commit youtube_subscriptions tx")?;
    Ok(n)
}

/// Minimal RFC 4180 row split: supports quoted fields with embedded
/// commas and `""`-escaped double-quotes. Channel titles can contain
/// commas (e.g. "Star Trek: The Next Generation, Official Channel"),
/// so the naive `split(',')` is wrong.
fn split_csv_row(line: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(c);
            }
        } else if c == '"' {
            in_quotes = true;
        } else if c == ',' {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    out.push(cur);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_split_handles_quoted_commas() {
        assert_eq!(
            split_csv_row(r#"UC1,https://x,"Star Trek: TNG, Official""#),
            vec!["UC1", "https://x", "Star Trek: TNG, Official"]
        );
    }

    #[test]
    fn csv_split_handles_escaped_quotes() {
        assert_eq!(
            split_csv_row(r#"UC1,u,"Q ""bait"""#),
            vec!["UC1", "u", r#"Q "bait""#]
        );
    }
}
