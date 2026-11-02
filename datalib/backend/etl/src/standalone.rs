//! A provider's `*-ingest` binary owns its raw store the way the ingest
//! step does: through [`RunCtx::run_store`], so it seals on the step's
//! cadence, commits when the download succeeds and closes the store on
//! every path. A store closed without that commit reads as empty, and the
//! next writer's open discards what it held.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use sqlx::sqlite::SqlitePool;

use crate::control::DownloadControl;
use crate::processor::RunCtx;
use crate::progress::{Progress, TracingSink};
use crate::raw_store::Sealer;
use crate::stop::StopFlag;

/// One command-line download of one source.
pub struct StandaloneRun {
    source: String,
    out: PathBuf,
    now: String,
    progress: Progress,
    control: DownloadControl,
}

impl StandaloneRun {
    /// Must be called inside a tokio runtime: it also makes Ctrl-C or
    /// SIGTERM raise the run's stop flag, as the step does, so the
    /// download ends at its next consistent point and that is sealed. A
    /// second signal exits at once, and the store keeps its last seal.
    pub fn start(source: &str, out: &Path) -> Self {
        let control = DownloadControl::default();
        stop_on_signal(control.stop.clone());
        Self {
            source: source.to_string(),
            out: out.to_path_buf(),
            now: datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339_secs(),
            progress: Progress::new(Arc::new(TracingSink::new(source))),
            control,
        }
    }

    /// For the download's `FetchOptions`, so the session's seals are
    /// announced and its stop flag is the one a signal raises.
    pub fn progress(&self) -> Progress {
        self.progress.clone()
    }

    pub fn control(&self) -> DownloadControl {
        self.control.clone()
    }

    /// `body` hands the [`Sealer`] to its `FetchOptions` and returns the
    /// run's summary. A run a signal stopped still commits where it
    /// stopped, and then comes back as an error so the exit code says it
    /// did not finish.
    pub async fn run_store<Fut>(
        &self,
        pool: SqlitePool,
        cas_pool: Option<SqlitePool>,
        body: impl FnOnce(Sealer) -> Fut,
    ) -> Result<String>
    where
        Fut: Future<Output = Result<String>>,
    {
        let ctx = RunCtx::new(
            &self.source,
            &self.out,
            &self.now,
            &self.progress,
            &self.control,
            crate::download_metrics::DownloadMetrics::new(),
            datalib_obs::diagnostics::Diagnostics::new(),
        );
        let summary = ctx.run_store(pool, cas_pool, body).await?;
        anyhow::ensure!(
            !self.control.stop.requested(),
            "a signal asked the run to stop; it stopped at a consistent point and committed \
             what it had: {summary}"
        );
        Ok(summary)
    }
}

fn stop_on_signal(stop: StopFlag) {
    use tokio::signal::unix::{signal, SignalKind};
    tokio::spawn(async move {
        let Ok(mut term) = signal(SignalKind::terminate()) else {
            tracing::warn!("could not install the SIGTERM handler; SIGTERM ends the run unsealed");
            return;
        };
        loop {
            let asked = tokio::select! {
                r = tokio::signal::ctrl_c() => r.is_ok(),
                _ = term.recv() => true,
            };
            if !asked {
                return;
            }
            if stop.requested() {
                tracing::warn!("interrupted again; exiting without a final commit");
                std::process::exit(130);
            }
            stop.request();
            tracing::info!(
                "interrupted; stopping at the next consistent point (again to quit now)"
            );
        }
    });
}
