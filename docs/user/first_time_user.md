# datalib (Project Data Liberation ✊) — first-time user guide

Liberate and own your data. Run powerful AI tools on it, on your terms.

This guide installs the command-line tools and walks you through your
first mirror. Two other ways in, if this one doesn't fit: the macOS
desktop app on the [latest release](https://github.com/imbue-ai/datalib/releases/latest)
does the same thing behind a folder picker, and the
[Docker image](docker.md) keeps everything inside a container that
sees only the folders you mount, with a demo library already loaded.
The sections on credentials (step 2) and on getting your data back out
(step 8) apply to all three.

> 🛑 **WITH GREAT POWER COMES GREAT RESPONSIBILITY** 🛑
>
> These tools accumulate a lot of high-value data in one place. Make
> sure the computer you run them on is a safe place to keep it.
>
> Think at least three times before running an agent on this data,
> then think again. Understand the
> [lethal trifecta](https://simonwillison.net/2025/Jun/16/the-lethal-trifecta/):
> nearly everything these tools collect is both **private data** and
> **untrusted content**.
>
> Most agentic harnesses are effectively **exfiltration scripts**.
> Running one on your private data uploads that data to a third party,
> where you have very little control over what happens next. Ask
> yourself: "would the people who sent me these messages be OK with me
> sending them to Anthropic, OpenAI, or Google?" Because that is what
> you are doing when you run an agent on this data.
>
> **Deletes might not actually delete from your local copy.** The
> stores are doltlite databases (SQLite with git-shaped history), which
> keep every version of your data as it changes. That helps you recover
> from unintended data loss, and it cuts both ways: a message deleted at
> the source is gone from the current view but still recoverable from
> the history. If you truly need something gone, delete the whole
> `.doltlite_db` file.
>
> **Terms of service.** The Claude.ai and ChatGPT sources use the same
> undocumented web APIs your browser does, with your own session. Check
> the terms of the services you use.

## 0. Prerequisites

You need `node` on your `PATH`:

```sh
brew install node
```

The tools shell out to two Node programs at sync time, fetching each
on demand with `npx`: `latchkey`, which holds your credentials, and
`qmd`, which builds the semantic search index. Nothing else is needed.

## 1. Install the tools and make a data root (here it's `~/datalib`)

One command installs the binaries from the GitHub Releases page — no
`gh`, no GitHub account:

```sh
curl -LsSf https://raw.githubusercontent.com/imbue-ai/datalib/main/scripts/install.sh | sh
```

This downloads the latest release tarball, verifies its checksum, and
drops the tools into `~/.local/bin`. The ones you will meet in this
guide:

- `datalib-http` — the app: a local web server with the UI built in.
- `datalib-dag` and `datalib-step` — the sync pipeline, which the app
  runs for you and which you can also run from the terminal.
- `datalib-applet` — serves the search grid inside the app.
- `datalib-doltlite` — the shell for reading and exporting your stores
  (step 8).
- `datalib-migrate-config` — rewrites a config file from an older
  datalib (step 3).

Also installed: `datalib-fsindex` and `datalib-dirtree-diff` (a
standalone directory scanner and a diff of two scans) and the two
`latchkey-curl-*` binaries the web-API sources fetch through. If
`~/.local/bin` isn't already on your `PATH`, the script prints the exact
line to add to your `~/.zshrc` — add it and restart your shell.

Three optional knobs:

- `DATALIB_INSTALL_DIR` — install somewhere else, e.g.
  `DATALIB_INSTALL_DIR=~/bin curl -LsSf …/install.sh | sh`.
- `DATALIB_VERSION` — pin a release tag instead of `latest`, e.g.
  `DATALIB_VERSION=v0.30.1 curl -LsSf …/install.sh | sh`.
- `DATALIB_LIBC` — Linux only: `gnu` or `musl`. Auto-detected (musl
  distros like Alpine get the fully-static musl build); set
  `DATALIB_LIBC=musl` to force the static build on a glibc distro —
  it runs on any Linux of the right architecture.

> The install script supports macOS on Apple Silicon and Linux
> (x86_64 / arm64, glibc or musl); it auto-detects your platform and
> pulls the matching tarball. The rest of this guide is written
> macOS-first (Homebrew, `pbpaste`) — on Linux, substitute your package
> manager and clipboard tool.

Next, make the data root — the folder everything gets written into —
and work from there:

```sh
mkdir -p ~/datalib && cd ~/datalib
```

Verify the install:

```sh
datalib-dag --version
```

## 2. Get access to some data

The options below cover the sources in the sample config. For every
other source — Gmail over the API, Fastmail, ChatGPT, Notion, GitHub,
Signal and WhatsApp backups off an Android phone, LinkedIn exports, and
the rest — see [**getting your data**](getting_your_data.md).

> 🛑 **READ BEFORE PROCEEDING** 🛑
>
> The commands in this section store live session cookies and tokens on
> your machine via `latchkey`. **Any process, script, or AI agent that
> can run commands as your user can invoke `latchkey` (or read its
> store) and thereby ACT AS YOU on these services** — read every
> conversation, send messages, change settings, impersonate you to
> coworkers. There is no password prompt, MFA challenge, or confirmation
> gate between a shell command and your identity on these services.
>
> Only run these steps on a machine you trust, and be aware that *every*
> local agent inherits this authority for as long as the credentials
> remain valid.

You don't need to install `latchkey`: the commands below run it through
`npx`, which fetches it on demand (the `node` install from step 0 ships
with `npx`).

### Option 1: A Google Takeout export (no credentials needed)

Google Takeout (<https://takeout.google.com>) lets you export your own
data out of Google's silos. Useful targets:

- **Mail** — exports as a single `.mbox` (one file for "All mail
  Including Spam and Trash"). The email source reads it directly.
- **Chat**, **Voice**, **Maps**, **YouTube history**, **Gemini** — the
  `google_takeout` source reads the unpacked tree. Chat and Voice
  render to markdown today; the rest land in the raw store.

Steps:

1. Go to <https://takeout.google.com>, **Deselect all**, then tick
   just the products you want. For Mail, expand the row and confirm
   **"Include all messages in Mail"** (or pick specific labels).
2. Choose **Export once**, **.zip**, and the largest split size you're
   comfortable with. Submit the request.
3. Google emails you a download link when it's ready (minutes to
   hours, depending on mailbox size). Download the archive(s) and
   unpack them somewhere stable — these instructions assume
   `~/backups/Takeout/`:

   ```sh
   mkdir -p ~/backups
   unzip ~/Downloads/takeout-*.zip -d ~/backups/
   ```

   After unpacking, your Gmail mbox should live at:

   ```
   ~/backups/Takeout/Mail/All mail Including Spam and Trash.mbox
   ```

   The sample config in the next step has an `email` source that
   points at exactly that path.

### Option 2: Slack (easy, supported flow)

Slack is built into latchkey. One command opens a browser, you sign in,
and latchkey keeps the session:

```sh
npx -y latchkey auth browser slack
```

The sample config includes a Slack source, so do this before the first
sync if you keep that source.

### Option 3: Claude.ai (fiddly)

Claude.ai has no official API for your conversations, so this uses the
session cookie your browser already has. It takes a trip through
DevTools.

a. Register the `claude-ai` service with latchkey (one-time):

   ```sh
   npx -y latchkey services register claude-ai --base-api-url="https://claude.ai/"
   ```

b. Paste the next command into your terminal **but don't run it yet** —
   the following step puts the cookie on your clipboard, so you want
   this staged first. `$(pbpaste)` is used instead of pasting the cookie
   value literally because zsh and bash record the command before
   expansion, so your shell history keeps the harmless `$(pbpaste)`
   text rather than your live session token:

   ```sh
   npx -y latchkey auth set claude-ai -H "Cookie: sessionKey=$(pbpaste)"
   ```

c. Open [claude.ai](https://claude.ai) in a logged-in browser tab and
   copy your `sessionKey` cookie. It's `HttpOnly`, so it's not visible
   to `document.cookie` — read it from DevTools directly:

   - Open DevTools → **Application** tab → **Storage** → **Cookies** →
     `https://claude.ai`.
   - Find the row named `sessionKey` and copy its **Value**.

   Now switch back to your terminal and press Enter to run the staged
   command — `$(pbpaste)` expands to the cookie you just copied.

If a sync later fails with an auth error, the error message repeats
these steps for whichever source failed.

## 3. Configuration

The config lives at `config.toml` in your data root. Each source is a
**group** with an `ingest` step (bring the data in) and a
`render_markdown` step (turn it into readable markdown), plus two shared
index steps that fan in over everything rendered. None of them names a
command: a step without one is datalib's own, and the group's `type`
says which source it is. A one-source config looks like this:

```toml
data_root = "~/datalib"

[[groups]]
id = "claude"
name = "Claude"
type = "claude"

[[steps]]
group = "claude"
function = "ingest"
[steps.params]
api = {}

[[steps]]
group = "claude"
function = "render_markdown"
inputs = ["claude/ingest"]

[[groups]]
id = "unified_index"

[[steps]]
group = "unified_index"
function = "grid_index"
inputs = ["claude/render_markdown"]

[[steps]]
group = "unified_index"
function = "qmd_index"
inputs = ["claude/render_markdown"]

[[applets]]
group = "unified_index"
id = "unified_index"
command = "datalib-applet unified_index"
```

Each step says which group it belongs to and what it does there, and
the pair names a directory — `claude/ingest`, `claude/render_markdown`
— that the next step's `inputs` refer to. Two TOML rules worth knowing
before you hand-edit: `data_root` has to come *above* the first `[[…]]`
header, and within a step the `params` sub-table comes last — anything
you write after a `[…]` header belongs to that header's table until the
next one.

You normally don't write this by hand. The app's first-run screen
writes the index steps and the applet for an empty folder, and the
**Manage** tab's **Add a source** button fills in a source (next step).
If you'd rather hand-edit, copy
[**configs/dag_example.toml**](https://github.com/imbue-ai/datalib/blob/main/configs/dag_example.toml),
a complete commented example.

For ready-made configs and each source's knobs, the files in
[docs/user/config_examples/](https://github.com/imbue-ai/datalib/tree/main/docs/user/config_examples)
are the reference — copy a file, or just one source's entries, straight
into `<data_root>/config.toml`:

- [**sample_config.toml**](https://github.com/imbue-ai/datalib/blob/main/docs/user/config_examples/sample_config.toml)
  — the Slack source, the Claude source, and an email source that
  reads a Google Takeout `.mbox` from disk (the trio step 2 above sets
  up).
- [**claude_only.toml**](https://github.com/imbue-ai/datalib/blob/main/docs/user/config_examples/claude_only.toml)
  — just the Claude source, plus the two index steps.
- [**all_sources.toml**](https://github.com/imbue-ai/datalib/blob/main/docs/user/config_examples/all_sources.toml)
  — every supported source type with realistic defaults (including
  every input mode for email and contacts).

Upgrading from an earlier datalib? A `config.toml` written for one —
steps naming a `datalib-step download …` command, a group `type`
spelled for its method (`slack_api`, `claude_export`, `carddav`), or
an ingest step whose params still say `sync` or `common.input_path` —
is refused by this version, with an error naming the fix, and is
rewritten once:

```sh
datalib-migrate-config ~/datalib --force     # rewrites ~/datalib/config.toml
```

It keeps the original beside the result as `config.toml.orig`. Comments
from the old file don't carry over, so review the result. A much older
root with only a `config.yaml` is not convertible any more: set it up
again from the app.

Credentials are never in the config — sources that need them ask
`latchkey` at run time.

Whichever route you take, eyeball the `data_root` line at the top to
make sure it points at the folder you created.

## 4. Run the sync

The easiest way is through the app. From your data root:

```sh
datalib-http ./
```

It binds to `http://127.0.0.1:8731` by default and opens that URL in
your browser. On an empty folder the first-run screen offers to write a
config; the **Manage** tab then lets you add sources, and **Sync all**
runs the pipeline (`datalib-dag` under the hood).

The URL it opens carries a one-time `?token=…`, the way a Jupyter
notebook server's does — the local API is authenticated, so that no web
page you happen to have open can reach it. Your browser trades the
token for a session cookie on that first load and drops it from the
address bar. If you want to open the app in a *different* browser (or
you closed the tab and lost the URL), the line the server printed is
still in your terminal, and the token is on disk at
`<data_root>/system/api-token`.

Prefer the terminal? Run the pipeline directly on your config:

```sh
datalib-dag config.toml
```

(`datalib-step` must be findable: on `PATH`, next to `datalib-dag` —
which is how the installer lays them out — or via `--binary-dir`. Pass
`--sync <group>/ingest` to sync just one source and what depends on it.)

The first run is slow: it downloads everything. All of the data goes
into the data root.

The run is stoppable and resumable. Ctrl-C it, then run the same
command again to pick up where it left off. It commits what it has when
you Ctrl-C, so stopping is not instant.

Later runs of the same command are incremental and should be much
faster.

**During the run** you'll see, roughly in order:

- An `ingest` step per source: enumerate what the source has, then a
  progress bar as each new or changed item is fetched. New items come
  first.
- A `render_markdown` step per source: each conversation or document
  rendered into readable markdown, attachments included.
- The `grid_index` step: one row per message or document written into
  the SQL store at `<data_root>/unified_index/grid_index/db.doltlite_db`.
- The `qmd_index` step: builds the semantic search index. **The first
  run is slow** — embedding takes roughly 5–10 minutes per thousand
  chunks on CPU, after a one-time download of the models. It's
  resumable, so Ctrl-C and re-run is safe. Re-runs after the backlog
  drains take seconds.

**On disk afterwards** (with `data_root = "~/datalib"`):

```
~/datalib/
├── config.toml
├── claude/                         # one directory per group …
│   ├── ingest/                     #   the captured raw stores (precious) …
│   │   ├── entities.doltlite_db
│   │   └── blobs.doltlite_db
│   └── render_markdown/            #   … and the rendered .md tree
│       └── …
├── slack/
│   ├── ingest/
│   └── render_markdown/
├── gmail-takeout/
│   └── …
├── unified_index/                  # the shared indexes, rebuildable
│   ├── grid_index/db.doltlite_db   #   grid rows + markdowns + edges
│   └── qmd_index/qmd/index.sqlite  #   the semantic search index
└── system/                         # everything that isn't a source
    ├── dag_state.json              # scheduler state (which steps are up to date)
    ├── api-token                   # the running server's bearer token
    ├── lock                        # held by the running server
    ├── feedback.doltlite_db        # feedback you filed (nothing regenerates it)
    ├── jobs.doltlite_db            # sync job queue + history
    ├── job-logs/                   # one log per sync job
    ├── usage.doltlite_db           # bytes on disk over time
    ├── media/                      # attachment bytes served to the UI
    └── frontend/                   # UI components the applets contribute
```

> **Backups:** the bulky **derived** trees — each `<name>/render_markdown/`,
> `unified_index/`, and `system/media/` — are rebuilt from your raw
> stores by re-running the pipeline, and each carries a `CACHEDIR.TAG`,
> so cache-aware backup tools skip them automatically:
>
> ```sh
> restic backup ~/datalib --exclude-caches        # or: borg create --exclude-caches
> tar --exclude-caches -czf datalib-backup.tgz ~/datalib
> ```
>
> What's left in the backup is exactly what you want to keep: every
> `<name>/ingest/` store (the captured data), `config.toml`, and
> `system/` (scheduler state, filed feedback, sync history).

A per-step report prints when the run finishes, and a machine-readable
`run_summary` event lands on `datalib-dag`'s stderr (NDJSON — tee
stderr if you want to keep it). The exit code is non-zero if any step
failed.

## 5. Browse the result

If you synced from the app, you're already looking at the result —
`datalib-http` is the single-binary backend with the web UI embedded.
If you ran `datalib-dag` from the terminal instead, start it now from
your data root:

```sh
datalib-http ./
```

It binds to `http://127.0.0.1:8731` by default and opens that URL in
your browser. Pass `--no-open` if you'd rather click in yourself, and
set `DATALIB_BIND=127.0.0.1:<port>` to change the listen address.

The API requires a token (see step 4). With `--no-open` you'll want the
URL the server prints, which already has it; to reach the API from a
script instead:

```sh
curl -H "Authorization: Bearer $(cat ./system/api-token)" \
  http://127.0.0.1:8731/api/health
```

A fresh token is minted every time the server starts, so re-read that
file rather than saving a copy. `DATALIB_TOKEN=<value>` pins one if you
need it stable across restarts.

## 6. Re-syncing

Re-run the sync (**Sync all** in the app, or `datalib-dag config.toml`)
whenever you want to pull in what's new. Downloads are incremental and
the semantic index is content-hashed, so a re-run over an unchanged
corpus is a fast no-op.

## 7. Querying the search index directly with qmd

You can also query the semantic index from the command line, by
pointing `qmd` at the sqlite file under your data root via the
`INDEX_PATH` env var:

```sh
INDEX_PATH=~/datalib/unified_index/qmd_index/qmd/index.sqlite \
    npx -y @tobilu/qmd query "hello"
```

`qmd status` against the same `INDEX_PATH` shows collections and
document counts.

## 8. Getting your data back out

The point of mirroring your data locally is that it stays yours, so it
is worth knowing the exit before you need it. Two of the three copies
are already in open formats you can read with no datalib at all:

- **The markdown.** `<name>/render_markdown/` is a tree of ordinary
  UTF-8 `.md` files, one per conversation or document. Copy it
  anywhere; every text editor and search tool on your machine already
  reads it.
- **The databases.** The `.doltlite_db` files are
  [doltlite](https://github.com/dolthub/doltlite) stores — SQLite's SQL
  engine over a versioned, content-addressed file format, which is what
  keeps the record of what a source *changed or deleted* between syncs.
  The trade is that the file itself is not a SQLite file: point stock
  `sqlite3` at one and it says `file is not a database`.

  So export it. `datalib-doltlite` was installed alongside
  `datalib-dag` in step 1, and one pipe writes a plain SQLite database
  that every SQLite tool — `sqlite3`, Datasette, pandas, DB Browser,
  your language's stdlib — opens directly:

  ```sh
  datalib-doltlite -readonly ~/datalib/unified_index/grid_index/db.doltlite_db .dump \
    | sqlite3 ~/grid.sqlite

  sqlite3 ~/grid.sqlite "SELECT provider, count(*) FROM grid_rows GROUP BY 1;"
  ```

  The same command works on any `.doltlite_db` under your data root,
  including the raw per-source stores under `<name>/ingest/`, whose
  attachment bytes come across intact.

  What the export gives you is the current state of every table, with
  its schema and indexes. What it leaves behind is the version history
  — the commit per sync that the diff-since-last-time answers come
  from. Keep the original if you want that; the export is a snapshot
  for other tools.

`datalib-doltlite` is a `sqlite3`-compatible shell, so you can also
just explore in place — `datalib-doltlite -readonly <file>` drops you
in a REPL. Pass `-readonly` whenever you are only looking: a second
writer against a live store can wedge your next sync. If you prefer a
GUI, a build of DB Browser for SQLite patched to open doltlite files is
at <https://github.com/thadd3us/sqlitebrowser/releases> (macOS). More
recipes, including the commit history and per-sync diffs, are in
[`docs/dev/doltlite.md`](../dev/doltlite.md).
