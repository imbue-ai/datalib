//! Failure classification + per-provider auth remediation hints.

use anyhow::Result;
use datalib_dag::events::Event;
use datalib_dag::FailureKind;

use crate::source_type::SourceType;

use crate::events::{Emitter, OutputClaim};

/// Which [`FailureKind`] a failure is, from the text of its cause
/// chain. The scheduler's retry policy keys off the answer, so this is
/// the runner's vocabulary rather than a label of our own.
pub fn classify(e: &anyhow::Error) -> FailureKind {
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
        FailureKind::Auth
    } else if s.contains("HTTP 429") || s.contains("rate limit") || s.contains("rate-limit") {
        FailureKind::RateLimited
    } else if s.contains("timed out")
        || s.contains("connection reset")
        || s.contains("connection refused")
        || s.contains("dns error")
    {
        FailureKind::Transient
    } else {
        FailureKind::Data
    }
}

/// If `res` is an auth-classified failure, emit the provider's fix-it
/// hint as a structured event (before the outcome line the caller
/// will emit).
pub fn emit_auth_hint_on_failure(
    emitter: &Emitter,
    source_type: SourceType,
    res: &Result<Vec<OutputClaim>>,
) {
    if let Err(e) = res {
        if classify(e) == FailureKind::Auth {
            emitter.event(&Event::Hint {
                step: String::new(), // re-tagged by the runner
                msg: auth_hint_for(source_type),
            });
        }
    }
}

/// Appended to every per-provider hint whose service can hold more than
/// one stored account, so the multi-account case is answered where the
/// user is already looking rather than only in the docs.
const MULTI_ACCOUNT_NOTE: &str = "\n\n\
Signed in to this service with more than one account? Store the \
credential under the one you mean (`--account` is a latchkey *global* \
option, so it precedes the subcommand):\n\
  {LK} --account \"you@example.com\" auth set <service> -H \"...\"\n\
then name that same account on the source, so the download \
authenticates as it:\n\
  [steps.params.latchkey_settings]\n\
  account = \"you@example.com\"\n\
`{LK} auth list` shows what is stored for each service. With two \
stored and none named, latchkey refuses the request as ambiguous \
rather than guessing.";

const GENERIC_AUTH_HINT: &str = "Provider returned an auth-failure status. \
This usually means latchkey credentials are missing or expired. \
See <provider>/INGEST.md for setup. Confirm the in-tree curl shim is \
built (`cargo build -p datalib-etl --bin latchkey-curl-impersonate`), or \
set $DATALIB_CURL_DISPATCH / $LATCHKEY_CURL explicitly, and that \
`{LK} auth list` shows entries.";

/// Per-provider fix-it text for auth failures. Every runnable latchkey
/// command is written with a `{LK}` placeholder (plain `.replace`, not
/// `format!` — several blocks contain literal braces) that resolves to
/// the app-bundled `latchkey` launcher when running from the packaged
/// app, else `npx -y latchkey@<pin>`, so the printed commands are
/// copy-pasteable as-is in both worlds.
pub fn auth_hint_for(source_type: SourceType) -> String {
    let template: &str = match source_type {
        // All hints route the secret through the macOS clipboard so it
        // never lands in shell history: a one-liner copies the token to
        // the pasteboard, then the printed `… auth set …` command
        // expands `$(pbpaste)` at exec time. zsh/bash record the literal
        // `$(pbpaste)`, not the resolved value.
        SourceType::Chatgpt => {
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

See datalib/backend/etl/providers/chatgpt/INGEST.md for details."
        }
        SourceType::Claude => {
            "\
Claude sessionKey expired or missing.

  1. One-time: make sure the claude-ai service is registered
     (`{LK} services info claude-ai` errors if it isn't):
       {LK} services register claude-ai --base-api-url=\"https://claude.ai/\"
  2. Open https://claude.ai logged in. In DevTools → Application →
     Cookies → claude.ai, copy the `sessionKey` value to the clipboard.
  3. Run (uses `$(pbpaste)` so the token isn't recorded in shell history):
       {LK} auth set claude-ai -H \"Cookie: sessionKey=$(pbpaste)\"
  4. Smoke-test:
       {LK} curl -s https://claude.ai/api/organizations | head -c 200

See datalib/backend/etl/providers/claude/INGEST.md for details."
        }
        SourceType::Slack => {
            "\
slack token expired or missing.

  1. Grab a user-scope OAuth token (xoxc/xoxp/xoxd) and copy it to the
     clipboard.
  2. Run (uses `$(pbpaste)` so the token isn't recorded in shell history):
       {LK} auth set slack -H \"Authorization: Bearer $(pbpaste)\"
  3. Smoke-test:
       {LK} curl -s https://slack.com/api/auth.test | head -c 200

See datalib/backend/etl/providers/slack/INGEST.md for details."
        }
        SourceType::Github => {
            "\
github PAT expired or missing.

  1. Create a fine-grained PAT at https://github.com/settings/tokens
     with `repo` + `read:user` scopes; copy it to the clipboard.
  2. Run (uses `$(pbpaste)` so the token isn't recorded in shell history):
       {LK} auth set github -H \"Authorization: Bearer $(pbpaste)\"
  3. Smoke-test:
       {LK} curl -s https://api.github.com/user | head -c 200

See datalib/backend/etl/providers/github/INGEST.md for details."
        }
        SourceType::Gitlab => {
            "\
gitlab token expired or missing.

  1. Create a personal token at https://gitlab.com/-/profile/personal_access_tokens
     with `read_api` scope; copy it to the clipboard.
  2. Run (uses `$(pbpaste)` so the token isn't recorded in shell history):
       {LK} auth set gitlab -H \"Authorization: Bearer $(pbpaste)\"
  3. Smoke-test:
       {LK} curl -s https://gitlab.com/api/v4/user | head -c 200

See datalib/backend/etl/providers/gitlab/INGEST.md for details."
        }
        SourceType::Notion => {
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

See datalib/backend/etl/providers/notion/INGEST.md for details."
        }
        SourceType::Email => {
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

For a **Gmail** account, prefer the REST API mode — it needs no service
registration at all, because latchkey has a built-in `google-gmail`
service and routes to it by URL host:

  1. {LK} auth browser google-gmail
     (If it reports no OAuth client, run
      `{LK} auth browser-prepare google-gmail` first and wait a few
      minutes for Google to finish creating it.)
  2. Smoke-test:
       {LK} curl -s https://gmail.googleapis.com/gmail/v1/users/me/profile
  3. Point the source at it — an empty table is a complete config:
       [steps.params.gmail]
     Signed in as more than one Google account? Name which one this
     source mirrors — latchkey requires it once a service holds two:
       [steps.params.latchkey_settings]
       account = \"you@gmail.com\"

  ACCESS_TOKEN_SCOPE_INSUFFICIENT means some scopes were not approved at
  consent time — run `{LK} auth browser google-gmail` again and approve
  all of them.

See datalib/backend/etl/providers/email/INGEST.md for details."
        }
        SourceType::Beeper => {
            "\
beeper download reads Beeper Texts' on-disk SQLite. No auth dance.

  1. Make sure Beeper Texts is installed and has run at least once
     so its data dir exists. Default path:
       ~/Library/Application Support/BeeperTexts/index.db
     (Pass --beeper-data-dir, or set `path` in the source's `texts`
     table, to override.)
  2. Confirm read access (Application Support is NOT Full Disk Access
     protected, so this should just work):
       sqlite3 ~/Library/Application\\ Support/BeeperTexts/index.db \\
           \"SELECT COUNT(*) FROM threads;\"

See datalib/backend/etl/providers/beeper/INGEST.md for details."
        }
        _ => GENERIC_AUTH_HINT,
    };
    // The multi-account note only makes sense where latchkey holds the
    // credential; `beeper` reads an on-disk SQLite and has no service.
    let hint = if source_type.uses_latchkey_account() {
        format!("{template}{MULTI_ACCOUNT_NOTE}")
    } else {
        template.to_string()
    };
    hint.replace("{LK}", &datalib_core::node_runtime::latchkey_cli_hint())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_common_failures() {
        let auth = anyhow::anyhow!("HTTP 403 Forbidden").context("fetch /me");
        assert_eq!(classify(&auth), FailureKind::Auth);
        let rl = anyhow::anyhow!("HTTP 429 too many requests");
        assert_eq!(classify(&rl), FailureKind::RateLimited);
        let tr = anyhow::anyhow!("connection reset by peer");
        assert_eq!(classify(&tr), FailureKind::Transient);
        let other = anyhow::anyhow!("unparseable row 17");
        assert_eq!(classify(&other), FailureKind::Data);
    }

    #[test]
    fn auth_hint_resolves_latchkey_placeholder() {
        let hint = auth_hint_for(SourceType::Slack);
        assert!(!hint.contains("{LK}"), "placeholder must be substituted");
        assert!(hint.contains("auth set slack"));
        // A type with no hint of its own gets the generic text.
        assert!(auth_hint_for(SourceType::Perseus).contains("latchkey credentials"));
    }
}
