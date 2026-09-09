//! Program-A `DataProcessor`s for the slack_api source (download + render).

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::http::LatchkeySettings;
use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_slack_config::{SlackApiSync, SlackConfig};

use crate::download;

/// Download wave: present iff `sync:` (managed).
pub fn plan_download(ctx: PlanContext, config: SlackConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    let name = ctx.name;
    let raw_path = config.common.raw_path().to_path_buf();
    let blob_size_limit_bytes = config.common.blob_size_limit_bytes;
    let event_tape_enabled = config.common.event_tape_enabled();
    let latchkey = config.latchkey_settings.clone();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.sync {
        procs.push(Box::new(SlackDownload {
            id: format!("slack/{name}/download"),
            raw_path,
            sync,
            blob_size_limit_bytes,
            event_tape_enabled,
            latchkey,
        }));
    }
    Ok(procs)
}

struct SlackDownload {
    id: String,
    raw_path: PathBuf,
    sync: SlackApiSync,
    blob_size_limit_bytes: Option<u64>,
    event_tape_enabled: bool,
    /// Which latchkey identity to authenticate as, forwarded whole from
    /// the source's `latchkey_settings:` block.
    latchkey: LatchkeySettings,
}

#[async_trait]
impl DataProcessor for SlackDownload {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = download::db_path_for(&self.raw_path);
        let mut db = download::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        // Slack owns its wire-event tape: mirror every upsert to JSONL when the
        // resolved shared config leaves it enabled. (The orchestrator used to
        // attach this; now the one provider that consumes it does.)
        if self.event_tape_enabled {
            let tape = Arc::new(datalib_etl::event_tape::EventTape::new(
                datalib_etl::raw_layout::events_dir(&self.raw_path),
            ));
            tracing::info!(
                source = %ctx.name,
                events_dir = %tape.dir().display(),
                "event tape enabled — mirroring upserts to JSONL",
            );
            db.attach_event_tape(tape);
        }
        let s = download::fetch(download::FetchOptions {
            db,
            channels: self.sync.channels.clone(),
            since: self
                .sync
                .since
                .clone()
                .unwrap_or_else(|| download::DEFAULT_SINCE.into()),
            refresh_window_days: self.sync.refresh_window_days.unwrap_or(0),
            members_only: !self.sync.all_channels && self.sync.channels.is_none(),
            media: self.sync.media,
            dms: self.sync.dms,
            dm_users: self.sync.dm_users.clone(),
            blob_size_limit_bytes: self.blob_size_limit_bytes,
            latchkey: self.latchkey.clone(),
            progress: ctx.progress.clone(),
            control: ctx.control.clone(),
        })
        .await?;
        let media = s
            .media
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ");
        let summary = format!(
            "msgs={} replies={} pruned={} media[{}]",
            s.messages, s.replies, s.pruned, media
        );
        Ok(session.finish(ctx, summary).await)
    }
}
