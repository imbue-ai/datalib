//! GitHub REST API client (`api.github.com`).
//!
//! The client itself is [`datalib_etl_forge_common::ForgeClient`],
//! shared with gitlab. What's GitHub-specific and stays here: the API
//! root, and the retry classifier below.
//!
//! Port of `_call_github_once` + `call_github` + `paginate` in
//! `src/download/github_web.py`.

use std::time::Duration;

use datalib_etl::http::{default_retryability, HttpResponse, Retryability};
use datalib_etl_forge_common::ForgeClient;

pub use datalib_etl_forge_common::{ForgeError as GitHubError, LATCHKEY_TIMEOUT, PER_PAGE};

pub const BASE: &str = "https://api.github.com";

/// Alias kept so call sites read as GitHub code; the type is shared.
pub type GitHubClient = ForgeClient;

/// A [`ForgeClient`] wired to the `github` latchkey service and the
/// classifier below.
pub fn github_client() -> ForgeClient {
    ForgeClient::new("github", github_retryability)
}

/// GitHub-specific retry classifier. The default classifier already treats
/// the *secondary* rate limit (HTTP 429) and 5xx as retryable; GitHub's
/// *primary* rate limit is instead a `403` with `x-ratelimit-remaining: 0`
/// plus an `x-ratelimit-reset` epoch telling us when the window resets. Map
/// that to a retry with the computed wait so the shared loop respects it.
fn github_retryability(resp: &HttpResponse) -> Retryability {
    if resp.status == 403 && resp.header("x-ratelimit-remaining") == Some("0") {
        let retry_after = resp.header("x-ratelimit-reset").and_then(|reset| {
            reset.parse::<i64>().ok().map(|ts| {
                let now = chrono::Utc::now().timestamp();
                Duration::from_secs(((ts - now).max(0) as u64).saturating_add(1))
            })
        });
        return Retryability::Retry { retry_after };
    }
    default_retryability(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn resp(status: u16, headers: &[(&str, &str)]) -> HttpResponse {
        HttpResponse {
            status,
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
            body: Vec::new(),
            duration_ms: 0,
        }
    }

    /// The whole reason github doesn't use `default_retryability`: a
    /// primary-rate-limit 403 must retry, not fail permanently.
    #[test]
    fn primary_rate_limit_403_is_retryable() {
        let r = resp(403, &[("x-ratelimit-remaining", "0")]);
        assert!(matches!(
            github_retryability(&r),
            Retryability::Retry { .. }
        ));
    }

    /// A 403 that is a real permission error still fails permanently.
    #[test]
    fn plain_403_is_not_retryable() {
        let r = resp(403, &[("x-ratelimit-remaining", "42")]);
        assert!(!matches!(
            github_retryability(&r),
            Retryability::Retry { .. }
        ));
        let bare = resp(403, &[]);
        assert!(!matches!(
            github_retryability(&bare),
            Retryability::Retry { .. }
        ));
    }
}
