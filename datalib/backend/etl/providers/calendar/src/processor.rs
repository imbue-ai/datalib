//! The `calendar` source's ingest-step processor: which method the
//! config holds decides the downloader, and the processor owns the
//! store.

use std::path::PathBuf;

use anyhow::Result;
use async_trait::async_trait;

use datalib_etl::processor::{DataProcessor, PlanContext, RunCtx};
use datalib_etl_calendar_config::{CalendarConfig, CalendarMethod};
use datalib_etl_files::fingerprint_cache::{self, FingerprintCache};
use datalib_etl_web::http::LatchkeySettings;
use datalib_time::TimeWindow;

use crate::ingest;

pub async fn migrate(raw_dir: &std::path::Path) -> anyhow::Result<()> {
    let db = ingest::RawDb::open(&datalib_etl::raw_layout::entities_db(raw_dir)).await?;
    db.close().await;
    Ok(())
}

pub fn plan_ingest(
    ctx: PlanContext,
    config: CalendarConfig,
) -> Result<Vec<Box<dyn DataProcessor>>> {
    let method = match config.method()? {
        CalendarMethod::Google { calendars, window } => Method::Google {
            calendars: calendars.to_vec(),
            window: window.parse()?,
        },
        CalendarMethod::Caldav {
            server_url,
            calendars,
            window,
        } => Method::Caldav {
            server_url: server_url.to_string(),
            calendars: calendars.to_vec(),
            window: window.parse()?,
        },
        CalendarMethod::Calendly { window } => Method::Calendly {
            window: window.parse()?,
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
        window: TimeWindow,
    },
    Caldav {
        server_url: String,
        calendars: Vec<String>,
        window: TimeWindow,
    },
    Calendly {
        window: TimeWindow,
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
        let window = match &self.method {
            Method::Google { window, .. }
            | Method::Caldav { window, .. }
            | Method::Calendly { window } => {
                ingest::Window::resolve(*window, &datalib_time::parse_strict(ctx.now)?)?
            }
            Method::Ics { .. } => None,
        };
        let db = ingest::RawDb::open(&ingest::db_path_for(&self.raw_path)).await?;
        ctx.run_store(db.pool().clone(), None, |sealer| async {
            let result = match &self.method {
                Method::Google { calendars, .. } => {
                    ingest::google::fetch(ingest::google::FetchOptions {
                        db,
                        calendars: calendars.clone(),
                        window,
                        latchkey: self.latchkey.clone(),
                        progress: ctx.progress.clone(),
                        control: ctx.control.clone(),
                        sealer: Some(sealer),
                    })
                    .await
                }
                Method::Caldav {
                    server_url,
                    calendars,
                    ..
                } => {
                    ingest::caldav::fetch(ingest::caldav::FetchOptions {
                        db,
                        server_url: server_url.clone(),
                        calendars: calendars.clone(),
                        window,
                        latchkey: self.latchkey.clone(),
                        progress: ctx.progress.clone(),
                        control: ctx.control.clone(),
                        sealer: Some(sealer),
                    })
                    .await
                }
                Method::Calendly { .. } => {
                    ingest::calendly::fetch(ingest::calendly::FetchOptions {
                        db,
                        window,
                        latchkey: self.latchkey.clone(),
                        progress: ctx.progress.clone(),
                        control: ctx.control.clone(),
                        sealer: Some(sealer),
                    })
                    .await
                }
                Method::Ics { path } => {
                    let cache =
                        FingerprintCache::open(&fingerprint_cache::default_cache_path()?).await?;
                    ingest::ics_dir::fetch(ingest::ics_dir::FetchOptions {
                        db,
                        input_path: path.clone(),
                        cache,
                        progress: ctx.progress.clone(),
                        control: ctx.control.clone(),
                    })
                    .await
                }
            };
            Ok(result?.line())
        })
        .await
    }
}
