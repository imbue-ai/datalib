# Project Data Liberation ✊ - First-time user guide

Liberate your data from silos. Run SOTA AI and data tools on it, on your own terms.

> 🛑 **<span style="color:red">WITH GREAT POWER COMES GREAT RESPONSIBILITY</span>** 🛑
>
> <span style="color:red">**These tools allow you to accumulate a lot of high-value
> data into a single place. Hopefully, the computer where you run these tools is a
> safe place to store this data.**</span>
>
> <span style="color:red">**Please think at least 3x before running an agent on this
> data, then think again. Make sure you understand the full implications of the
> [lethal trifecta](https://simonwillison.net/2025/Jun/16/the-lethal-trifecta/).
> Most of the data accumulated by these tools should be considered both <span style="color:red">**Private
> Data**</span> and <span style="color:red">**Untrusted Content**</span>.**</span>
>
> <span style="color:red">**Also remember that most agentic harnesses are effectively
> (!!!) EXFILTRATION SCRIPTS (!!!), and running them on your private data will
> upload it to a third party where you have very little control over what happens
> with it next. Ask yourself: "would the people who sent me these messages be
> OK with me sending them to Anthropic, OpenAI, or Google?"  Because that's exactly what
> you're doing when you run an agentic harness on this data.**</span>

 <span style="color:red">**Deletes might not actually delete from your local copy.**
 We use Doltlite (a version of SQLite) to keep versions of your data as it changes over time. 
 This can help you recover from unintended data loss, but is a double-edged sword.
 Deletions in your data sources, even if they propagate into the current
 version of your data, as stored and presented by our tools, are still in theory recoverable from
 the version history.  If you truly need to delete, you'll have to remove the whole doltlite_db file,
 not just delete from the data source.
 **</span>

## 0. Setup pre-reqs

If you don't already have it, you'll need `node` on `PATH`:

```sh
brew install node
```

- `node` — the qmd indexer shells out to latchkey, and `npx -y @tobilu/qmd@<version>` 
  during the `qmd_index` step.

## 1. Install the CLI and make a data_root playground (here it's `~/datalib`)

Now that the repo is public, you can install the binaries straight from the
GitHub Releases with a one-line `curl` script — no `gh` and no GitHub auth:

```sh
curl -LsSf https://raw.githubusercontent.com/imbue-ai/datalib/main/scripts/install.sh | sh
```

This downloads the latest release tarball, verifies its checksum, and drops
`datalib-dag`, `datalib-step`, `datalib-http`, `datalib-doltlite` (the
shell for reading and exporting your stores — see step 8), and the
latchkey curl shim into `~/.local/bin`. If that directory isn't already on your `PATH`,
the script prints the exact line to add to your `~/.zshrc` — add it and
restart your shell so the installed commands resolve.

Three optional knobs:

- `DATALIB_INSTALL_DIR` — install somewhere else, e.g.
  `DATALIB_INSTALL_DIR=~/bin curl -LsSf …/install.sh | sh`.
- `DATALIB_VERSION` — pin a release tag instead of `latest`, e.g.
  `DATALIB_VERSION=v0.13.0 curl -LsSf …/install.sh | sh`.
- `DATALIB_LIBC` — Linux only: `gnu` or `musl`. Auto-detected (musl
  distros like Alpine get the fully-static musl build); set
  `DATALIB_LIBC=musl` to force the static build on a glibc distro —
  it runs on any Linux of the right architecture.

> The install script supports macOS arm64 (Apple Silicon) and Linux
> (x86_64 / arm64, glibc or musl); it auto-detects your platform and pulls
> the matching release tarball. The rest of this guide is written
> macOS-first (Homebrew, `pbpaste`) — on Linux, substitute your package
> manager and clipboard tool.

Next, make the data_root playground — this is where the tools will download
your data — and work from there:

```sh
mkdir -p ~/datalib && cd ~/datalib
```

Verify the install:

```sh
datalib-dag --version
```

## 2. Get access to some data

The options below cover the sources wired into the sample config. For a
fuller per-source cheat sheet on getting your data onto disk — including
Signal and WhatsApp backups off an Android phone — see
[**getting your data**](/docs/user/getting_your_data.md).

> 🛑 **RED WARNING — READ BEFORE PROCEEDING** 🛑
>
> The commands in this section store live session cookies for `claude.ai`
> and Slack on your machine via `latchkey`. **Any process, script, or AI
> agent that can run CLI programs as your user account can invoke
> `latchkey` (or read its store) and thereby ACT AS YOU on these
> services** — read every conversation, send messages, change settings,
> impersonate you to coworkers, etc. There is no additional password
> prompt, MFA challenge, or confirmation gate between a shell command
> and your identity on these services.
>
> Only run these steps on a machine you trust, and be aware that *every*
> local agent inherits this authority for as long as the cookies remain valid.

You don't necessarily need to install `latchkey` — the commands below invoke it via
`npx`, which fetches it on demand (the `node` install from step 0 ships
with `npx`).

### Option 1: Download some Google Takeout data (no Latchkey necessary)

Google Takeout (<https://takeout.google.com>) lets you export your own
data out of Google's silos. Useful targets for this project:

- **Mail** — exports as a single `.mbox` (one file for "All mail
  Including Spam and Trash"). The email source below ingests this
  directly; no credentials needed.
- **Chat**, **Maps (Your Timeline)**, **YouTube history** — also
  exportable; not wired into the sample config yet but live on disk
  the same way once you've unpacked them.

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

   The sample config in the next step has an `email` source
   that points at exactly that path.


### Option 2: Register Slack with latchkey (easy, supported flow)

  Register Slack via latchkey's browser flow (the sample config in the
  next step includes a Slack source, so this is needed for the sync to
  succeed):

  ```sh
  npx -y latchkey auth browser slack
  ```

### Option 3: Register Claude web with latchkey (tricky)

This is tricky, requires you to do sketchy things in your browser.

It also might not work inside Minds because of the Chrome handshake issues.
When Minds runs latchkey, it doesn't use our curl shim with the Chrome 131 handshake
because latchkey reaches out to its gateway.

a. Register the `claude-ai` service with latchkey (one-time):

   ```sh
   npx -y latchkey services register claude-ai --base-api-url="https://claude.ai/"
   ```

b. Paste the registration command into your terminal **but don't run it
   yet** — the next step puts the cookie on your clipboard, so you want
   this command staged first. `pbpaste` is used (instead of pasting the
   cookie value literally) because zsh/bash record the pre-expansion
   command in history, so history ends up storing the harmless
   `$(pbpaste)` text instead of your live session token:

   ```sh
   npx -y latchkey auth set claude-ai -H "Cookie: sessionKey=$(pbpaste)"
   ```

c. Open [claude.ai](https://claude.ai) in a logged-in browser tab and
   copy your `sessionKey` cookie. It's `HttpOnly`, so it's not visible
   to `document.cookie` — you have to read it from DevTools directly:

   - Open DevTools → **Application** tab → **Storage** → **Cookies** →
     `https://claude.ai`.
   - Find the row named `sessionKey` and copy its **Value**.

   Now switch back to your terminal and press Enter to run the staged
   command — `$(pbpaste)` will expand to the cookie you just copied.


## 3. Configuration

The running config lives at `config.toml` in your data_root, and it's a
**steps** config: each source becomes a `<name>.download` +
`<name>.render` step pair, plus two shared index steps that fan in over
everything rendered. A one-source config looks like this:

```toml
data_root = "~/datalib"

[[steps]]
id = "claude.download"
command = "datalib-step download claude_api"
outputs = ["claude/raw"]
[steps.params]
sync = {}

[[steps]]
id = "claude.render"
command = "datalib-step render claude_api"
inputs = ["claude/raw"]
outputs = ["claude/rendered_md"]

[[steps]]
id = "grid_index"
command = "datalib-step grid_index"
inputs = ["**/rendered_md"]
outputs = ["unified_index/grid"]

[[steps]]
id = "qmd_index"
command = "datalib-step qmd_index"
inputs = ["**/rendered_md"]
outputs = ["unified_index/qmd"]
```

Two TOML rules worth knowing before you hand-edit: `data_root` has to
come *above* the first `[[steps]]`, and within a step the `params`
sub-table comes last — anything you write after a `[…]` header belongs
to that header's table until the next one.

You normally don't write this by hand — the app's **Setup** tab
scaffolds it for you (next step). If you'd rather hand-edit, copy
[**configs/dag_example.toml**](https://github.com/imbue-ai/datalib/blob/main/configs/dag_example.toml),
a complete commented example.

For ready-made configs and each source's knobs, the files in
[docs/user/config_examples/](https://github.com/imbue-ai/datalib/tree/main/docs/user/config_examples)
are the reference — all in the steps format, so you can copy a file (or
just one source's step pair) straight into `<data_root>/config.toml`:

- [**sample_config.toml**](https://github.com/imbue-ai/datalib/blob/main/docs/user/config_examples/sample_config.toml)
  — the Slack source, the Claude API source, and an email source that
  reads a Google Takeout `.mbox` from disk (the trio step 2 above sets
  up).
- [**claude_only.toml**](https://github.com/imbue-ai/datalib/blob/main/docs/user/config_examples/claude_only.toml)
  — just the Claude source, plus the two index steps.
- [**all_sources.toml**](https://github.com/imbue-ai/datalib/blob/main/docs/user/config_examples/all_sources.toml)
  — every supported source type with realistic defaults (including
  both input modes for email and contacts).

(Upgrading from an earlier datalib? Nothing reads `config.yaml` any
more — in either of its old shapes, the YAML steps format or the much
older stanza-based `sources:` one. Convert it once:

```sh
datalib-migrate-config ~/datalib     # writes ~/datalib/config.toml
```

It auto-detects which of the two you have, writes `config.toml` beside
the old file, and refuses to overwrite an existing one. Your
`config.yaml` is left untouched; review the result, then delete it.
Comments from the old file don't carry over.)

Credentials are not in the config — downloaders that need them use `latchkey` at runtime.

Whichever route you take, eyeball the `data_root` parameter at the top
to make sure it is writing to the directory you created.

## 4. Run the sync

The easiest way is through the app. From your data_root:

```sh
datalib-http ./
```

It binds to `http://127.0.0.1:8731` by default and opens that URL in
your default browser. The **Setup** tab scaffolds `config.toml` if you
don't have one yet and lets you add sources; **Sync now** then runs the
pipeline (`datalib-dag` under the hood).

The URL it opens carries a one-time `?token=…`, the way a Jupyter
notebook server's does — the local API is authenticated, so that no web
page you happen to have open can reach it. Your browser trades the
token for a session cookie on that first load and drops it from the
address bar. If you want to open the app in a *different* browser (or
you closed the tab and lost the URL), the line the server printed is
still in your terminal, and the token is on disk at
`<data_root>/system/api-token`.

Prefer the terminal? Run the pipeline directly on your steps config:

```sh
datalib-dag config.toml
```

(`datalib-step` must be findable: on `PATH`, next to `datalib-dag` —
which is how the installer lays them out — or via `--binary-dir`. Pass
`--sync <step-id>` to sync just a subset of your sources.)

The first time you run this, it is slow and takes a long time to download everything.
All of the data will be going into the data_root directory.

This process is meant to be stoppable and resumable, so you can control-C it,
Then run the same command again to resume downloading.
It does do some database commits when you control-C, so that part is not instant. 

Subsequent runs of the same command are meant to be incremental delta downloads,
and should be faster.

**During the run** you'll see, roughly in order:

- A `download` step per source: per-org conversation enumeration, then
  a progress bar as each new / updated / overlap conversation is
  fetched from `claude.ai/api`. New conversations are fetched first.
- A `render` step per source: each conversation rendered into intelligible Markdown (including image attachments).
- The `grid_index` step: rows written into the doltlite SQL store at `<data_root>/unified_index/grid/db.doltlite_db`.
- The `qmd_index` step: builds the search index. **First run is slow** —
  embedding ~5–10 minutes per thousand chunks on CPU. It's resumable, so
  Ctrl-C and re-run is safe. Re-runs after the backlog drains take
  seconds.

**On disk afterwards** (with `data_root: ~/datalib`):

```
~/datalib/
├── claude_web/                     # one directory per source stanza …
│   ├── raw/                        #   its captured raw stores …
│   │   ├── entities.doltlite_db
│   │   └── blobs.doltlite_db
│   └── rendered_md/                #   … and its rendered .md tree (UUID-keyed)
│       └── …
├── slack/
│   ├── raw/
│   │   ├── entities.doltlite_db
│   │   └── blobs.doltlite_db
│   └── rendered_md/
├── fastmail/                       # (mbox source lands here too)
│   └── …
├── …
├── unified_index/                  # the shared indexes, rebuildable
│   ├── grid/db.doltlite_db         #   grid rows + markdowns + edges
│   └── qmd/index.sqlite            #   search index for hybrid / vector queries
└── system/                         # everything that isn't a source
    ├── dag_state.json              # scheduler state (which steps are up to date)
    ├── api-token                   # the running server's bearer token
    ├── feedback.doltlite_db        # feedback you filed (nothing regenerates it)
    ├── jobs.doltlite_db            # sync job queue + history
    └── job-logs/                   # one log per sync job
```

> **Backups:** the bulky **derived** artifacts — each `<name>/rendered_md/`
> tree, the search DB (`unified_index/grid/`), the qmd index (`unified_index/qmd/`),
> and served attachments (`system/media/`) — are all rebuildable from your raw
> stores, and each carries a `CACHEDIR.TAG`, so cache-aware backups skip them
> automatically:
>
> ```sh
> restic backup ~/datalib --exclude-caches        # or: borg create --exclude-caches
> tar --exclude-caches -czf datalib-backup.tgz ~/datalib
> ```
>
> What's left in the backup is exactly what you want to keep: the per-stanza
> `<name>/raw/` stores (your precious captured data), `config.toml`, and
> `system/` (scheduler state + sync job logs — operational
> history, not rebuildable).

A final per-step report prints when the run finishes, and a
machine-readable `run_summary` event lands on `datalib-dag`'s stderr
(NDJSON — tee stderr if you want to keep it). Exit code is non-zero if
any step failed.

## 5. Browse the result

If you synced from the app, you're already looking at the result —
`datalib-http` is the single-binary search backend with the web UI
embedded. If you ran `datalib-dag` from the terminal instead, start it
now from your data_root:

```sh
datalib-http ./
```

It binds to `http://127.0.0.1:8731` by default and opens that URL in
your default browser. Pass `--no-open` if you'd rather click in
yourself, and set `DATALIB_BIND=127.0.0.1:<port>` to override the
listen address.

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

Re-run the sync (**Sync now** in the app, or `datalib-dag config.toml`)
whenever you want to pull new conversations.
The downloader is incremental and the qmd index is content-hashed, so
re-runs against an unchanged corpus are relatively fast no-ops.

## 7. Querying the index directly with qmd

To find relevant markdown content, you can also query the search index directly from the command line by
pointing `qmd` at the sqlite file under your data root via the
`INDEX_PATH` env var:

```sh
INDEX_PATH=~/datalib/unified_index/qmd/index.sqlite \
    npx -y @tobilu/qmd query "hello"
```

Use `qmd status` against the same `INDEX_PATH` to confirm collections
and document counts.

## 8. Getting your data back out

The point of mirroring your data locally is that it stays yours, so it
is worth knowing the exit before you need it. Two of the three copies
are already in open formats you can read with no datalib at all:

- **The markdown.** `<name>/rendered_md/` is a tree of ordinary
  UTF-8 `.md` files, one per conversation or document. Copy it
  anywhere; every text editor and search tool on your machine already
  reads it.
- **The databases.** The `.doltlite_db` files are
  [doltlite](https://github.com/dolthub/doltlite) stores — SQLite's SQL
  engine over a versioned, content-addressed file format, which is what
  lets datalib show you what a source *changed or deleted* between
  syncs. The trade is that the file itself is not a SQLite file: point
  stock `sqlite3` at one and it says `file is not a database`.

  So export it. `datalib-doltlite` was installed alongside
  `datalib-dag` in step 1, and one pipe writes a plain SQLite database
  that every SQLite tool — `sqlite3`, Datasette, pandas, DB Browser,
  your language's stdlib — opens directly:

  ```sh
  datalib-doltlite -readonly ~/datalib/unified_index/grid/db.doltlite_db .dump \
    | sqlite3 ~/grid.sqlite

  sqlite3 ~/grid.sqlite "SELECT provider, count(*) FROM grid_rows GROUP BY 1;"
  ```

  The same command works on any `.doltlite_db` under your data root,
  including the raw per-source stores under `<name>/raw/`, whose
  attachment bytes come across intact.

  What the export gives you is the current state of every table, with
  its schema and indexes. What it leaves behind is the version history
  — the commit per sync that the diff-since-last-time answers come
  from. Keep the original if you want that; the export is a snapshot
  for other tools.

`datalib-doltlite` is a `sqlite3`-compatible shell, so you can also
just explore in place — `datalib-doltlite -readonly <file>` drops you
in a REPL. Pass `-readonly` whenever you are only looking: a second
writer against a live store can wedge your next sync. More recipes,
including the commit history and per-sync diffs, are in
[`docs/dev/doltlite.md`](/docs/dev/doltlite.md).
