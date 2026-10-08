# latchkey: how datalib signs in

Every source that reaches a web service signs in through
[latchkey](https://github.com/imbue-ai/latchkey), a command-line tool
that keeps credentials encrypted on the machine and puts them on
requests for us. A downloader never sees a token: it runs
`latchkey curl <url>`, and latchkey adds the header or cookie that
service needs. This page is what you need to know before touching a
sign-in, the wizard's account row, or a provider's HTTP client.

## How datalib runs it

- **One pinned version.** `LATCHKEY_VERSION` in
  `datalib/backend/runtime/src/node_runtime.rs` names it, and the
  bundled runtime ships that version beside its own Node. Nothing runs
  whatever `latchkey` happens to be on the PATH.
- **Requests** go through `datalib_etl_web::http::latchkey_curl`, which
  sets `LATCHKEY_CURL` to our router curl so that Cloudflare-fronted
  hosts get the Chrome-impersonating curl
  ([`curl_impersonate.md`](curl_impersonate.md)).
- **The wizard** runs the other commands (`services info`,
  `auth browser`, `auth set`, `ensure-browser`) from
  `datalib/backend/http/src/connect.rs`, and checks a connection by
  running `datalib-step probe`, which makes one real request.
- **`--account` is a global option**, so it goes *before* the
  subcommand: `latchkey --account work curl …`, never after.

## Every run reads the keychain

latchkey keeps its encryption key in the system keychain, and every
invocation reads it, even `--version`, unless `LATCHKEY_ENCRYPTION_KEY`
or `LATCHKEY_GATEWAY` is set. On a Mac the first read can put up a
password prompt. So the wizard runs latchkey only after a person picks
a source that needs it, never on page load. Tests always set a key of
their own (below). The container has no keychain and keeps the key in
its bind mount instead ([`docker.md`](docker.md)).

## Three kinds of service

A latchkey *service* is a name (`slack`, `google-gmail`, `claude-ai`)
plus the URLs its credential is good for and how to get one.

| kind | examples | where it comes from |
|---|---|---|
| **built-in** | `slack`, `google-gmail`, `google-calendar`, `github`, `fastmail`, `fastmail-dav`, `notion` | ships inside latchkey |
| **plugin** | `garmin` | a package under `<latchkey dir>/plugins/<name>/`. datalib vendors the ones it needs (`third-party/latchkey-garmin/`) and installs them on the first sign-in (`http/src/plugins.rs`); a copy without datalib's stamp is the person's own and is never touched. |
| **registered** | `claude-ai`, `chatgpt` | `latchkey services register`, run by the wizard on the first sign-in from the catalog entry's `credentialRegister`. latchkey refuses to re-register a name it already holds, so a registration someone made by hand stays theirs. |

`latchkey services info <name>` reports a plugin as `built-in` and a
registered service as `user-registered`. When two services cover the
same URL, `latchkey curl` uses the first one that holds a credential,
so a stale sibling can shadow the service you just signed in to.

## Accounts: who names them

latchkey can hold several credentials for one service, one per
*account*. An account is a free-text name, spaces and all. The empty
name is the default account, which is where a credential goes when
nobody names one. A source picks its account with
`latchkey_settings.account` in its config, which becomes `--account` on
every request. Leaving the setting out passes no flag at all, so datalib
never sends `--account ""`.

**Who picks the name depends on the service**, and only for a browser
login:

| | browser login (`auth browser`) | pasted credential (`auth set`, `auth set-nocurl`) |
|---|---|---|
| **built-in and plugin services** | The service names the account: the login asks who you signed in as (an email, a workspace user) and stores the credential under that. `--account X` is accepted only when X is already stored, which refreshes it. A new name is refused before any browser opens. | Stored under the name you give. |
| **registered services** | You name it: `--account X` stores under X, a new name included. With none, the default account. | Stored under the name you give. |

Two services latchkey ships with a browser login and no naming of their
own (`openrouter`, `ngrok`) break the first row's rule. datalib uses
neither. latchkey does not report the rule directly, which is why
datalib reads it off the service's type
([imbue-ai/latchkey#169](https://github.com/imbue-ai/latchkey/issues/169)).
`e2e_auth/accounts.spec.ts` checks the rule for every service the
catalog signs in to with a browser, so a latchkey upgrade that changes
it fails there.

When no account is named, latchkey works it out from what is stored:

- **nothing stored:** the default account;
- **one account stored:** that one, named or not. A read uses it, and a
  `set` without `--account` *overwrites* it;
- **two or more:** it refuses and asks for `--account` ("Multiple
  accounts are stored…").

Three more rules follow from that:

- **A name is overwritten without asking.** Two credentials that must
  live side by side on one service need two names. Fastmail's contacts
  and calendar passwords are the worked case ([`fastmail.md`](fastmail.md)).
- **A paste with no name overwrites a lone named credential**, a browser
  login say. So the wizard's paste form always stores under a name, and
  `pasteTarget` in `datalib/ui/src/config/credentialShape.ts` says what
  that name will replace.
- **`--account` naming nothing stored fails before anything is sent**,
  so it never shows up as a 401.

### What the wizard does with this

The account row has one account box per source, and it behaves
the way the service names accounts (`ServiceInfo.account_naming`, read
off latchkey's `type` and whether the service has a browser login):

- **The service names it** (Slack, Gmail, Garmin, …): the box says so
  and offers the accounts latchkey holds. "Sign in with browser" passes
  `--account` only when the box names one of those, and then reads
  "Sign in again as …"; otherwise it adds an account and the box takes
  the name latchkey reports. A new name typed in the box is kept only
  for a pasted key, which latchkey stores under the name it is given,
  and the sign-in tab says the login will replace it. The pure decision
  is `datalib/ui/src/config/accountNaming.ts`.
- **You name it** (Claude, ChatGPT, and every service with no browser
  login, such as `fastmail-dav`, `notion` and `gitlab`, where a pasted
  key is the only way in): the box takes any name, a new one included,
  and every way of signing in stores under it. Empty means the default
  account. Its help line is `NAME_IT_HELP` in the same file.

## It speaks HTTP and nothing else

latchkey only ever puts a credential on an HTTP request it makes
itself; it never hands the credential out. A protocol curl cannot hold
a session for (IMAP, say) cannot go through it, and datalib does not
pull secrets out of latchkey to get around that.
[`email_download_modes.md`](email_download_modes.md#5-why-there-is-no-imap-mode)
has the long version.

## A gateway holds everything elsewhere

Under Minds, datalib runs with `LATCHKEY_GATEWAY` set, and latchkey
forwards `services info`, `auth browser` and `curl` to a latchkey
running elsewhere. It refuses every command that changes local state:
`ensure-browser`, `services register`, `auth set` and `auth clear`.
The wizard therefore offers no sign-in of its own there and says where
to sign in instead, and no plugin is installed.

## The browser

`auth browser` needs a browser configured. The wizard first runs
`ensure-browser` with only the sources that find one already on the
machine. If none turns up, it downloads Playwright's Chromium, once, and says
so on screen while it runs.

## When it fails

- `latchkey curl` exits 1 when latchkey will not send the request at
  all: no credential, a missing plugin, a refresh that failed. Every
  later request would fail the same way, so a provider should end the
  run on it rather than retry (see `garmin/src/ingest/api.rs`).
- The wizard turns whatever latchkey or a probe printed into one
  sentence per kind of trouble: `IssueKind` in
  `datalib/backend/probe/src/issue.rs`, worded in
  `datalib/ui/src/config/issues.ts`.

## Testing against the real thing

A provider's `live` tests reach the real service with your own
credentials, and need `LATCHKEY_CURL` pointed at the router curl
([`testing.md`](testing.md)).

`//datalib/ui:e2e_auth` runs the wizard's sign-ins against the real
pinned latchkey, a fake internet and a headless browser, with a store
and key of its own, so it never reads the keychain.
[`datalib/ui/tests/e2e_auth/README.md`](../../datalib/ui/tests/e2e_auth/README.md)
says what stands in for what. To try latchkey by hand without touching
your own store:

```sh
export LATCHKEY_DIRECTORY=$(mktemp -d) LATCHKEY_ENCRYPTION_KEY=$(openssl rand -base64 32)
npx -y latchkey@<LATCHKEY_VERSION> services info slack
```
