//! The `todoist` source's download processor.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_todoist_config::{TodoistConfig, TodoistSync};
use datalib_etl_web::http::LatchkeySettings;

use crate::ingest;

pub async fn migrate(raw_dir: &std::path::Path) -> Result<()> {
    let db = ingest::RawDb::open(&datalib_etl::raw_layout::entities_db(raw_dir)).await?;
    db.close().await;
    Ok(())
}

/// Ingest wave: present iff `api`.
pub fn plan_ingest(ctx: PlanContext, config: TodoistConfig) -> Result<Vec<Box<dyn DataProcessor>>> {
    let raw_path = config.common.raw_path().to_path_buf();
    let mut procs: Vec<Box<dyn DataProcessor>> = Vec::new();
    if let Some(sync) = config.api {
        procs.push(Box::new(TodoistIngest {
            id: format!("todoist/{}/download", ctx.name),
            raw_path,
            sync,
            latchkey: config.latchkey_settings,
        }));
    }
    Ok(procs)
}

struct TodoistIngest {
    id: String,
    raw_path: PathBuf,
    sync: TodoistSync,
    latchkey: LatchkeySettings,
}

#[async_trait]
impl DataProcessor for TodoistIngest {
    fn id(&self) -> &str {
        &self.id
    }

    /// Seals as pages land.
    fn streams_output(&self) -> bool {
        true
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let db = ingest::RawDb::open(&ingest::db_path_for(&self.raw_path)).await?;
        let pool = db.pool().clone();
        ctx.run_store(pool, None, |sealer| async {
            let s = ingest::fetch(ingest::FetchOptions {
                latchkey: self.latchkey.clone(),
                sync: self.sync.clone(),
                progress: ctx.progress.clone(),
                control: ctx.control.clone(),
                sealer: Some(sealer),
                ..ingest::FetchOptions::new(db)
            })
            .await?;
            Ok(s.line())
        })
        .await
    }
}
