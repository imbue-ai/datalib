//! Fixture playback: a replayed request is answered from a tape on disk
//! and never reaches the network. A test points the future it runs at its
//! own tape with [`scope`]; a process (a step launched by a test, the
//! fixture pipeline, an e2e backend) is pointed at one by [`PLAYBACK_ENV`]
//! and its siblings. A scope wins over the environment.
//!
//! The scope is task-local, like [`crate::retry::scope`], so two tests in
//! one binary never see each other's tape. A task spawned inside a scope
//! does not inherit it, and its requests go live.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::http::{fixture_key, HttpError, HttpRequest, HttpResponse};

/// A directory of tapes; per-request fixtures live at
/// `<dir>/<service>/<key>.json`. Set for a whole process, it switches every
/// provider transport in it into playback.
pub const PLAYBACK_ENV: &str = "DATALIB_HTTP_PLAYBACK";

/// Milliseconds to wait before answering each replayed request ([`Playback::delay`]).
pub const PLAYBACK_DELAY_ENV: &str = "DATALIB_HTTP_PLAYBACK_DELAY_MS";

/// A file whose presence holds every replayed request ([`Playback::hold`]).
pub const PLAYBACK_HOLD_ENV: &str = "DATALIB_HTTP_PLAYBACK_HOLD";

/// Like [`PLAYBACK_HOLD_ENV`], but only once a checkpoint has sealed
/// ([`Playback::hold_sealed`]).
pub const PLAYBACK_HOLD_SEALED_ENV: &str = "DATALIB_HTTP_PLAYBACK_HOLD_SEALED";

const HOLD_POLL: Duration = Duration::from_millis(50);

/// Where replayed requests are answered from, and how they are paced.
#[derive(Debug, Clone)]
pub struct Playback {
    root: PathBuf,
    delay: Duration,
    hold: Option<PathBuf>,
    hold_sealed: Option<PathBuf>,
}

impl Playback {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Playback {
            root: root.into(),
            delay: Duration::ZERO,
            hold: None,
            hold_sealed: None,
        }
    }

    /// Wait this long before answering each request. A fixture answers
    /// instantly, which hides everything that depends on a download taking
    /// time: checkpoints sealing mid-run, a consumer starting on a partial
    /// store, the Manage screen showing a step in flight.
    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// While `file` exists no request is answered; the moment it is gone
    /// they are. A test that has to act on a download in flight holds the
    /// tape, acts, and releases it, instead of picking a delay and hoping
    /// the window is wide enough on a slow runner. A stop ends the wait as
    /// `Interrupted`, like a backoff.
    pub fn hold(mut self, file: impl Into<PathBuf>) -> Self {
        self.hold = Some(file.into());
        self
    }

    /// Like [`Playback::hold`], but a request waits only once its process
    /// has sealed a checkpoint, so a download publishes something before
    /// it parks.
    pub fn hold_sealed(mut self, file: impl Into<PathBuf>) -> Self {
        self.hold_sealed = Some(file.into());
        self
    }

    fn from_env() -> Option<Self> {
        let path_in = |name: &str| {
            std::env::var_os(name)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        Some(Playback {
            root: path_in(PLAYBACK_ENV)?,
            delay: delay_from_env(),
            hold: path_in(PLAYBACK_HOLD_ENV),
            hold_sealed: path_in(PLAYBACK_HOLD_SEALED_ENV),
        })
    }

    /// Answers `req` from the tape, after the delay and any hold. A stop
    /// during a hold, or a cut the interruption test makes here, is
    /// `Interrupted`.
    pub(crate) async fn answer(
        &self,
        req: &HttpRequest,
        stop: &datalib_etl::stop::StopFlag,
    ) -> Result<HttpResponse, HttpError> {
        let interrupted = || HttpError::Interrupted {
            service: req.service,
            url: req.url.clone(),
        };
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        let hold = self.hold.as_ref().or(self
            .hold_sealed
            .as_ref()
            .filter(|_| datalib_etl::raw_store::has_sealed_a_checkpoint()));
        if let Some(hold) = hold {
            if held(stop, hold).await {
                return Err(interrupted());
            }
        }
        match crate::interrupt::before_request().await {
            Some(crate::interrupt::Strike::Interrupted) => Err(interrupted()),
            None => lookup(req, &self.root).await,
        }
    }
}

impl From<&Path> for Playback {
    fn from(root: &Path) -> Self {
        Playback::at(root)
    }
}

impl From<PathBuf> for Playback {
    fn from(root: PathBuf) -> Self {
        Playback::at(root)
    }
}

impl From<&PathBuf> for Playback {
    fn from(root: &PathBuf) -> Self {
        Playback::at(root)
    }
}

tokio::task_local! {
    static SCOPED: Playback;
}

/// Runs `fut` with every request it makes on this task answered from
/// `playback` (a [`Playback`], or just its root).
pub async fn scope<F: Future>(playback: impl Into<Playback>, fut: F) -> F::Output {
    SCOPED.scope(playback.into(), fut).await
}

/// The playback in force here: this task's [`scope`], else the process's
/// environment, else none (live).
pub(crate) fn current() -> Option<Playback> {
    SCOPED
        .try_with(Playback::clone)
        .ok()
        .or_else(Playback::from_env)
}

/// Waits while `hold` exists; `true` when a stop ended the wait instead.
async fn held(stop: &datalib_etl::stop::StopFlag, hold: &Path) -> bool {
    while hold.exists() {
        if stop.requested() {
            return true;
        }
        tokio::time::sleep(HOLD_POLL).await;
    }
    false
}

fn delay_from_env() -> Duration {
    let Some(raw) = std::env::var_os(PLAYBACK_DELAY_ENV) else {
        return Duration::ZERO;
    };
    match raw.to_str().and_then(|s| s.trim().parse::<u64>().ok()) {
        Some(ms) => Duration::from_millis(ms),
        None => {
            tracing::warn!(
                value = %raw.to_string_lossy(),
                "{PLAYBACK_DELAY_ENV} is not a whole number of milliseconds; replaying with no delay"
            );
            Duration::ZERO
        }
    }
}

async fn lookup(req: &HttpRequest, root: &Path) -> Result<HttpResponse, HttpError> {
    let key = fixture_key(req);
    let path = root.join(req.service.as_str()).join(&key);
    let bytes = tokio::fs::read(&path).await.map_err(|_| {
        HttpError::PlaybackMiss(format!(
            "{}: no fixture for {} {} (key={})",
            path.display(),
            req.method.as_str(),
            req.url,
            key
        ))
    })?;
    serde_json::from_slice(&bytes)
        .map_err(|e| HttpError::PlaybackInvalid(format!("{}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{latchkey_curl, HttpService};

    fn tape(root: &Path, req: &HttpRequest, body: &str) {
        let file = root.join(req.service.as_str()).join(fixture_key(req));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let answer = HttpResponse {
            status: 200,
            headers: Default::default(),
            body: body.into(),
            duration_ms: 0,
        };
        std::fs::write(file, serde_json::to_vec(&answer).unwrap()).unwrap();
    }

    /// Two scopes in flight at once each answer from their own tape: the
    /// root a test sets is never another test's.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_scopes_in_flight_at_once_answer_from_their_own_tapes() {
        let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let req = HttpRequest::get(HttpService::Slack, "https://slack.com/api/auth.test");
        tape(a.path(), &req, "picard");
        tape(b.path(), &req, "riker");
        // The delay only keeps both requests pending together.
        let pace = Duration::from_millis(20);
        let (from_a, from_b) = tokio::join!(
            scope(Playback::at(a.path()).delay(pace), latchkey_curl(&req)),
            tokio::spawn({
                let (root, req) = (b.path().to_path_buf(), req.clone());
                async move { scope(Playback::at(root).delay(pace), latchkey_curl(&req)).await }
            }),
        );
        assert_eq!(from_a.unwrap().body_str(), "picard");
        assert_eq!(from_b.unwrap().unwrap().body_str(), "riker");
    }
}
