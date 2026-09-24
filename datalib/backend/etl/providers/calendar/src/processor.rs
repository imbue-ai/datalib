//! The ingest wave for the `calendar` source: which method the config
//! holds decides the downloader, and the processor owns the store.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::fingerprint_cache::{self, FingerprintCache};
use datalib_etl::http::LatchkeySettings;
use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_calendar_config::{CalendarConfig, CalendarMethod};

use crate::ingest;

pub fn plan_ingest(
    ctx: PlanContext,
    config: CalendarConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let method = match config.method()? {
        CalendarMethod::Google { calendars } => Method::Google {
            calendars: calendars.to_vec(),
        },
        CalendarMethod::Caldav {
            server_url,
            calendars,
        } => Method::Caldav {
            server_url: server_url.to_string(),
            calendars: calendars.to_vec(),
        },
        CalendarMethod::Ics(path) => Method::Ics { path: path.path() },
    };
    Ok(vec![Box::new(CalendarIngest {
        id: format!("calendar/{}/download", ctx.name),
        raw_path: config.common.raw_path().to_path_buf(),
        latchkey: config.latchkey_settings.clone(),
        method,
    })])
}

enum Method {
    Google {
        calendars: Vec<String>,
    },
    Caldav {
        server_url: String,
        calendars: Vec<String>,
    },
    Ics {
        path: PathBuf,
    },
}

pub struct CalendarIngest {
    id: String,
    raw_path: PathBuf,
    latchkey: LatchkeySettings,
    method: Method,
}

#[async_trait]
impl DataProcessor for CalendarIngest {
    fn id(&self) -> &str {
        &self.id
    }

    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String> {
        let entity_db = ingest::db_path_for(&self.raw_path);
        let db = ingest::RawDb::open(&entity_db).await?;
        let session = ctx.open_store(db.pool().clone(), entity_db).await;
        let summary = match &self.method {
            Method::Google { calendars } => {
                ingest::google::fetch(ingest::google::FetchOptions {
                    db,
                    calendars: calendars.clone(),
                    latchkey: self.latchkey.clone(),
                    progress: ctx.progress.clone(),
                    control: ctx.control.clone(),
                })
                .await?
            }
            Method::Caldav {
                server_url,
                calendars,
            } => {
                ingest::caldav::fetch(ingest::caldav::FetchOptions {
                    db,
                    server_url: server_url.clone(),
                    calendars: calendars.clone(),
                    latchkey: self.latchkey.clone(),
                    progress: ctx.progress.clone(),
                    control: ctx.control.clone(),
                })
                .await?
            }
            Method::Ics { path } => {
                ingest::ics_dir::fetch(ingest::ics_dir::FetchOptions {
                    db,
                    input_path: path.clone(),
                    cache: FingerprintCache::open(&fingerprint_cache::default_cache_path()?)
                        .await?,
                    progress: ctx.progress.clone(),
                    control: ctx.control.clone(),
                })
                .await?
            }
        };
        session.finish(ctx, summary.line()).await
    }
}
