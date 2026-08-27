# Design: the "New Data Source" wizard

**Status: proposal, revised 2026-08-26 against main @ `f54e7e80`.
Nothing here is built yet.** The first revision was written against a
tree that has since moved a long way — `unified_index/`, the
one-file-per-writer store split, the applet that took the grid routes
out of `datalib-http`, and the removal of the download report. Claims
below have been re-checked against that main; the ones that changed are
called out where they sit.
Related: [#171](https://github.com/imbue-ai/datalib/issues/171)
(`grid_rows` needs `source_name` before the sources grid can count rows
per source). Per
[`AGENTS.md`](../../AGENTS.md), don't cite this file as a description of
the tree — it describes work we intend to do. When the first slice
lands, rewrite the sections it makes real and delete the rest.

Scope of *this* document: the **configuration** half — a hand-holding
flow that takes a user from "I want my Slack in here" to a saved,
credential-verified `[[steps]]` pair. The **execution/observation**
half (run one source, watch it closely) is sketched in the last section
and designed separately.

## The problem

Today the Manage tab (`ui/src/views/SourcesView.vue`) is a raw
`config.toml` textarea beside a derived table of source steps, plus a
row of "quick add" chips (`ui/src/config/snippets.ts`). Adding Slack
means:

1. Click the Slack chip. It appends a step pair with a guessed channel
   list (`["general"]`) and a `since` 30 days back.
2. Discover — from a *comment in the appended TOML*, or from a failed
   sync's hint — that you first need to register a latchkey service and
   paste an `xoxc` token into a shell.
3. Guess your channel names. Get one wrong; nothing tells you until the
   run finishes.
4. Save, hit Sync, read the log to find out whether any of it worked.

Every check that could happen *before* the first run happens after it,
as a failure. The knowledge needed to do it right is distributed across
`hints.rs`, each provider's `DOWNLOAD.md`, and
`docs/user/config_examples/all_sources.toml` — three places the UI
never shows you.

The wizard's job is to move all of that in front of the first run.

## The Manage screen

The wizard is not a feature bolted onto today's Manage tab — it replaces
what that tab leads with. Right now the tab opens on a raw
`config.toml` textarea, with a derived list of step ids beside it and a
row of quick-add chips. That puts a text editor in front of a person
whose actual question is "which of my things are mirrored, and are they
working?"

Invert it:

```
┌──────────────────────────────────────────────────────────────────────┐
│  Data sources                                    [ + Add Data Source ]│
├──────────────────────────────────────────────────────────────────────┤
│  ▣ Slack       slack_api    imbue-ai ✓   2h ago    ok      ▓▓▓▓░ 4.1G │
│  ▣ Claude      claude_api   thad     ✓   2h ago    ok      ▓▓░░░ 890M │
│  ▣ Fastmail    email        thad     ⚠   6d ago    failed  ▓▓▓░░ 2.2G │
│  ▣ Documents   pdf          —            2h ago    ok      ▓▓▓▓▓ 12G  │
├──────────────────────────────────────────────────────────────────────┤
│  Recent activity …          (status cells open a per-source log panel)│
│  ▸ Advanced: edit config.toml                                         │
└──────────────────────────────────────────────────────────────────────┘
```

An AG Grid of configured sources is the page. **Add Data Source** sits
above it and opens the picker → configure flow. Each row carries
**Run · Edit · Delete**, and Edit opens the same wizard that created the
source, on the screen you came for.

The config editor doesn't go away — it stays the source of truth and the
escape hatch, and agents edit through it via `PUT /api/config`. It moves
into a collapsed **Advanced** disclosure at the bottom. Demoted, not
deleted: when the wizard can't express something, "edit as TOML" has to
lead somewhere.

### Columns

| Column | Where the value comes from |
|---|---|
| **Source** | stanza name + catalog icon |
| **Type** | the step `command`'s provider word |
| **Account** | `latchkey services info <service>` → account key + `credentialStatus` chip |
| **Last synced** | *(needs new persistence — see below)* |
| **Last status** | *(same)* — **and it's a button**: clicking it opens that source's recent logs |
| **Documents** | the `unified_index` applet — **not** a direct query, see below |
| **Storage** | http stats the source's directories itself |
| **Actions** | Run · Edit · Delete |

### The status cell is the way into the logs

A source that failed should not make you go hunting. The **Last status**
cell is the control: click `failed` and you get that source's recent
log, scrolled to the failure. Click `ok` and you get the last run's log
anyway — the same affordance, no dead ends, and it doubles as "what did
that sync actually do?"

That beats a separate Logs button in the actions group for two reasons:
the status is *already* the thing you looked at to decide you cared, and
an actions group of four buttons in every row is where a table starts
feeling like a cockpit.

What opens is a side panel, not a route — you are triaging one row of a
table you want to stay in. It carries:

- **The failure first.** The DAG already emits `Event::Hint` for
  actionable remediation, distinct from `Log` precisely so a UI can
  surface it instead of burying it — and `hints.rs` fills it with the
  provider's fix-it text on an auth failure. That hint belongs at the
  top of the panel, above the log, with the *Reconnect* button beside it
  when the failure kind is `auth`.
- **A level filter.** The worker already writes the tracing
  subscriber's NDJSON to `<root>/system/job-logs/<id>.log`, and
  `SourcesView` already classifies lines by `level`. Error / warn / info
  / everything is a filter over data that exists.
- **This source's lines only.** A run spans several sources, so the
  panel filters the job log by step id rather than showing the whole
  run. (The same per-step attribution the `step_runs` table needs — one
  more reason it comes first.)
- **Older runs.** A dropdown of this source's recent runs, from
  `step_runs`, so "it broke sometime last week" is answerable.

Live runs stream into the same panel over the existing
`/api/sync/stream` SSE, so clicking status on a running source is how
you watch it — which is most of what "part two" wanted, reachable from
the grid rather than as a separate screen.

### Two of those columns have no data source yet

"Last synced" and "Last status" are the obvious things to want per
source, and **neither is currently recorded per source.**

- `sync_jobs` (in `system/jobs.doltlite_db`) is per *run*, and a run
  routinely spans several sources — the UI comma-joins step ids into one
  job's `source_name`. A multi-source job that failed doesn't say which
  source failed.
- `DagState`'s `StepState` *is* per step, but it holds
  `input_versions`, `output_versions`, `succeeded` and `fingerprint` —
  no timestamp, no error. It answers "is this step up to date", not
  "when did it last run and how did it go".

The events already exist and are simply not persisted: every run ends
with `Event::RunSummary`, whose `StepSummary { step, status, failure,
attempts, error, outputs }` is exactly these two columns, per step.

**Proposal: persist it.** A `step_runs` table in
`system/jobs.doltlite_db` — the server already owns that file and is its
only writer — keyed `(job_id, step_id)`, written by the sync worker as
it consumes the run's event stream. Both columns become one query, and
per-source run history comes free (a status sparkline in the row, if we
want it later). `dag_state.rs`'s own module doc already flags moving
this state into a `pipeline_runs` table as an open question, so this
runs with the grain.

### Document counts must go through the applet

The first draft had `datalib-http` running
`SELECT source_name, COUNT(*) FROM markdowns GROUP BY source_name`.
**That is no longer allowed.** `core/src/layout.rs` now states that
`unified_index/` is "owned end to end by the `unified_index` applet and
the two steps that write it; nothing in `datalib-http` or `datalib-dag`
reads what is under here" — which is the whole point of the applet that
took `/api/search` and friends out of the server.

So the count comes from a new endpoint on the `unified_index` applet
(it serves `/search`, `/columns`, `/docs`, `/chat`, `/asset` today —
add `/sources/stats`), reached through the existing applet gateway. The
grid degrades gracefully when the applet isn't running: blank counts,
everything else still renders.

**Storage stays on the http side**, because it is a plain directory
stat of `<data_root>/<name>/` — no index, no applet, no provider.

## Principles

1. **The config text stays the single source of truth.** The wizard is
   a structured generator *and editor* of that text, not a parallel
   config store: it appends to, or surgically edits, the same buffer the
   raw editor shows, and the user can see the exact text before it's
   written. No hidden state, no round-trip fidelity problem, no "the
   wizard and the file disagree".
2. **Providers own their own descriptors.** The catalog entry for
   `slack_api` lives next to `SlackConfig` in `slack_config`, the same
   way the schema does (issue #41's compose-don't-flatten discipline).
   The UI renders a generic form from a declarative descriptor — see
   [What "generic" has to mean](#what-generic-has-to-mean) for exactly
   how much Slack-specific behavior that does and doesn't buy.
3. **Verify at every step, never at the end.** Credentials are
   established in-flow — a *Connect* button where latchkey supports a
   browser login, a token field where it doesn't — and confirmed before
   you move on; channel/folder/label pickers are populated by a live
   call, so a listed choice is a choice that works. The wizard should
   be nearly incapable of producing a config that fails on first run
   for a reason we could have known.
4. **Progressive disclosure.** One decision per screen, sensible
   defaults pre-filled, an "Advanced" disclosure for the long tail
   (`refresh_window_days`, `blob_size_limit_bytes`,
   `download_params.*`). Thunderbird's account setup is the model: it
   asks for the two things it can't infer and derives everything else.
5. **Every source type is reachable from day one.** A provider with no
   hand-written descriptor still gets a generic flow (name + a params
   textarea + the `hints.rs` credential text), so the button is never a
   dead end. Descriptors upgrade sources one at a time.

## Architecture

```
  UI  ── GET  /api/sources/catalog ──►  datalib-http
                                          └─ links datalib-source-catalog
                                             (schema-only crates, no provider runtime)

  UI  ── POST /api/sources/probe ─────►  datalib-http
                                          └─ spawns: datalib-step probe <type> --op … --params …
                                                       └─ provider code, latchkey, network

  UI  ── POST /api/sources/draft ─────►  datalib-http   (params JSON → TOML step pair, validated)
  UI  ── PUT  /api/config ────────────►  datalib-http   (unchanged: whole-text save)
```

Three new endpoints, one new `datalib-step` verb, one new crate. The
existing save path is reused verbatim.

### Why the catalog is linked and the probe is spawned

`datalib-http` deliberately does not depend on any provider crate (see
its `Cargo.toml`) — providers are reached only by shelling out to
`datalib-step`. The catalog descriptors live in the `*_config` crates,
which are schema-only (serde + anyhow, no transport, no doltlite), so
linking an aggregator crate costs the http binary nothing and saves a
subprocess spawn on every page load.

The wizard certainly does need credentials, the network and the
filesystem — that is most of what it does. Those split three ways by
*who owns the knowledge*, and only the first needs a provider:

| The wizard needs to… | Runs as | Because |
|---|---|---|
| list channels, inspect a PDF tree, hit a provider's API | `datalib-step probe` | provider code; http links none of it |
| connect an account, check credential status | http → `latchkey` directly | latchkey is a generic credential CLI, not provider code, and its runtime resolution already lives in `datalib_core::node_runtime` |
| browse for a folder or file | http itself | plain filesystem; no provider or credential involved |

Routing the latchkey calls through `datalib-step` would be a hop for
nothing — no provider crate is involved in `latchkey auth browser`.

Three registries would then name the source types: `SOURCE_TYPES` and
the `dispatch::plan` match in `datalib_step`, and the catalog. A test
in `datalib_step` (the only crate linking both) asserts the catalog
covers exactly `SOURCE_TYPES` — drift becomes a build failure rather
than a missing tile.

## The catalog descriptor

One entry per source type. `serde`-serialized to JSON at
`GET /api/sources/catalog`. Sketch, with the Slack entry filled in:

```jsonc
{
  "type": "slack_api",
  "label": "Slack",
  "blurb": "Mirror channels and DMs from one Slack workspace.",
  "icon": "slack",                    // ui/src/assets/<icon>.svg; null → kind glyph
  "keywords": ["slack", "chat", "workspace", "channels"],
  "kind": "api",                      // api | local_file | local_dir | export
  "default_name": "slack",            // seeds the step id: slack.download / slack.render
  "docs": "datalib/backend/etl/providers/slack/DOWNLOAD.md",

  // Just the latchkey service name. latchkey already knows Slack —
  // authOptions, the header shape, the token example and the live
  // credential status all come from `services info slack` at runtime.
  // The field exists only because the names differ (our `slack_api`
  // vs latchkey's `slack`). See the credentials section for the
  // `register` form user-registered services need.
  "credential": { "service": "slack" },

  "screens": [
    { "id": "credential", "kind": "credential" },
    { "id": "channels",   "kind": "multiselect",
      "probe": "list.channels",
      "target": "sync.channels",      // dotted path into the download params
      "value_field": "name",          // channels are named, not id'd, in our config
      "label": "Which channels should we mirror?",
      "empty_means": "all channels you're a member of",
      "columns": ["name", "is_private", "num_members", "purpose"] },
    { "id": "window", "kind": "fields", "fields": [
      { "target": "sync.since", "kind": "date", "label": "Mirror messages since",
        "default": "-P30D", "help": "Moving this earlier backfills on the next run." },
      { "target": "sync.media", "kind": "bool", "label": "Download file attachments",
        "default": true }
    ]},
    { "id": "advanced", "kind": "fields", "collapsed": true, "fields": [
      { "target": "sync.refresh_window_days", "kind": "int", "label": "Edit-catcher window (days)" },
      { "target": "common.blob_size_limit_bytes", "kind": "bytes", "label": "Skip attachments larger than" }
    ]},
    { "id": "review", "kind": "review" }
  ]
}
```

Field kinds are a **closed, small set** — `text`, `int`, `bytes`,
`bool`, `date`, `path`, `string_list`, `select`, `multiselect`,
`tree_multiselect`. This is deliberately not a general form-builder
DSL: anything a provider can't express in those kinds belongs in the
raw-TOML escape hatch, not in a richer descriptor language. See
[What "generic" has to mean](#what-generic-has-to-mean).

`target` is a dotted path into the download step's `params` tree
(`sync.channels` → `[steps.params.sync] channels = …`). A field may
carry `"phase": "render"` to land on the render step instead (e.g.
email's `only_render_labels`, beeper's `period`). That mapping is the
whole trick: it lets one generic renderer serve twenty providers, and
it lives in the crate that owns the struct being filled.

Descriptors are optional. A type with none gets
`kind: "generic"` — name field, params textarea seeded from
`all_sources.toml`, and the `credential.howto_md` text if any.

## What "generic" has to mean

"The UI knows nothing about Slack" is a claim worth pinning down,
because listing a user's channels is obviously Slack-specific work.
The Slack-specific parts are the **descriptor** (in `slack_config`) and
the **probe implementation** (in the slack provider crate). What the UI
holds is:

> screen kind `multiselect`; call probe `list.channels` for source type
> `slack_api`; render the returned rows using columns
> `[name, is_private, num_members, purpose]`; store the `name` field of
> each checked row into `sync.channels`.

Every noun there is data from the descriptor. The UI POSTs
`{type, op}`, gets back a list of flat JSON objects, and renders the
declared columns. It never branches on the provider. Swap in
`email` + `list.mailboxes` + different columns and the same component
serves it.

**The contract that makes this work is the probe's return shape**: a
list of flat objects with string/number/bool fields, plus a descriptor
naming which field is the stored value and which are displayed. That's
the whole interop surface, and it is deliberately narrow.

### Where it stops working, and what happens then

A flat checklist doesn't serve everything:

- **Email labels are a tree.** They're POSIX-like paths and matching is
  exact — `Work` does *not* pull in `Work/Projects`
  (`all_sources.toml`). A flat list of 300 labels is a bad picker; this
  wants a tree with explicit per-node selection.
- **Notion pages are a tree** for the same reason.

So the closed set of screen kinds needs `tree_multiselect` alongside
`multiselect` — probe returns the same flat objects plus a parent/path
field, the UI builds the tree. Two kinds cover every selection case we
have.

**The rule for anything beyond the closed set: it falls back to the
generic params form.** A provider does not get to inject bespoke UI. If
some future source genuinely needs a control the kinds can't express,
that is a deliberate decision to extend the closed set — reviewed, and
paid for once in the shared renderer — not an escape hatch each provider
opens for itself. The moment providers can ship UI, the "one generic
renderer" property is gone and every screen becomes a special case.

## The probe verb

New subcommand, same NDJSON contract as every other step type
(`docs/dev/step_protocol.md`):

```
datalib-step probe <source_type> --op <op> --params <json>
```

It streams the usual `log` / `progress_message` / `hint` events on
stdout and finishes with one terminal line:

```jsonc
{"event": "probe_result", "ok": true,
 "data": {"team": "imbue", "user": "thad", "url": "https://imbue.slack.com/"}}
```

Failures reuse the existing machinery: `hints::classify` picks the
failure kind and `auth_hint_for` supplies the remediation text, so a
probe that 401s tells the user exactly what it would have told them
after a failed sync — just twenty minutes earlier.

**Ops are a closed set**, so the UI stays generic:

| op | meaning | Slack | email (JMAP) | pdf / fsindex |
|---|---|---|---|---|
| `auth` | credentials work; return an identity summary — **the only check available for user-registered services**, whose `credentialStatus` is always `unknown` | `auth.test` | `.well-known/jmap` | n/a |
| `list.<resource>` | enumerate selectable things | `conversations.list` | `Mailbox/get` | n/a |
| `inspect` | validate a path, summarize what's there | n/a | n/a | file count, total bytes, `needs_ocr` count |

Hard contract for probes: **read-only, bounded, and never touches the
data root.** No raw store is opened, nothing is committed, and the
backend imposes a wall-clock timeout (60 s) and cancels on client
disconnect. A probe is the one place in this system where the user is
sitting there watching, so it must be cheap or it must stream.

Dispatch for probes is opt-in — a separate match from `dispatch::plan`
listing only the providers that implement one, everything else
returning `unsupported`. Adding a probe to a provider is then a local
change, not a twenty-crate refactor.

## Credentials: latchkey should be invisible

**The user should never learn the word "latchkey."** For Slack that is
fully achievable today, because latchkey already has the flow we want.

### What latchkey actually offers (verified against the pinned 3.7.0)

`latchkey auth browser <service>` — *"Login to a service via the
browser and store the API credentials."* It exists in the pinned
**3.7.0** (bumped from 3.1.0 by #177; it was already there in 3.1.0). For Slack it opens a
real Chromium at `https://slack.com/signin`, lets the user log in
normally, and scrapes the `xoxc-` api_token plus the `d` cookie out of
the authenticated session — exactly the credential shape our downloader
consumes.

It is **cleanly spawnable from a server**: no TTY, no stdin prompts. On
success it prints `Done. Stored credentials for account '<workspace>'.`
and exits 0. On failure it exits 1 with one typed message —
`Login cancelled.`, `GraphicalEnvironmentNotFoundError`,
`BrowserNotConfiguredError`, `BrowserDisabledError`,
`PreparationRequiredError`, `BrowserFlowsNotSupportedError`. Those map
one-to-one onto states the credential screen should render.

Two more things make the wizard simpler than designed:

- **`latchkey services info <service>` is machine-readable JSON** with
  `authOptions` (`["browser", "set"]` for Slack), `baseApiUrls`,
  `setCredentialsExample`, `developerNotes`, and a `credentials` map of
  account → `credentialStatus` (`valid` | `invalid` | `unknown`). The
  credential screen picks its own mode from `authOptions` — no
  hard-coded per-provider table in our catalog at all, just the service
  name.
- **For Slack, latchkey already runs our auth probe.** Its service
  definition carries
  `credentialCheckCurlArguments = ['https://slack.com/api/auth.test']`,
  which is where the `valid` status and the workspace-derived account
  name come from. So `--op auth` is redundant for Slack — though not in
  general; see
  [`credentialStatus` is only as good as latchkey's checker](#credentialstatus-is-only-as-good-as-latchkeys-checker).

### Registration is data, so browser login isn't only for built-ins

`latchkey services register` takes `--login-flow=cookie-capture` with
`--login-flow-params '{"cookieKeys": [...]}'`: *"Open the login URL and
capture named session cookies as they are set."* A service latchkey has
never heard of can therefore be **taught** a browser login — and once
registered it reports `authOptions: ["browser", "set"]` and the
credential screen treats it exactly like Slack.

That matters most for Claude, whose whole credential is the single
`sessionKey` cookie we currently ask people to copy out of DevTools.
The descriptor carries the registration as data:

```jsonc
"credential": {
  "service": "claude-ai",
  "register": {
    "base_api_url":      "https://claude.ai/",
    "login_url":         "https://claude.ai/login",
    "login_flow":        "cookie-capture",
    "login_flow_params": { "cookieKeys": ["sessionKey"] }
  }
}
```

**Unverified — this needs a live test before we build on it.** The help
text is explicit about the failure mode: cookies are read from
`Set-Cookie` headers seen *during* the sign-in, "so a cookie that only a
page script sets is not seen, and neither is one that an already
signed-in session never sends again." Whether claude.ai sets `sessionKey`
that way is an empirical question.

### Coverage: better than it looks, because latchkey matches by URL

Two things I had wrong. First, **latchkey picks the service by matching
the request URL against the service's `baseApiUrls`** — not by any name
we pass it. `etl/src/http.rs` shells out to `latchkey curl <url>` and
latchkey resolves from there; the string providers pass to
`HttpRequest::get("jmap", …)` is our own tag for logging and
impersonation routing, nothing more. The email provider's Gmail module
says so in its header: *"it routes by URL host"*.

Second, the 3.7.0 pin (#177) made **Fastmail built-in with OAuth**, and
#175 added a Gmail REST path that lands on `gmail.googleapis.com` —
which is the built-in `google-gmail` service. Both have browser login.

Checked against a live `latchkey auth list` on a developer machine:

| datalib source | latchkey service (URL-matched) | How the user connects |
|---|---|---|
| `slack_api` | `slack` — built-in | **Browser.** |
| `github_api` | `github` — built-in | **Browser.** |
| `email` (JMAP / Fastmail) | `fastmail` — built-in, OAuth | **Browser.** |
| `email` (Gmail REST) | `google-gmail` — built-in, OAuth | **Browser**, after a one-time `auth browser-prepare` to mint an OAuth client. |
| `claude_api` | `claude-ai` — user-registered | Token field today. Cookie-capture candidate (above). |
| `chatgpt_api` | `chatgpt` — user-registered | Token field; the downloader wants a Bearer token from a JSON endpoint, not a cookie. |
| `gitlab_api` | `gitlab` — built-in | Token field (`PRIVATE-TOKEN`); built-in is set-only. |
| `notion_api` | `notion` — built-in | Token field. |
| `carddav` (Fastmail DAV) | `fastmail-dav` — built-in | Token field — an app password, not OAuth. |

So **four sources get a Connect button**, not two — including the
Gmail-over-IMAP-style onboarding that started this whole thread, which
is now reachable rather than aspirational.

### The screen still has two modes, chosen from `authOptions`

- **`browser`** → one button, "Connect Slack". The backend runs
  `latchkey ensure-browser` then `latchkey auth browser <service>`,
  streams status, and re-reads `services info` to confirm and name the
  account. Nothing typed, nothing pasted, latchkey never named. Covers
  slack, github, fastmail and google-gmail.
- **`set`** → a labeled secret field whose value the backend pipes to
  `latchkey auth set` **on stdin** — never argv, which is world-readable
  via `ps` — after running the descriptor's `register` block first for
  the user-registered services. The `hints.rs` prose becomes the *how to
  get this token* text beside the field, not an instruction to open a
  terminal.

The copy-this-command escape hatch stays behind a disclosure, because it
is the only thing that works against a headless server.

### `credentialStatus` is only as good as latchkey's checker

The design leaned on `services info` reporting `valid` / `invalid` /
`unknown`. Comparing two live entries shows where that stops:

```jsonc
"slack":     { "thad@imbue-ai": { "credentialType": "slack",   "credentialStatus": "valid"   } }
"claude-ai": { "":              { "credentialType": "rawCurl", "credentialStatus": "unknown" } }
```

Both are connected and working. Slack reads `valid` because its built-in
service carries `credentialCheckCurlArguments =
['https://slack.com/api/auth.test']` — latchkey has something to call.
`claude-ai` is a generic user-registered service with no checker, so it
**can never report better than `unknown`**, no matter how healthy the
credential is.

That settles a question the earlier draft got half right. The `--op
auth` probe is redundant *for Slack* — but it is the only confirmation
available for every user-registered service, which is most of the
token-field column above. So:

- `credentialStatus` is `valid`/`invalid` → show it, skip the probe.
- `credentialStatus` is `unknown` → run the provider's `auth` probe and
  show *that* result. Never render "unknown" to a user as if it were a
  problem; it usually isn't.

### Hazard: the descriptor's service name can silently drift

Because latchkey resolves by URL, the `"service"` field in a descriptor
is used only for the credential UI — `services info`, `auth browser`,
`auth set`. Nothing at request time validates it. A descriptor naming
the wrong service would therefore show the wrong status, and connect an
account the downloader never uses, while syncs kept working (or kept
failing) for unrelated reasons.

Cheap guard: a test that, for each descriptor, asserts the provider's
base URL matches the named service's `baseApiUrls` per
`latchkey services info`. Tagged `requires-network`-ish since it shells
out, but it turns a silent mismatch into a red test.

### Three things this turns up

1. **Multi-account is a live bug, not a wizard feature.** latchkey
   stores credentials *per account* and errors when a service has more
   than one and no `--account` is passed.
   `datalib/backend/etl/src/http.rs` builds `latchkey curl …` with no
   `--account`, ever. A live `auth list` shows named accounts on
   `slack` (`thad@imbue-ai`), `github`, `gitlab`, `fastmail`
   (`thad_imbue@fastmail.com`) and `google-gmail` (`thad@imbue.com`) —
   so this is not a Slack quirk. A second Slack workspace, or a second
   Gmail account, breaks *every* request for that service today.
   The wizard forces it open on day one, since "connect an account" is
   its first screen: it needs an account field on the source config and
   `--account` plumbed through `HttpRequest`. **Fix in phase 2.**
2. **`ensure-browser` may download a Chromium.** Its source list ends
   in `download-playwright-browser`. First-run can therefore pull a
   large binary — that has to be surfaced as an explicit, consented
   step with progress, not a silent stall behind a spinner.
3. **Gateway mode already works.** When `gatewayUrl` is set,
   `auth browser` forwards the request to the gateway rather than
   opening a local browser, which lines up with datalib's existing
   gateway env var.

### Where the latchkey calls run

Not through `datalib-step`. latchkey is not provider code, and the
runtime resolution already lives in `datalib_core::node_runtime`
(`bundled_command` / `npx_command`, pinned `LATCHKEY_VERSION`), which
`datalib-http` links. `datalib_etl::latchkey::latchkey_command()` is a
thin wrapper over those; move it down into `datalib-core` and have
`datalib-etl` re-export it, and http can call latchkey directly:

```
POST /api/credentials/status   { service }        → services info, parsed
POST /api/credentials/connect  { service }        → ensure-browser + auth browser (SSE)
POST /api/credentials/set      { service, secret} → services register + auth set (stdin)
```

Resolution must go through that one pinned path. A bare
`npx -y latchkey` floats to whatever version npm feels like and
misreports credential state.

## Local-path sources need a file picker we don't have

Over half the twenty types read from local disk (`pdf`, `fsindex`,
`lightroom`, `linkedin`, `google_takeout`, `signal_backup`,
`whatsapp_backup`, `sms_backup_restore`, `beeper`, `perseus`,
`claude_export`, and `carddav`/`email` in their file-backed modes) and
want "choose a folder" / "choose a `.lrcat`".

The UI has no Tauri IPC today — `capabilities/default.json` grants
`core:default` only, and nothing in `ui/src` imports `@tauri-apps`.
The app is also usable as a plain browser client against
`datalib-http`. So: **a backend-served browse endpoint**, not a native
dialog.

```
GET /api/fs/browse?path=~/Documents&accept=dir
  → { path, parent, entries: [{name, kind, size, mtime}] }
```

Rooted at `$HOME` by default with an explicit "browse elsewhere"
escape, `accept` filtering by extension or dir-ness, and — the part
that earns its keep — an `inspect` probe fired on selection so the
screen can say *"4,182 PDFs, 3.1 GB, 96 need OCR"* before you commit.
Adding a native Tauri dialog later is an enhancement layered on the
same endpoint, not a replacement.

## The picker screen

The entry point is a **New Data Source** button at the top of the
Manage tab's source table, opening a route (`/sources/new`) rather
than a modal — it's a multi-step flow and should survive a reload.

- A grid of tiles: icon, label, one-line blurb.
- A filter box with focus on open, matching label + type + keywords
  (`slack`, `chat`, `workspace` all find Slack). Arrow keys move,
  Enter selects.
- Grouped by `kind`: **Connected accounts** (api), **Exports & backups**
  (export), **On this computer** (local\_\*). Twenty tiles is already
  past the point where a flat list scans well.

Icons: `ui/src/assets/` has eleven brand SVGs; `beeper`,
`google_takeout`, `lightroom`, `pdf`, `fsindex`, `perseus`, `yolink`
and `carddav` have none. A per-`kind` fallback glyph covers them until
someone draws the rest. (Brand marks are trademarks — we're using them
nominatively to identify the service, which is the ordinary and
defensible use, but don't restyle or recolor them.)

## Writing the config

The last screen shows the generated TOML — the same step pair
`snippets.ts` produces today, but with real values — and a diff against
the current buffer. `POST /api/sources/draft {type, name, params}`
returns that text, generated **backend-side** so the formatting and
comment conventions live in one place, and validated by the real config
loader before it comes back.

"Create" appends it to the editor buffer and saves via the existing
`PUT /api/config`. A trailing checkbox — *Sync this source now* —
enqueues the job that already exists (`enqueueJob({kind:"all",
source_name})`), which is the seam where this flow hands off to the
execution half.

## Create, edit, delete: one descriptor, three verbs

Editing looks like it needs a painful TOML round trip, which is a good
reason to defer it — and it's wrong. "Read the params, regenerate the
step" *would* destroy comments and hand-edits, but that is not how you
write it. With a **format-preserving document model** you navigate to
`steps[i].params.sync.channels`, replace that one value, and leave every
other byte alone.

`toml_edit` is exactly that crate. The tree today has `toml` 1.1
(serde-shaped, parse-only) and not `toml_edit`; adding it is an ordinary
Cargo.toml dep — `MODULE.bazel` already resolves the workspace manifest,
so no MODULE change. That makes edit the same size of job as create, and
the same descriptor drives both:

| verb | what it does to the text |
|---|---|
| **create** | append the step pair at the end (the only safe insertion point in TOML) |
| **edit** | surgical value replacement for the fields that changed; insert into the right table for fields that didn't exist |
| **delete** | remove the two steps' character ranges, which `configSources.ts` already computes |

### The property that makes editing trustworthy

**The wizard writes back only the fields it changed, and only fields the
descriptor models.** A hand-written
`[steps.params.common.download_params]` block, an inline comment, a knob
no descriptor covers — all untouched, because nothing regenerates the
step wholesale.

The corollary has to be enforced, not assumed: when a source's params
contain something the descriptor *can't* represent, the wizard says so
and offers "edit as TOML" rather than showing a form that silently
under-represents the file. A form that quietly drops a setting is worse
than no form.

### Editing skips what's already settled

"Add two more Slack channels" should not be a five-screen walk. Opening
*Edit* on a configured source lands directly on the screen you came for,
with the rest collapsed to one-line summaries:

```
  ✓ Connected as imbue-ai                                  [change]
  ▸ Channels — 4 selected                                  ← opens here
  ✓ Since 2025-01-01 · attachments on                      [change]
```

The channels screen re-runs `list.channels`, shows every channel with
the current four checked, and writes back only `sync.channels`. That is
strictly better than editing the array by hand — you see membership,
privacy and message counts while choosing, and you cannot typo a name.

Credential state comes free here: `latchkey services info slack` already
reports the account and `credentialStatus`, so an expired token shows as
a red row with a *Reconnect* button on the source grid, before you run
anything.

## The sources grid

Column list and data sources are in [The Manage screen](#the-manage-screen)
above. What follows is the part that needs argument rather than a table.

`markdowns` is what makes per-source attribution possible at all: it
carries `source_name`, so counts attribute to the *configured source*.
`grid_rows` has only `provider` and `source_label`, under which two
email sources (`fastmail` and `gmail-takeout`) collapse into one bucket
— hence [#171](https://github.com/imbue-ai/datalib/issues/171).

### Storage: a stacked bar, not one number

Walking `<data_root>/<name>/` gives three naturally separate parts —
`raw/entities.doltlite_db`, `raw/blobs.doltlite_db`, and
`rendered_md/`. Stacked in one bar per row, that answers the question
people actually have ("why is this 40 GB?" → it's blobs) instead of just
restating a total. A plain CSS bar in a cell renderer does this; it does
not need AG Grid's enterprise sparklines.

Worth knowing before leaning further on enterprise features:
`GridCard.ce.vue` registers `AllEnterpriseModule` with **no
`LicenseManager` key set**, so that grid runs in evaluation mode today.
That's pre-existing, but a second enterprise-dependent surface deepens
the commitment — one more reason the storage bar should be plain CSS.

**One caveat the walk must respect:** `common.raw_path` can point a
source's raw store outside the data root (documented in
`all_sources.toml`). The declared `outputs` still say `slack/raw` in
that case, so a naive walk reports zero. Resolve the real path, or mark
the cell "stored elsewhere" — do not render a confident 0 B.

### Source names must be unique, and some are reserved

A source's name is its identity everywhere: it is the stanza directory
on disk (`<data_root>/<name>/`), the prefix of both its artifact paths
(`<name>/raw`, `<name>/rendered_md`), the `markdowns.source_name` its
rows carry, and the stem of its two step ids. Nothing currently enforces
that it is unique, and the wizard is the moment that stops being
academic — a "Add Data Source" button with a pre-filled default name
will produce a second `slack` the first time someone connects a second
workspace.

**Duplicates are not rejected today.** `dag::config::to_specs` builds a
`Vec<StepSpec>` with no id check — `validate_applets` has a
`bail!("applet {:?}: duplicate id")` and the steps path has no
counterpart. Both duplicate steps then run, writing the same output
paths, while the persisted scheduler state (`DagState.steps`, a
`BTreeMap<StepId, StepState>`) has one entry the two of them clobber in
turn. So the failure isn't a clean error, it's two steps fighting over
one slot of bookkeeping.

**Reserved names are enforced on the wrong path.**
`RESERVED_STANZA_NAMES` has exactly one caller —
`migrate_config/src/legacy_stanza.rs::validate_source_name`, which runs
only when converting a pre-TOML `config.yaml`. Nothing checked it on the
live TOML path, so `config.toml` could name a source `system`. The list
was also incomplete: it held only `SYSTEM_DIR`, while `unified_index/`
became a second reserved top-level directory in the refactor that
introduced it.

Worth noting what else that migrator function does, since it is the only
place source names are validated at all: it rejects `.`/`..`, a leading
`-`, and anything outside the POSIX portable filename character set.
**None of those rules apply to a `config.toml`.** Porting them is a
separate change from the reserved-name fix — a step's `outputs` are
free-form paths, not a `name` field — but a source called `../etc` is
worth thinking about before the wizard starts generating names.

Three fixes, smallest first, and the first two are worth doing whether or
not the wizard ships:

1. **Reject duplicate step ids in `to_specs`**, mirroring
   `validate_applets`. This is where the config loader already refuses
   malformed configs, so every entry point gets it — the wizard, a
   hand-edited file, and an agent's `PUT /api/config` alike.
2. **Enforce `RESERVED_STANZA_NAMES`** at the same point, and add
   `unified_index` to the list. One list, shared with the migrator, so
   the two paths can't disagree about what a stanza may be called.
3. **Make the wizard never propose a colliding name**: the name field
   starts from `default_name`, and if that is taken it suffixes
   (`slack-2`) and shows the conflict inline rather than failing on
   save. Validation still lives in the loader — the wizard is just being
   polite about it.

Enforcing this in the loader rather than the UI is the point. The config
file is the source of truth, so a rule the UI enforces alone is a rule
that a hand-edit silently breaks.

### Delete means "remove from config"

Delete drops the source's two steps from the config and stops there. The
data on disk is untouched, and that turns out to be the well-behaved
option rather than a compromise:

- `<data_root>/<name>/` keeps its raw stores and `rendered_md/` tree.
- Both fan-in steps declare `inputs = ["**/rendered_md"]`, and
  `build_grid_index` walks the data root **by directory**, not by
  config — so the removed source's documents keep getting indexed and
  stay searchable in Explore.
- The raw store is intact, so re-adding the source later resumes
  incrementally instead of re-downloading.

That makes Delete non-destructive and reversible, which is the right
default for a button sitting one click away in a table.

Removing the *data* is a separate feature and isn't designed here. When
someone does design it, note the prerequisite: **`grid_index` is
upsert-only.** `build_grid_index` walks the sidecar trees and per
document runs `DELETE … WHERE markdown_uuid = ?` followed by an insert
— the delete-then-insert of a document it is currently re-indexing.
Nothing in `etl/src/grid_index.rs` sweeps rows whose sidecar has
disappeared, so deleting a `rendered_md` tree by hand today leaves its
`grid_rows`, `markdowns` and `edges` rows in the index indefinitely.
An orphan sweep keyed on `source_name` would be the first piece of that
work. (Established by reading the module and its callers, not by
running it.)

## Phasing

| Phase | Delivers |
|---|---|
| **0** | Duplicate + reserved source-name rejection in `dag::config::to_specs`. Independently correct, and everything below assumes it. |
| **1** | The Manage screen inversion: AG Grid of sources with Run/Edit/Delete, **Add Data Source** above it, config editor demoted to an Advanced disclosure. Catalog crate + `GET /api/sources/catalog`; picker with filter; the generic (descriptor-less) flow for all twenty types; `toml_edit`-backed create, edit and delete-from-config. |
| **1b** | `step_runs` in `system/jobs.doltlite_db`, so the grid's Last synced / Last status columns have data. |
| **2** | Slack end to end: the browser-mode credential screen (`/api/credentials/*` → `latchkey auth browser slack`), `datalib-step probe` + `POST /api/sources/probe` for the live channel multi-select, since/media, review. Includes plumbing `--account` through `HttpRequest` — the multi-workspace bug above. The reference implementation the rest copy. |
| **3** | Credentials for the rest: test cookie-capture registration for `claude-ai`, and the `set`-mode token field → `latchkey auth set` on stdin for gitlab / notion / chatgpt / fastmail-dav. Includes the `auth`-probe fallback for services whose `credentialStatus` can only ever be `unknown`. |
| **4** | `GET /api/fs/browse` + `inspect` probes; descriptors for the file-backed sources. |
| **5** | Descriptors + probes for email (JMAP mailbox list — the second-best demo after Slack), notion, github, gitlab. |
| **6** | Nothing — edit is folded into phase 1 now (see above). |

Phases 1–2 are the vertical slice worth building first: they prove the
descriptor/probe contract against the hardest interesting case
(credentials *and* live discovery) while leaving every other source
better off than today.

## Part two: running one source and watching it

Sketched here only to keep the phase-1 seams honest; designed
separately.

The wanted thing is: hit Run on a grid row and watch **logs at several
levels of detail, rows added, bytes stored**.

> **Correction.** The first revision of this doc said the highest-value
> change here was emitting `DownloadReport` as a structured event. That
> machinery **no longer exists** — `6dae9185` deleted `DownloadReport`,
> `DbFileReport`, `DbSnapshot`, `TableStats`, `snapshot_db_file` and the
> rest, precisely because it was assembling something nobody read at a
> cost of four full table-count passes per source per sync. Do not
> resurrect it.

What survives is the cheap half, and it is the half worth using: the
**live counters** at the shared chokepoints —
`download_metrics::record_api_request` in `etl/src/http.rs`,
`record_upserts` in `bulk.rs` and `blob_cas.rs`, accumulating into a
task-local `DownloadMetrics` that `datalib_step::download` already
installs for the duration of a source's download. Those are increments
on an atomic, not table scans. They give API-call counts and rows
written per table, source-agnostically, with no provider aware they
exist.

So part two is:

- **Emit the live counters** as a periodic structured event on the step's
  NDJSON stream (there is still no metrics variant in
  `datalib_dag::events::Event`), and render them as a per-step counters
  panel. No before/after snapshots — for "bytes stored", stat the
  directory, the same walk the grid's storage column already does.
- **A focused run view.** Per-source runs already work
  (`--sync <step id>`, behind the row's Run button); what's missing is a
  view of one run rather than a row in a job table.
- **Log levels.** The worker writes the tracing subscriber's NDJSON to
  `<root>/system/job-logs/<id>.log` and `SourcesView` already classifies
  lines by `level`. A level filter and per-step grouping are UI work on
  data that is already there.
- **`step_runs`** (proposed above for the grid's Last synced / Last
  status columns) is the same table a run view would page through for
  history.
