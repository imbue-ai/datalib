//! Failure classification + per-provider auth remediation hints.

use anyhow::Result;
use datalib_dag::events::Event;
use datalib_dag::FailureKind;
use datalib_etl::upstream::Upstream;

use crate::source_type::SourceType;

use crate::events::{Emitter, OutputClaim};

/// Which [`FailureKind`] a failure is. The scheduler's retry policy keys
/// off the answer, so this is the runner's vocabulary rather than a label
/// of our own. It reads types, never the words: whoever judged a refusal
/// says so with an [`Upstream`] in the chain.
pub fn classify(e: &anyhow::Error) -> FailureKind {
    if e.chain()
        .any(|c| c.is::<datalib_etl::run_problems::GaveUp>())
    {
        return FailureKind::Incomplete;
    }
    let io_kind = e
        .chain()
        .find_map(|c| c.downcast_ref::<std::io::Error>().map(std::io::Error::kind));
    if io_kind == Some(std::io::ErrorKind::PermissionDenied) {
        return FailureKind::Permission;
    }
    match Upstream::in_chain(e) {
        Some(Upstream::CredentialRefused) => return FailureKind::Auth,
        Some(Upstream::RateLimited) => return FailureKind::RateLimited,
        Some(Upstream::Unavailable) => return FailureKind::Transient,
        None => {}
    }
    use std::io::ErrorKind::*;
    match io_kind {
        Some(TimedOut | ConnectionRefused | ConnectionReset | ConnectionAborted) => {
            FailureKind::Transient
        }
        _ => FailureKind::Data,
    }
}

/// If `res` failed in a way a person fixes — a credential, or access
/// the OS withheld — emit the fix-it hint as a structured event (before
/// the outcome line the caller will emit).
pub fn emit_hint_on_failure(
    emitter: &Emitter,
    source_type: SourceType,
    res: &Result<Vec<OutputClaim>>,
) {
    let Err(e) = res else { return };
    let msg = match classify(e) {
        FailureKind::Auth => auth_hint_for(source_type),
        FailureKind::Permission => PERMISSION_HINT.to_string(),
        _ => return,
    };
    emitter.event(&Event::Hint {
        step: String::new(), // re-tagged by the runner
        msg,
    });
}

/// macOS keeps some folders private (Desktop, Documents, Downloads,
/// cloud storage, removable volumes) and lets an app in only once the
/// person has chosen the path in an open panel. `docs/dev/wizard_file_pickers.md`
/// §"What the picker buys us in macOS permissions".
const PERMISSION_HINT: &str = "\
The operating system would not let Datalib read a file this source's \
settings name (\"Operation not permitted\"). On a Mac that is privacy \
protection, not a broken file: choosing the folder in Datalib's own picker \
is what grants access, and that grant can lapse.

In the app: on the Manage screen, press \"Choose folder again\" on this \
source's row and pick the same folder; it syncs again on its own. For a \
grant that lasts, give Datalib Full Disk Access in System Settings → \
Privacy & Security.

From a terminal, the terminal app is the one that needs the access.";

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
See <provider>/INGEST.md for setup. Confirm the router curl and its \
impersonator sibling are fetched (`bazel build \
//third-party/latchkey-curl-shims`), or set \
$DATALIB_CURL_ROUTER / $LATCHKEY_CURL to the router explicitly, and \
that `{LK} auth list` shows entries.";

/// Per-provider fix-it text for auth failures. Every runnable latchkey
/// command is written with a `{LK}` placeholder (plain `.replace`, not
/// `format!` — several blocks contain literal braces) that resolves to
/// the app-bundled `latchkey` launcher when running from the packaged
/// app, else `npx -y latchkey@<pin>`, so the printed commands are
/// copy-pasteable as-is in both worlds.
pub fn auth_hint_for(source_type: SourceType) -> String {
    let template: &str = match source_type {
        // Prefer `auth browser` where the service has a login flow: the
        // secret never leaves latchkey. Where a hint falls back to
        // pasting one, it routes it through the macOS clipboard so it
        // never lands in shell history — a one-liner copies the token
        // to the pasteboard, then the printed `… auth set …` command
        // expands `$(pbpaste)` at exec time. zsh/bash record the
        // literal `$(pbpaste)`, not the resolved value.
        SourceType::Chatgpt => {
            "\
chatgpt access token expired or missing.

  1. Refresh it from the browser — latchkey opens chatgpt.com, waits
     for the login, and stores the fresh token itself:
       {LK} auth browser chatgpt
  2. Smoke-test:
       {LK} curl -s https://chatgpt.com/backend-api/me
     Expect a JSON object with your account id.

If step 1 says the service has no browser login, it was registered
before the token-capture flow existed. `{LK} services info chatgpt`
lists `authOptions`; a name that already exists cannot be
re-registered, so drop it and register it again:
  {LK} services deregister chatgpt
  {LK} services register chatgpt --base-api-url=https://chatgpt.com/ --login-url=https://chatgpt.com/auth/login --login-flow=token-capture --login-flow-params='{\"tokenUrl\": \"https://chatgpt.com/api/auth/session\", \"tokenField\": \"accessToken\"}'

To paste the token by hand instead, open https://chatgpt.com logged in
and run in the DevTools console (clipboard write needs page focus, so
it waits for a click on the page):
  const r = await fetch('/api/auth/session');
  const j = await r.json();
  addEventListener('click', async () => {
    await navigator.clipboard.writeText(j.accessToken);
    console.log('  {LK} auth set chatgpt -H \"Authorization: Bearer $(pbpaste)\"');
  }, { once: true });
Then click anywhere on the chatgpt page and run the printed command;
zsh/bash record the literal `$(pbpaste)`, not the token.

If you still see a Cloudflare challenge, copy `cf_clearance` from
DevTools → Application → Cookies → chatgpt.com and add a second
`-H \"Cookie: cf_clearance=$(pbpaste)\"` to an `auth set` call.

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

Fastmail is a built-in latchkey service, covering both its API host
and the host it serves blob bytes from:

  1. Log in through the browser:
       {LK} auth browser fastmail
     Or, for a read-only credential, create an API token at
     https://app.fastmail.com/settings/security/tokens with the
     'Read-only access to mail' scope, copy it, and store it:
       {LK} auth set fastmail -H \"Authorization: Bearer $(pbpaste)\"
  2. Smoke-test:
       {LK} curl -sS https://api.fastmail.com/jmap/session \\
           | jq .primaryAccounts

Another JMAP server needs its hosts registered as a latchkey service
first; see datalib/backend/etl/providers/email/INGEST.md.

For a **Gmail** account, prefer the REST API mode — it needs no service
registration at all, because latchkey has a built-in `google-gmail`
service and routes to it by URL host:

  1. {LK} auth browser google-gmail
     (If it says google-gmail requires preparation, latchkey has no
      OAuth client for it yet: `{LK} auth browser-prepare google-gmail`
      creates one in a few minutes, or `{LK} auth prepare google-gmail
      '{\"clientId\":\"…\",\"clientSecret\":\"…\"}'` stores one you made.)
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
        SourceType::Garmin => {
            "\
Garmin Connect refused the credential, or latchkey holds none.

The `garmin` service comes from latchkey's Garmin plugin, which the
Add a source dialog installs the first time you sign in there.

  1. Sign in again from the source's settings, or in a terminal:
       {LK} auth browser garmin
     A token folder garth wrote works too:
       {LK} auth set-nocurl garmin ~/.garth
  2. Re-run the sync.

See datalib/backend/etl/providers/garmin/INGEST.md for details."
        }
        SourceType::Beeper => {
            "\
beeper download mirrors Beeper Texts' on-disk SQLite. No auth dance.

  1. Make sure Beeper Texts is installed and has run at least once
     so its data dir exists. Default path:
       ~/Library/Application Support/BeeperTexts/index.db
     (Set `path` in the source's `texts` table to override.)
  2. Application Support is not Full Disk Access protected, so a
     read should just work; a failure here is usually a wrong `path`.

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
    fn classify_reads_the_marker_a_provider_attached() {
        let auth = anyhow::Error::new(Upstream::CredentialRefused).context("fetch /me");
        assert_eq!(classify(&auth), FailureKind::Auth);
        let rl = anyhow::Error::new(Upstream::RateLimited);
        assert_eq!(classify(&rl), FailureKind::RateLimited);
        let down = anyhow::Error::new(Upstream::Unavailable);
        assert_eq!(classify(&down), FailureKind::Transient);
        let reset = std::io::Error::from(std::io::ErrorKind::ConnectionReset);
        assert_eq!(classify(&anyhow::Error::new(reset)), FailureKind::Transient);
        let other = anyhow::anyhow!("unparseable row 17");
        assert_eq!(classify(&other), FailureKind::Data);
    }

    /// The words are never read. Gmail's spent daily quota is a 403 whose
    /// message says "HTTP 403", and a signed URL's refusal says it too;
    /// neither is a credential to sign in again for (both were, by text).
    #[test]
    fn a_status_in_the_words_is_not_a_classification() {
        for said in [
            "Gmail API https://gmail.googleapis.com/x → HTTP 403: the project's daily quota is spent",
            "every window was refused (HTTP 403); check the device id and its start",
            "HTTP 401 Unauthorized; rate limit; timed out",
        ] {
            assert_eq!(classify(&anyhow::anyhow!("{said}")), FailureKind::Data, "{said}");
        }
    }

    /// The transport's own failures carry their meaning to the classifier
    /// through any provider error that keeps them as a source.
    #[test]
    fn the_transports_failures_classify_through_its_source() {
        use datalib_etl_web::http::{HttpError, HttpService};
        let timeout = HttpError::Timeout {
            service: HttpService::Slack,
            url: "https://slack.com/api/auth.test".into(),
            timeout_ms: 60_000,
        };
        assert_eq!(
            classify(&anyhow::Error::new(timeout).context("list channels")),
            FailureKind::Transient
        );
        let unregistered = HttpError::Curl {
            service: HttpService::Claude,
            url: "https://claude.ai/api/organizations".into(),
            exit: 1,
            stderr: "Error: No service matches URL: https://claude.ai/api/organizations".into(),
        };
        assert_eq!(
            classify(&anyhow::Error::new(unregistered)),
            FailureKind::Auth
        );
        let throttled = HttpError::GaveUp {
            service: HttpService::Slack,
            url: "https://slack.com/api/conversations.history".into(),
            reason: "50 failed in a row; last attempt: HTTP 429".into(),
            last_status: Some(429),
        };
        assert_eq!(
            classify(&anyhow::Error::new(throttled)),
            FailureKind::RateLimited
        );
        let gave_up_on_500s = HttpError::GaveUp {
            service: HttpService::Slack,
            url: "https://slack.com/api/conversations.history".into(),
            reason: "50 failed in a row; last attempt: HTTP 500".into(),
            last_status: Some(500),
        };
        assert_eq!(
            classify(&anyhow::Error::new(gave_up_on_500s)),
            FailureKind::Data
        );
    }

    /// macOS refusing a read of a configured path (EPERM, "Operation not
    /// permitted") is a person's to fix, not bad data.
    #[test]
    fn an_os_refusal_anywhere_in_the_chain_is_permission() {
        let eperm = std::io::Error::from_raw_os_error(1);
        let e = anyhow::Error::new(eperm)
            .context("read /backups/msgstore.db.crypt15")
            .context("decrypt /backups/msgstore.db.crypt15");
        assert_eq!(classify(&e), FailureKind::Permission);
        let eacces = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert_eq!(
            classify(&anyhow::Error::new(eacces)),
            FailureKind::Permission
        );
        let missing = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert_eq!(classify(&anyhow::Error::new(missing)), FailureKind::Data);
    }

    /// A give-up says `timeout` or `HTTP 429` in its words; it is still
    /// `incomplete`, never a kind the runner would retry at once (#1157).
    #[test]
    fn a_give_up_is_incomplete_whatever_its_words() {
        let said = "HTTP 429 too many requests; stopped after 50 failed in a row";
        let e = anyhow::Error::new(datalib_etl::run_problems::GaveUp(said.to_string()))
            .context("source tng-mail");
        assert_eq!(classify(&e), FailureKind::Incomplete);
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
