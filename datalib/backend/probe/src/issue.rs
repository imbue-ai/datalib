//! What kind of trouble a sign-in or a probe ran into, read off the
//! text that reports it. The wizard says one sentence per kind, so the
//! person reads "Slack turned the stored sign-in away" rather than an
//! error chain; the chain stays beside it as the detail.
//!
//! Classified from text because that is all that survives the trip: a
//! probe's error is an `anyhow` chain printed by `datalib-step`, a
//! login's is latchkey's stderr. The patterns are the ones those texts
//! actually carry — the tests below are strings taken from real runs
//! against `datalib/ui/tests/e2e_auth/`'s fault matrix.

use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr, VariantArray};

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    EnumString,
    IntoStaticStr,
    VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum IssueKind {
    /// latchkey holds no credential for the service, or the service is
    /// not registered with it.
    NoCredential,
    /// latchkey's own record says the credential has expired.
    Expired,
    /// The service refused the credential: signed out, revoked, wrong.
    Rejected,
    /// The credential works but may not do this.
    Forbidden,
    /// A bot wall (Cloudflare's challenge) stood in front of the service.
    Blocked,
    RateLimited,
    /// The service answered with a 5xx.
    ServiceError,
    /// Nothing answered: no network, no DNS, a refused connection.
    Unreachable,
    /// The latchkey gateway that holds the credentials did not answer.
    GatewayUnreachable,
    /// An answer that was not what the API returns.
    UnexpectedResponse,
    /// The bundled runtime latchkey runs in is missing.
    NoRuntime,
    NoBrowser,
    /// latchkey could not get its encryption key from the keychain.
    Keychain,
    Unknown,
}

impl IssueKind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// A failure as the wizard shows it: its kind, and the text it was read
/// from, for the details fold.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub issue: IssueKind,
    pub detail: String,
}

impl Failure {
    pub fn from_text(detail: String, gateway: bool) -> Self {
        Self {
            issue: classify(&detail, gateway),
            detail,
        }
    }
}

/// The kind of trouble `text` reports. `gateway`: the credentials live
/// behind a latchkey gateway, so a refused connection is the gateway's.
/// The order matters — a "credentials are not set up" wrapper around an
/// HTTP 403 with Cloudflare's marker is a block, not a sign-in problem.
pub fn classify(text: &str, gateway: bool) -> IssueKind {
    let has = |needles: &[&str]| needles.iter().any(|n| text.contains(n));
    let lower = text.to_ascii_lowercase();
    let has_lower = |needles: &[&str]| needles.iter().any(|n| lower.contains(n));

    if has(&[
        "no bundled runtime",
        "spawn latchkey failed",
        "could not run latchkey",
    ]) {
        return IssueKind::NoRuntime;
    }
    if has(&[
        "No encryption key available",
        "system keyring",
        "encryption key was lost",
        "Failed to decrypt file",
    ]) {
        return IssueKind::Keychain;
    }
    if has(&["Failed to reach latchkey gateway"]) {
        return IssueKind::GatewayUnreachable;
    }
    if has(&[
        "No browser found",
        "no browser available",
        "No browser configured",
    ]) {
        return IssueKind::NoBrowser;
    }
    if has(&[
        "No credentials found for",
        "No credentials stored for account",
        "has no credentials stored for account",
        "No service matches URL",
        "Unknown service:",
    ]) {
        return IssueKind::NoCredential;
    }
    if text.contains("Credentials for ") && text.contains(" are expired") {
        return IssueKind::Expired;
    }
    if has(&[r#"cf-mitigated=Some("challenge")"#, "Just a moment..."])
        || has_lower(&["cf-mitigated: challenge"])
    {
        return IssueKind::Blocked;
    }
    if has(&["HTTP 429", "\"ratelimited\""]) {
        return IssueKind::RateLimited;
    }
    if has(&[
        "HTTP 401",
        "\"invalid_auth\"",
        "\"not_authed\"",
        "\"token_revoked\"",
        "\"token_expired\"",
        "\"account_inactive\"",
        "authentication_error",
    ]) {
        return IssueKind::Rejected;
    }
    if has(&[
        "HTTP 403",
        "\"missing_scope\"",
        "\"not_allowed_token_type\"",
    ]) {
        return IssueKind::Forbidden;
    }
    if (500..600).any(|code| text.contains(&format!("HTTP {code}"))) {
        return IssueKind::ServiceError;
    }
    let unreachable = has(&[
        "curl exit 6 ",
        "curl exit 7 ",
        "curl exit 28 ",
        "curl exit 35 ",
        "curl exit 52 ",
        "curl exit 56 ",
        "curl exit 60 ",
        "latchkey curl timed out",
        "Could not resolve host",
        "Failed to connect",
    ]);
    if unreachable {
        return if gateway {
            IssueKind::GatewayUnreachable
        } else {
            IssueKind::Unreachable
        };
    }
    if has(&["invalid JSON", "non-JSON body", "malformed response"]) {
        return IssueKind::UnexpectedResponse;
    }
    IssueKind::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What each probe printed against the fault matrix — trimmed only
    /// where it ran on.
    const SEEN: &[(&str, IssueKind)] = &[
        (
            r#"auth.test: HTTP 403 body="<!DOCTYPE html><title>Just a moment...</title>""#,
            IssueKind::Blocked,
        ),
        (
            r#"chatgpt.com credentials are not set up: GET /backend-api/me -> HTTP 403 cf-mitigated=Some("challenge") body="<!DOCTYPE html>""#,
            IssueKind::Blocked,
        ),
        (
            r#"claude.ai credentials are not set up: forbidden: GET /account -> HTTP 403 cf-mitigated=Some("challenge")"#,
            IssueKind::Blocked,
        ),
        (r#"auth.test: ok=false error="invalid_auth""#, IssueKind::Rejected),
        (
            r#"claude.ai credentials are not set up: GET /account -> HTTP 401: "{\"error\":{\"type\":\"authentication_error\"}}""#,
            IssueKind::Rejected,
        ),
        (
            r#"chatgpt.com credentials are not set up: GET /backend-api/me -> HTTP 401 cf-mitigated=None body="{}""#,
            IssueKind::Rejected,
        ),
        (
            "auth.test: slack: latchkey curl exit 1 (https://slack.com/api/auth.test): Error: No credentials found for slack.\nRun 'latchkey auth browser slack' or 'latchkey auth set slack' first.",
            IssueKind::NoCredential,
        ),
        (
            "claude.ai credentials are not set up: claude: latchkey curl exit 1 (https://claude.ai/api/account): Error: No credentials found for claude-ai.",
            IssueKind::NoCredential,
        ),
        (
            r#"auth.test: invalid JSON: "<html><body>Sign in to continue</body></html>" (expected value at line 1 column 1)"#,
            IssueKind::UnexpectedResponse,
        ),
        (
            "fetch /me: GET /backend-api/me: 200 but non-JSON body: expected value at line 1 column 1",
            IssueKind::UnexpectedResponse,
        ),
        (
            "auth.test: slack: gave up retrying (https://slack.com/api/auth.test): 1 sequential failed requests (limit 1); last attempt: HTTP 429",
            IssueKind::RateLimited,
        ),
        (
            "auth.test: slack: gave up retrying (https://slack.com/api/auth.test): 1 sequential failed requests (limit 1); last attempt: HTTP 503",
            IssueKind::ServiceError,
        ),
        (
            "auth.test: slack: gave up retrying (https://slack.com/api/auth.test): 1 sequential failed requests (limit 1); last attempt: slack: latchkey curl exit 7 (https://slack.com/api/auth.test): curl: (7) Failed to connect to slack.com:443 via 127.0.0.1:64055 after 0 ms: Could not connect to server",
            IssueKind::Unreachable,
        ),
        (
            "auth.test: slack: spawn latchkey failed: no bundled runtime for latchkey@3.16.2 — looked for `node/bin/node` under /tmp/x",
            IssueKind::NoRuntime,
        ),
        (
            "Error: No browser found after trying sources: existing-config, system-browser, existing-playwright-browser",
            IssueKind::NoBrowser,
        ),
        (
            "Error: Failed to reach latchkey gateway at http://127.0.0.1:1989/latchkey: fetch failed",
            IssueKind::GatewayUnreachable,
        ),
        (
            "Error: Service 'slack' has no credentials stored for account 'picard'. No accounts are stored for 'slack' yet.",
            IssueKind::NoCredential,
        ),
        (
            "Error: Credentials for google-gmail are expired.\nRun 'latchkey auth browser google-gmail' first.",
            IssueKind::Expired,
        ),
        (
            "No encryption key available. Set LATCHKEY_ENCRYPTION_KEY or allow access to the system keyring.",
            IssueKind::Keychain,
        ),
        ("list orgs: something nobody has seen before", IssueKind::Unknown),
    ];

    #[test]
    fn each_text_seen_is_read_as_its_kind() {
        for (text, kind) in SEEN {
            assert_eq!(classify(text, false), *kind, "{text}");
        }
    }

    /// Behind a gateway, a connection nothing accepted is the gateway's:
    /// the request never left for the service.
    #[test]
    fn a_refused_connection_behind_a_gateway_is_the_gateways() {
        let text = "auth.test: slack: latchkey curl exit 7 (https://slack.com/api/auth.test): curl: (7) Failed to connect";
        assert_eq!(classify(text, true), IssueKind::GatewayUnreachable);
        assert_eq!(classify(text, false), IssueKind::Unreachable);
    }

    #[test]
    fn strum_and_serde_spell_the_kinds_the_same() {
        for kind in IssueKind::VARIANTS {
            let json = serde_json::to_string(kind).unwrap();
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
            assert_eq!(IssueKind::parse(kind.as_str()), Some(*kind));
        }
    }
}
