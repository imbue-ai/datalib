//! A run that a service ended: it refused the credential, or the retry
//! guard gave up, and every request left would only fail the same way.
//! What came before is kept; the run fails only when nothing did.

use anyhow::Result;
use datalib_etl::download_problems::RunProblem;
use datalib_etl::run_problems::RunProblems;
use datalib_etl::upstream::Upstream;

/// Raised by whatever asked, and caught by [`settle`].
#[derive(Debug, thiserror::Error)]
#[error("{said}")]
pub struct RunEnded {
    pub said: String,
    pub upstream: Option<Upstream>,
}

impl RunEnded {
    pub fn new(e: impl std::fmt::Display, upstream: Option<Upstream>) -> Self {
        Self {
            said: e.to_string(),
            upstream,
        }
    }
}

/// `result`, with a [`RunEnded`] in it made a problem and a give-up when
/// `anything_came`, and the run's failure when nothing did.
pub fn settle(
    result: Result<()>,
    service: &str,
    anything_came: bool,
    found: &RunProblems,
) -> Result<()> {
    let ended = match result.map_err(|e| e.downcast::<RunEnded>()) {
        Ok(()) => return Ok(()),
        Err(Err(other)) => return Err(other),
        Err(Ok(ended)) => ended,
    };
    if !anything_came {
        return Err(Upstream::error(
            ended.upstream,
            format!("{service} refused the run before anything came: {ended}"),
        ));
    }
    let name = match ended.upstream {
        Some(Upstream::CredentialRefused) => "credential",
        _ => "gave_up",
    };
    let problem = RunProblem::phase(
        name,
        format!(
            "{service} stopped answering part-way through, so the run stopped; what it fetched \
             before is kept, and the next run carries on from there: {ended}"
        ),
    );
    found.gave_up(problem.detail.clone());
    found.push(problem);
    Ok(())
}
