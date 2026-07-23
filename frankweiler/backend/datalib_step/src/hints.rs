//! Failure classification + per-provider auth remediation hints.
//!
//! The hint table is carried over from the retired sync orchestrator's
//! `auth_hint_for` (sync is a binary crate, so it isn't importable;
//! this becomes the single copy when sync retires). On an
//! auth-classified failure the hint is emitted as a structured
//! `{"event":"hint",…}` so the runner/UI can surface it prominently
//! instead of burying it in log text.

use anyhow::Result;
use frankweiler_dag::events::Event;

use crate::events::{Emitter, OutputClaim};

/// Map an error chain to the DAG failure taxonomy (`FailureKind` wire
/// values). Heuristic — same signal sync's auth-hint detection used —
/// until providers classify their own errors.
pub fn classify(e: &anyhow::Error) -> &'static str {
    let s: String = e
        .chain()
        .map(|c| c.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    if s.contains("HTTP 401")
        || s.contains("HTTP 403")
        || s.contains("Unauthorized")
        || s.contains("Forbidden")
        // `cf-mitigated` only when actually set — see sync's note on
        // the Debug rendering of the absent header.
        || s.contains("cf-mitigated=Some(")
        // latchkey's error for a service that was never registered.
        || s.contains("No service matches URL")
    {
        "auth"
    } else if s.contains("HTTP 429") || s.contains("rate limit") || s.contains("rate-limit") {
        "rate_limited"
    } else if s.contains("timed out")
        || s.contains("connection reset")
        || s.contains("connection refused")
        || s.contains("dns error")
    {
        "transient"
    } else {
        "data"
    }
}

/// If `res` is an auth-classified failure, emit the provider's fix-it
/// hint as a structured event (before the outcome line the caller
/// will emit).
pub fn emit_auth_hint_on_failure(
    emitter: &Emitter,
    provider_type: &str,
    res: &Result<Vec<OutputClaim>>,
) {
    if let Err(e) = res {
        if classify(e) == "auth" {
            emitter.event(&Event::Hint {
                step: String::new(), // re-tagged by the runner
                msg: auth_hint_for(provider_type),
            });
        }
    }
}

const GENERIC_AUTH_HINT: &str = "Provider returned an auth-failure status. \
This usually means latchkey credentials are missing or expired. \
See <provider>/DOWNLOAD.md for setup. Confirm the in-tree curl shim is \
built (`cargo build -p frankweiler-etl --bin latchkey-curl-impersonate`), or \
set $FRANKWEILER_CURL_SHIM / $LATCHKEY_CURL explicitly, and that \
`{LK} auth list` shows entries.";

/// Per-provider fix-it text for auth failures. Every runnable latchkey
/// command is written with a `{LK}` placeholder (plain `.replace`, not
/// `format!` — several blocks contain literal braces) that resolves to
/// the app-bundled `latchkey` launcher when running from the packaged
/// app, else `npx -y latchkey@<pin>`, so the printed commands are
/// copy-pasteable as-is in both worlds.
pub fn auth_hint_for(provider: &str) -> String {
    let template: &str = match provider {
        // All hints route the secret through the macOS clipboard so it
        // never lands in shell history: a one-liner copies the token to
        // the pasteboard, then the printed `… auth set …` command
        // expands `$(pbpaste)` at exec time. zsh/bash record the literal
        // `$(pbpaste)`, not the resolved value.
        "chatgpt_api" => {
            "\
chatgpt access token expired or missing.

  1. Open https://chatgpt.com in a logged-in browser, then in DevTools
     console run (clipboard write needs page focus, so it waits for a
     click on the page):
       const r = await fetch('/api/auth/session');
       const j = await r.json();
       addEventListener('click', async () => {
         await navigator.clipboard.writeText(j.accessToken);
         console.log('  {LK} auth set chatgpt -H \"Authorization: Bearer $(pbpaste)\"');
       }, { once: true });
     Then click anywhere on the chatgpt page; the console prints the
     command to run.
  2. Paste the printed `latchkey auth set …` line into your shell and
     run it. zsh/bash record the literal `$(pbpaste)`, not the resolved
     token, so the secret never lands in shell history.
  3. Smoke-test:
       {LK} curl -s https://chatgpt.com/backend-api/me | head -c 200
     Expect a JSON object with your account id. If you still see a
     Cloudflare challenge, copy `cf_clearance` from DevTools → Application
     → Cookies → chatgpt.com and add a second `-H \"Cookie: cf_clearance=$(pbpaste)\"`
     to the `latchkey auth set chatgpt` call.

See frankweiler/backend/etl/providers/chatgpt/DOWNLOAD.md for details."
        }
        "claude_api" => {
            "\
anthropic sessionKey expired or missing.

  1. One-time: make sure the claude-ai service is registered
     (`{LK} services info claude-ai` errors if it isn't):
       {LK} services register claude-ai --base-api-url=\"https://claude.ai/\"
  2. Open https://claude.ai logged in. In DevTools → Application →
     Cookies → claude.ai, copy the `sessionKey` value to the clipboard.
  3. Run (uses `$(pbpaste)` so the token isn't recorded in shell history):
       {LK} auth set claude-ai -H \"Cookie: sessionKey=$(pbpaste)\"
  4. Smoke-test:
       {LK} curl -s https://claude.ai/api/organizations | head -c 200

See frankweiler/backend/etl/providers/anthropic/DOWNLOAD.md for details."
        }
        "slack_api" => {
            "\
slack token expired or missing.

  1. Grab a user-scope OAuth token (xoxc/xoxp/xoxd) and copy it to the
     clipboard.
  2. Run (uses `$(pbpaste)` so the token isn't recorded in shell history):
       {LK} auth set slack -H \"Authorization: Bearer $(pbpaste)\"
  3. Smoke-test:
       {LK} curl -s https://slack.com/api/auth.test | head -c 200

See frankweiler/backend/etl/providers/slack/DOWNLOAD.md for details."
        }
        "github_api" => {
            "\
github PAT expired or missing.

  1. Create a fine-grained PAT at https://github.com/settings/tokens
     with `repo` + `read:user` scopes; copy it to the clipboard.
  2. Run (uses `$(pbpaste)` so the token isn't recorded in shell history):
       {LK} auth set github -H \"Authorization: Bearer $(pbpaste)\"
  3. Smoke-test:
       {LK} curl -s https://api.github.com/user | head -c 200

See frankweiler/backend/etl/providers/github/DOWNLOAD.md for details."
        }
        "gitlab_api" => {
            "\
gitlab token expired or missing.

  1. Create a personal token at https://gitlab.com/-/profile/personal_access_tokens
     with `read_api` scope; copy it to the clipboard.
  2. Run (uses `$(pbpaste)` so the token isn't recorded in shell history):
       {LK} auth set gitlab -H \"Authorization: Bearer $(pbpaste)\"
  3. Smoke-test:
       {LK} curl -s https://gitlab.com/api/v4/user | head -c 200

See frankweiler/backend/etl/providers/gitlab/DOWNLOAD.md for details."
        }
        "notion_api" => {
            "\
notion integration token expired or missing.

  1. Create an internal integration at https://www.notion.so/profile/integrations
     and copy the secret to the clipboard.
  2. Run (uses `$(pbpaste)` so the token isn't recorded in shell history):
       {LK} auth set notion -H \"Authorization: Bearer $(pbpaste)\"
  3. Smoke-test:
       {LK} curl -s -X POST https://api.notion.com/v1/search \\
         -H 'Notion-Version: 2022-06-28' -H 'Content-Type: application/json' \\
         -d '{}' | head -c 200

See frankweiler/backend/etl/providers/notion/DOWNLOAD.md for details."
        }
        "email" => {
            "\
Email source: JMAP (Fastmail / generic) auth missing or expired.

  1. Create an API token at https://app.fastmail.com/settings/security/tokens
     with the 'Read-only access to mail' scope; copy it to the clipboard.
  2. Register the two host services and attach the token to both
     (Fastmail serves blob bytes from a separate host):
       {LK} services register fastmail \\
           --base-api-url=\"https://api.fastmail.com/\"
       {LK} services register fastmail-content \\
           --base-api-url=\"https://www.fastmailusercontent.com/\"
       {LK} auth set fastmail         -H \"Authorization: Bearer $(pbpaste)\"
       {LK} auth set fastmail-content -H \"Authorization: Bearer $(pbpaste)\"
  3. Smoke-test:
       {LK} curl -sSL https://api.fastmail.com/.well-known/jmap \\
           | jq .primaryAccounts

See frankweiler/backend/etl/providers/jmap/DOWNLOAD.md for details."
        }
        "beeper" => {
            "\
beeper download reads Beeper Texts' on-disk SQLite. No auth dance.

  1. Make sure Beeper Texts is installed and has run at least once
     so its data dir exists. Default path:
       ~/Library/Application Support/BeeperTexts/index.db
     (Pass --beeper-data-dir or set `beeper_data_dir:` in the source's
     sync block to override.)
  2. Confirm read access (Application Support is NOT Full Disk Access
     protected, so this should just work):
       sqlite3 ~/Library/Application\\ Support/BeeperTexts/index.db \\
           \"SELECT COUNT(*) FROM threads;\"

See frankweiler/backend/etl/providers/beeper/DOWNLOAD.md for details."
        }
        _ => GENERIC_AUTH_HINT,
    };
    template.replace("{LK}", &frankweiler_core::node_runtime::latchkey_cli_hint())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_common_failures() {
        let auth = anyhow::anyhow!("HTTP 403 Forbidden").context("fetch /me");
        assert_eq!(classify(&auth), "auth");
        let rl = anyhow::anyhow!("HTTP 429 too many requests");
        assert_eq!(classify(&rl), "rate_limited");
        let tr = anyhow::anyhow!("connection reset by peer");
        assert_eq!(classify(&tr), "transient");
        let other = anyhow::anyhow!("unparseable row 17");
        assert_eq!(classify(&other), "data");
    }

    #[test]
    fn auth_hint_resolves_latchkey_placeholder() {
        let hint = auth_hint_for("slack_api");
        assert!(!hint.contains("{LK}"), "placeholder must be substituted");
        assert!(hint.contains("auth set slack"));
        // Unknown providers get the generic text.
        assert!(auth_hint_for("carrier_pigeon").contains("latchkey credentials"));
    }
}
