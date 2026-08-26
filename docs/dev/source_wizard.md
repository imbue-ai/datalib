# Design: the "New Data Source" wizard

**Status: proposal, 2026-08-25. Nothing here is built yet.**
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

## Principles

1. **The config text stays the single source of truth.** The wizard is
   a sophisticated snippet generator, not a parallel config store. It
   ends by appending TOML to the same buffer the editor shows, and the
   user can see the exact text before it's written. No hidden state, no
   round-trip fidelity problem, no "the wizard and the file disagree".
2. **Providers own their own descriptors.** The catalog entry for
   `slack_api` lives next to `SlackConfig` in `slack_config`, the same
   way the schema does (issue #41's compose-don't-flatten discipline).
   The UI renders a generic form from a declarative descriptor and
   knows nothing about Slack.
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
subprocess spawn on every page load. Anything that needs credentials,
the network, or the filesystem goes through `datalib-step probe`,
which already links the providers.

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

  "credential": {
    "service": "slack",               // the latchkey service name
    "register": [],                   // services register … lines, if any
    "header": "Authorization: Bearer <token>",
    "howto_md": "…",                  // today's hints.rs text, as markdown
    "probe": "auth"
  },

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
`bool`, `date`, `path`, `string_list`, `select`, `multiselect`. This is
deliberately not a general form-builder DSL: anything a provider can't
express in those kinds belongs in the raw-TOML escape hatch, not in a
richer descriptor language.

`target` is a dotted path into the download step's `params` tree
(`sync.channels` → `[steps.params.sync] channels = …`). A field may
carry `"phase": "render"` to land on the render step instead (e.g.
email's `only_render_labels`, beeper's `period`). That mapping is the
whole trick: it lets one generic renderer serve twenty providers, and
it lives in the crate that owns the struct being filled.

Descriptors are optional. A type with none gets
`kind: "generic"` — name field, params textarea seeded from
`all_sources.toml`, and the `credential.howto_md` text if any.

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
| `auth` | credentials work; return an identity summary | `auth.test` | `.well-known/jmap` | n/a |
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

### What latchkey actually offers (verified against 3.1.0 and 3.6.0)

`latchkey auth browser <service>` — *"Login to a service via the
browser and store the API credentials."* It exists in the pinned
**3.1.0** as well as the globally-installed 3.6.0. For Slack it opens a
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
  name come from. So `--op auth` is redundant for Slack; probes earn
  their keep on `list.channels`.

### Coverage: browser login reaches two of our seven API sources

| datalib type | latchkey service | authOptions |
|---|---|---|
| `slack_api` | `slack` | **browser**, set |
| `github_api` | `github` | **browser**, set |
| `gitlab_api` | `gitlab` | set |
| `notion_api` | `notion` | set |
| `chatgpt_api` | `chatgpt` (user-registered) | set |
| `claude_api` | `claude-ai` (user-registered) | set |
| `email` (JMAP) | `fastmail` + `fastmail-content` (user-registered) | set |

So the credential screen has **two modes, chosen from `authOptions`**:

- **`browser`** → one button, "Connect Slack". Backend runs
  `latchkey ensure-browser` then `latchkey auth browser slack`, streams
  status, then re-reads `services info` to confirm and to name the
  account. Nothing is typed, nothing is pasted, latchkey is never named.
- **`set`** → a labeled secret field ("Paste your Slack token", "Paste
  your claude.ai sessionKey") whose value the backend pipes to
  `latchkey auth set` **on stdin** — never argv, which is
  world-readable via `ps` — after running `services register` first for
  the user-registered services. Still no latchkey command for the user
  to run; the existing `hints.rs` prose becomes the *how to get this
  token* text beside the field, not an instruction to visit a terminal.

The copy-this-command escape hatch stays available behind a
disclosure, because it is the only thing that works when the server is
headless.

Notably, `google-gmail` supports `browser` (via `auth browser-prepare`,
which provisions an OAuth client). That is the Thunderbird-grade Gmail
onboarding — but it needs a Gmail-API provider we don't have; today
Gmail arrives as a Takeout `.mbox`. Worth knowing the credential half is
already solved if we ever build that provider.

### Three things this turns up

1. **Multi-account is a live bug, not a wizard feature.** latchkey keys
   Slack credentials by workspace (`thad@imbue-ai`) and errors when a
   service has more than one stored account and no `--account` is
   passed. `datalib/backend/etl/src/http.rs` builds
   `latchkey curl …` with no `--account` ever. A second Slack workspace
   therefore breaks *every* request today. The wizard forces this into
   the open on day one, since "connect an account" is its first screen:
   it needs an account field on the source config and `--account`
   plumbed through `HttpRequest`. **Fix this in phase 2, alongside the
   Slack descriptor.**
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

The wizard needs a home, and the thing it should sit on top of is a real
table of configured sources — replacing today's plain-HTML list of step
ids beside a textarea. AG Grid is already a dependency
(`ag-grid-community` + `ag-grid-vue3` + `ag-grid-enterprise` 35.2.1), so
the Manage tab can look like the Explore tab instead of like a form.

Proposed columns, and — the part that matters — where each number
actually comes from:

| Column | Source of truth |
|---|---|
| Source (icon + name) | the config's step ids, as today |
| Type | the step `command`'s provider word |
| Account / status | `latchkey services info <service>` → account key + `credentialStatus` |
| Last sync | `sync_jobs` (`app_schema`): `finished_at`, `state`, `error` |
| Documents | `SELECT source_name, COUNT(*) FROM markdowns GROUP BY source_name` |
| Rows | `grid_rows JOIN markdowns USING (markdown_uuid)`, grouped by `source_name` — see [#171](https://github.com/imbue-ai/datalib/issues/171) |
| Storage | directory walk of the step's declared outputs |
| Actions | Run · Edit · Delete |

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
| **1** | Catalog crate + `GET /api/sources/catalog`; the AG Grid sources table with Run/Edit/Delete and storage bars; picker screen with filter; the generic (descriptor-less) flow for all twenty types; `toml_edit`-backed create, edit **and** delete-from-config. |
| **2** | Slack end to end: the browser-mode credential screen (`/api/credentials/*` → `latchkey auth browser slack`), `datalib-step probe` + `POST /api/sources/probe` for the live channel multi-select, since/media, review. Includes plumbing `--account` through `HttpRequest` — the multi-workspace bug above. The reference implementation the rest copy. |
| **3** | The `set`-mode credential screen (token field → `latchkey auth set` on stdin) for gitlab / notion / chatgpt / claude-ai / fastmail, incl. `services register` for the user-registered ones. |
| **4** | `GET /api/fs/browse` + `inspect` probes; descriptors for the file-backed sources. |
| **5** | Descriptors + probes for email (JMAP mailbox list — the second-best demo after Slack), notion, github, gitlab. |
| **6** | Nothing — edit is folded into phase 1 now (see above). |

Phases 1–2 are the vertical slice worth building first: they prove the
descriptor/probe contract against the hardest interesting case
(credentials *and* live discovery) while leaving every other source
better off than today.

## Part two: running one source and watching it

Sketched here only to keep phase-1 seams honest; designed separately.

The wanted thing is: pick a source, hit Run, and watch **logs at
several levels of detail, rows added, bytes stored**. Most of that data
already exists and simply isn't plumbed to the UI:

- `datalib_etl::download_metrics` already computes, source-agnostically,
  API-request counts, per-table rows upserted, and before/after row and
  byte deltas of the raw store. Today `datalib_step::download` uses the
  resulting `DownloadReport` only for the output-changed claim and a
  `tracing::info!`. **It is not emitted as a structured event** — there
  is no metrics variant in `datalib_dag::events::Event`. Adding one, and
  a per-step counters panel that consumes it, is the single highest-value
  change in this half.
- Per-source runs already work end to end (`--sync <step id>`, surfaced
  as the table's Sync button); what's missing is a focused *run view*
  rather than a row in a job table.
- Log levels: the worker writes NDJSON from the tracing subscriber to
  `<root>/system/state/job-logs/<id>.log`, and `SourcesView` already
  classifies lines by `level`. A level filter and a per-step grouping
  are UI work on data that's already there.
