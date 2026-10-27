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
  `auth browser`, `auth set`, `ensure-browser`, and for Google
  `auth browser-prepare` and `auth prepare`) from
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
| **built-in** | `slack`, `google-gmail`, `google-calendar`, `github`, `fastmail`, `fastmail-dav`, `linear`, `notion`, `todoist` | ships inside latchkey |
| **plugin** | `garmin` | a package under `<latchkey dir>/plugins/<name>/`. datalib vendors the ones it needs (`third-party/latchkey-garmin/`) and installs them on the first sign-in (`http/src/plugins.rs`); a copy without datalib's stamp is the person's own and is never touched. |
| **registered** | `claude-ai`, `chatgpt` | `latchkey services register`, run by the wizard on the first sign-in from the catalog entry's `credentialRegister`. latchkey refuses to re-register a name it already holds, so a registration someone made by hand stays theirs. |

`latchkey services info <name>` reports a plugin as `built-in` and a
registered service as `user-registered`. When two services cover the
same URL, `latchkey curl` uses the first one that holds a credential,
so a stale sibling can shadow the service you just signed in to.

## An OAuth client of your own

Google's services (`google-gmail`, `google-calendar`, `google-drive`,
`google-docs`, `google-sheets`, `google-slides`) sign in through an
OAuth client in a Google Cloud project, and latchkey ships none. Until
a service has one, `auth browser` refuses at once with "requires
preparation first", which the wizard reads as
`IssueKind::NoOauthClient`. latchkey keeps one client per service,
apart from its accounts; every account of the service shares it. Two
commands store one:

- **`auth browser-prepare <service>`** drives the Cloud console in the
  browser: it makes or reuses a project named `Latchkey-<service>`
  (`Latchkey-gmail`), turns on that service's APIs, sets up the consent
  screen and creates a Desktop app client. It takes minutes, and needs
  2-Step Verification on the Google account. So each service gets a
  project of its own, and Google limits how many an account may have.
- **`auth prepare <service> '{"clientId":…,"clientSecret":…}'`**
  stores a client made some other way, with no browser. Nothing ties a
  client to one service, so a single client whose project has every
  API turned on can be given to all six.

Each service is still its own sign-in, because latchkey asks for each
one's scopes separately; only the client is shared.

Once a sign-in has reported the refusal, the wizard offers both:
"Create one for me" sends `create_oauth_client` with the next
`POST /api/latchkey/<service>/connect`, which runs `browser-prepare`
before the login; a pasted client goes to
`POST /api/latchkey/<service>/oauth_client`, which turns Google
Cloud's downloaded `client_secret.json` into latchkey's shape
(`http/src/oauth_client.rs`), runs `auth prepare`, and the wizard then
signs in. latchkey cannot say whether a service has a client short of
trying, which is why the offer comes after the refusal.

### A source that signs in to several services

Google Drive's download reaches `google-drive`, `google-docs`,
`google-sheets` and `google-slides`, and needs the same account on
each. Its catalog entry names the first as `credentialService` and the
rest as `credentialAlso`, and the wizard's one "Sign in with browser"
runs `auth browser` for each in turn. Every one of them names its own
accounts, so the first login says who, and each later one is passed
`--account` only where that service already holds the account; a login
that lands on someone else stops the run. A service that already holds
the account is skipped, so a run cut short is finished by the logins
left ("Finish signing in as …"), and when all of them hold it every one
is run again. The decision is `datalib/ui/src/config/multiSignIn.ts`.
Such a source is offered only the pasted client, stored on every
service with one `POST …/oauth_client` each: `browser-prepare` would
make a Cloud project per service. It has no "Paste a key" tab, since a
pasted key would reach only the first service.

## Accounts: who names them

latchkey can hold several credentials for one service, one per
*account*. An account is a free-text name, spaces and all. The empty
name is the default account, which is where a credential goes when
nobody names one. A source picks its account with
`latchkey_settings.account` in its config, which becomes `--account` on
every request. Leaving the setting out passes no flag at all, so datalib
never sends `--account ""`.

**Who picks the name depends on the service**, and only for a browser
login. `latchkey services info` says which, as
`capabilities.detectsLoginAccount`:

| | browser login (`auth browser`) | pasted credential (`auth set`, `auth set-nocurl`) |
|---|---|---|
| **detects the login account** (Slack, Gmail, Fastmail, GitHub, Garmin, Linear, Todoist) | The service names the account: the login asks who you signed in as (an email, a workspace user) and stores the credential under that. `--account X` is accepted only when X is already stored, which refreshes it. A new name is refused before any browser opens. | Stored under the name you give. |
| **does not** (every registered service, so Claude and ChatGPT; `openrouter`, `ngrok`) | You name it: `--account X` stores under X, a new name included. With none, the default account. | Stored under the name you give. |

The flag describes a browser login, so it means nothing for a service
that has none: latchkey reports `true` for `fastmail-dav` and `notion`,
which only take a pasted key. `e2e_auth/accounts.spec.ts` checks the
rule against the pinned latchkey for every service the catalog signs in
to with a browser.

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
off `detectsLoginAccount` and whether the service has a browser login):

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

latchkey keeps **one saved browser session** (`browser_state.json.enc`)
for every service and account, and a login starts from it, so a site
you signed in to before opens already signed in. That saves a password,
and latchkey takes the session's cookie from the browser when the site
sends no new one. But it is signed in as whoever used it last, and a
service that does not detect the login account files that person under
the name it was given. So the wizard starts the login from a signed-out
browser (`LATCHKEY_EPHEMERAL_BROWSER`, which neither loads nor saves
the session) when the service does not detect the account and latchkey
already holds a different account for it: a second Claude or ChatGPT
account. The decision is `freshBrowser` in
`datalib/ui/src/config/accountNaming.ts`.

## Starting over

To test a sign-in from scratch, clear every credential and the saved
browser session at once:

```sh
npx -y latchkey@<LATCHKEY_VERSION> auth clear -y
```

With no service name it deletes `credentials.json.enc` and
`browser_state.json.enc` from latchkey's directory (`~/.latchkey`
unless `LATCHKEY_DIRECTORY` says otherwise). Every data root on the
machine shares that directory, and under Minds the gateway's store is
the host's, so this signs out everywhere. Latchkey's browser keeps no
profile of its own, so the cookies go with that file. It leaves
`config.json` (the browser path and the services the wizard
registered), `plugins/` and the keychain key; to test those first-run
steps too, move the whole directory aside instead.

## Signing in again without a person

A token-capture service (ChatGPT) stores a token the page mints from
its session, and that token expires; latchkey has no refresh for it.
But `auth browser` starts from the saved browser session, so while
that session is still signed in the login finishes in about ten
seconds with nobody at the keyboard. A download can therefore renew
its own credential on a `401` with
`datalib_etl_web::latchkey::renew_by_browser_login`, which runs it
under a time limit and refuses under a gateway or in playback. The
saved session belongs to whoever used it last, so a provider that
renews must check the account it got before using it
(`chatgpt/src/ingest/api.rs`).

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
