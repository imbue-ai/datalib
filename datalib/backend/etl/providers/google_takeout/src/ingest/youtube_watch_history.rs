//! `YouTube and YouTube Music/history/watch-history.html` walker.

use datalib_etl::download_problems::SkippedRecord;
use datalib_etl::run_problems::RunProblems;
use datalib_etl_files::fsscan;
use datalib_problems::{Problem, Severity};

use anyhow::Result;
use datalib_etl::progress::Progress;
use datalib_etl_files::file_checkpoint::{self, SnapshotCounts};
use serde_json::json;

use super::db::RawDb;
use super::mdl_html;
use super::schema_raw::{ns_id, YoutubeWatchRow};
use super::time as time_parser;
use datalib_etl::doltlite_raw::WirePayload;

pub(crate) const FILE_REL: &str = "YouTube and YouTube Music/history/watch-history.html";
const SCOPE: &str = "google_takeout/youtube_watch_history";

pub async fn ingest(
    db: &RawDb,
    scan: &fsscan::Scan,
    progress: &Progress,
    found: &RunProblems,
) -> Result<SnapshotCounts> {
    let mut skipped = None;
    let n = file_checkpoint::ingest_snapshot(db.pool(), SCOPE, scan.file(FILE_REL), |bytes| {
        let skipped = skipped.insert(Vec::new());
        let html = String::from_utf8_lossy(bytes);
        let mut rows: Vec<YoutubeWatchRow> = Vec::new();
        let cells: Vec<&str> = mdl_html::iter_cells(&html).collect();
        if cells.is_empty() {
            return Err(super::unknown_layout(FILE_REL, "holds no activity cells"));
        }
        for &cell in &cells {
            let anchors = mdl_html::iter_anchors(cell);
            // The first anchor is the video; channel anchor is second
            // when present. We tolerate cells that only have a video.
            let Some((video_url, video_title)) = anchors.first().cloned() else {
                continue;
            };
            let video_id = video_id_from_url(&video_url).unwrap_or_default();
            if video_id.is_empty() {
                // A community post, an ad's redirect link, an account
                // page: entries the history lists that are not videos.
                skipped.push(SkippedRecord {
                    entry: cell.to_string(),
                    problem: Problem::lossy(
                        "youtube_watch_not_a_video",
                        Some("videoUrl".to_string()),
                        &video_url,
                    )
                    .severity(Severity::Warning),
                });
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
        super::require_some_read(FILE_REL, cells.len(), rows.len())?;
        Ok(rows)
    })
    .await?;
    // `None`: the file was unchanged, and last run's rows still hold.
    if let Some(skipped) = skipped {
        found.skipped("youtube_watch_history", skipped);
    }
    progress.set_message(&format!("youtube_watch_history: {}", n.written));
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
