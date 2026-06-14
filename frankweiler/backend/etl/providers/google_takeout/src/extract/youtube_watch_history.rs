//! `YouTube and YouTube Music/history/watch-history.html` walker.
//!
//! MDL cell-per-entry shape, one cell per video watched. The cell's
//! anchors are `(video_url, video_title)` then `(channel_url,
//! channel_title)`; the visible text trailing the anchors is the
//! timestamp Google rendered. PK recipe:
//! `uuidv5(NS, "youtube:watch:{video_id}:{iso_ts}")`.

use std::path::Path;

use anyhow::{Context, Result};
use frankweiler_etl::bulk::bulk_upsert_in_tx;
use frankweiler_etl::file_checkpoint::{self, FileFingerprint};
use frankweiler_etl::progress::Progress;
use frankweiler_time::IsoOffsetTimestamp;
use serde_json::json;
use tracing::warn;

use super::db::RawDb;
use super::mdl_html;
use super::schema_raw::{ns_id, YoutubeWatchRow};
use super::time as time_parser;
use frankweiler_etl::doltlite_raw::WirePayload;

const FILE_REL: &str = "YouTube and YouTube Music/history/watch-history.html";
const SCOPE: &str = "google_takeout/youtube_watch_history";

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
    let html =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let mut rows: Vec<YoutubeWatchRow> = Vec::new();
    for cell in mdl_html::iter_cells(&html) {
        let anchors = mdl_html::iter_anchors(cell);
        // The first anchor is the video; channel anchor is second
        // when present. We tolerate cells that only have a video.
        let Some((video_url, video_title)) = anchors.first().cloned() else {
            continue;
        };
        let video_id = video_id_from_url(&video_url).unwrap_or_default();
        if video_id.is_empty() {
            warn!(event = "youtube_watch_skip_no_video_id", url = %video_url);
            continue;
        }
        let (channel_url, channel_title) = anchors
            .get(1)
            .cloned()
            .map(|(u, t)| (Some(u), Some(t)))
            .unwrap_or((None, None));
        let channel_id = channel_url
            .as_deref()
            .and_then(channel_id_from_url)
            .map(str::to_string);
        let when_str = mdl_html::last_timestamp_chunk(cell);
        let when_ts = when_str.as_deref().and_then(time_parser::parse_mdl_grid);
        let iso_for_id = when_ts
            .clone()
            .unwrap_or_else(|| when_str.clone().unwrap_or_default());
        let id = ns_id(&format!("youtube:watch:{video_id}:{iso_for_id}"));
        let payload = json!({
            "videoUrl": video_url,
            "videoId": video_id,
            "videoTitle": video_title,
            "channelUrl": channel_url,
            "channelId": channel_id,
            "channelTitle": channel_title,
            "whenStr": when_str,
        });
        rows.push(YoutubeWatchRow {
            id_and_payload: WirePayload {
                id,
                payload: payload.to_string(),
            },
            when_ts,
            video_id: Some(video_id),
            channel_id,
        });
    }
    let n = rows.len();
    progress.set_message(&format!("youtube_watch_history: {n}"));

    let now = IsoOffsetTimestamp::now_local().to_rfc3339();
    let mut tx = db
        .pool()
        .begin()
        .await
        .context("begin youtube_watch_history tx")?;
    bulk_upsert_in_tx(&mut tx, &rows, &now).await?;
    file_checkpoint::record_finished(&mut tx, SCOPE, &fp).await?;
    tx.commit()
        .await
        .context("commit youtube_watch_history tx")?;
    Ok(n)
}

fn video_id_from_url(url: &str) -> Option<String> {
    let key = "watch?v=";
    let start = url.find(key)? + key.len();
    let rest = &url[start..];
    let end = rest.find(['&', '#']).unwrap_or(rest.len());
    let id = &rest[..end];
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

fn channel_id_from_url(url: &str) -> Option<&str> {
    let key = "/channel/";
    let start = url.find(key)? + key.len();
    let rest = &url[start..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let id = &rest[..end];
    if id.is_empty() {
        None
    } else {
        Some(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_id_from_watch_url() {
        assert_eq!(
            video_id_from_url("https://www.youtube.com/watch?v=abc123&t=10s"),
            Some("abc123".to_string()),
        );
    }

    #[test]
    fn channel_id_from_channel_url() {
        assert_eq!(
            channel_id_from_url("https://www.youtube.com/channel/UCabc/videos"),
            Some("UCabc"),
        );
    }
}
