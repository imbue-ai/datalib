# datalib — Project Data Liberation ✊

Liberate and own your data. Run powerful AI tools on it, on your terms.

datalib mirrors your personal data — chats, email, messages, contacts,
documents, photos — out of the services that hold it and into one place
you own: a folder on your own computer, in open formats, with history.
Once it is there you can search it, join it across sources, and point
whatever tools you like at it, without asking anyone's permission.

Think of it as the data warehouse and pipelines a big company runs over
its own data, in laptop-sized packaging. Hat tips to
[Perkeep](https://perkeep.org/) and
[Dogsheep](https://dogsheep.github.io/), which got there first.

datalib is an early Imbue project. It is read-only today: it brings
data in and never writes anything back to a source.

## Get started

Three ways in, from least to most hands-on:

1. **The desktop app** (macOS, Apple Silicon). Download the `.dmg` from
   the [latest release](https://github.com/imbue-ai/datalib/releases/latest).
   It asks which folder to keep your data in, then opens the app; you
   add your first source from the **Manage** screen.
2. **The command-line tools** (macOS or Linux). One `curl | sh` installs
   them. The [**first-time user guide**](docs/user/first_time_user.md)
   walks through install, credentials, the config file, and your first
   sync.
3. **Hand it to your agent.** Point an AI coding agent at this
   repository and ask it to set you up.
   [`docs/agent_user.md`](docs/agent_user.md) is written for an agent
   running datalib on your behalf; [`AGENTS.md`](AGENTS.md) is for one
   working on the code.

Cautious? The [Docker image](docs/user/docker.md) keeps the binaries
and your credentials inside a container that sees only the folders
you mount, and it comes with a demo library already loaded, so you can
look before you hand it anything of yours. Building from source is the
[first-time dev guide](docs/dev/first_time_dev.md).

## Read this before you point an agent at it

datalib is plain old software. Running a sync invokes no AI model and no
agent, and nothing leaves your machine: the only network traffic is
datalib reading from the services you configured, plus a one-time
download of the search models the first time the semantic index is
built.

What it produces, though, is a very valuable pile of private data in one
place — and most of it was written by other people. Three things follow:

- **Credentials.** Web sources authenticate through
  [latchkey](https://github.com/imbue-ai/latchkey), which keeps live
  session cookies and API tokens on your machine. Any process that can
  run commands as you can use them to act as you on those services, with
  no further prompt. Only do this on a machine you trust.
- **Agents.** Before you let an agent loose on the mirror, read about the
  [lethal trifecta](https://simonwillison.net/2025/Jun/16/the-lethal-trifecta/).
  Treat everything in your mirror as both *private* and *untrusted*
  content. And remember that an agentic harness sends what it reads to a
  model provider: ask yourself whether the people who wrote you those
  messages would be fine with that.
- **Terms of service.** The Claude.ai and ChatGPT sources talk to the
  same undocumented web APIs your browser does, using your own session.
  It is your data, but check the terms of the services you use, and know
  that those APIs can change without notice.

## Supported data sources

Every source links to its section of [**Getting your data**](docs/user/getting_your_data.md):
what it mirrors, and how to get at yours — a login kept by latchkey, an
export, or a backup pulled off a phone.

<table>
  <tr>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#airvisual"><img src="datalib/ui/src/assets/airvisual.svg" width="40" height="40" alt=""><br><b>AirVisual</b></a></td>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#apple-messages"><img src="datalib/ui/src/assets/apple_messages.svg" width="40" height="40" alt=""><br><b>Apple Messages</b></a></td>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#apple-photos"><img src="datalib/ui/src/assets/apple_photos.svg" width="40" height="40" alt=""><br><b>Apple Photos</b></a></td>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#beeper"><img src="datalib/ui/src/assets/beeper.png" width="40" height="40" alt=""><br><b>Beeper</b></a></td>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#chatgpt"><img src="datalib/ui/src/assets/chatgpt.svg" width="40" height="40" alt=""><br><b>ChatGPT</b></a></td>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#claude"><img src="datalib/ui/src/assets/claude.svg" width="40" height="40" alt=""><br><b>Claude</b></a></td>
  </tr>
  <tr>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#claude-code"><img src="datalib/ui/src/assets/claude_code.svg" width="40" height="40" alt=""><br><b>Claude Code</b></a></td>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#contacts"><img src="datalib/ui/src/assets/contacts.svg" width="40" height="40" alt=""><br><b>Contacts</b></a></td>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#email"><img src="datalib/ui/src/assets/email.svg" width="40" height="40" alt=""><br><b>Email</b></a></td>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#facebook"><img src="datalib/ui/src/assets/facebook.svg" width="40" height="40" alt=""><br><b>Facebook</b></a></td>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#garmin"><picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/garmin_dark.svg"><img src="datalib/ui/src/assets/garmin.svg" width="40" height="40" alt=""></picture><br><b>Garmin</b></a></td>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#github"><picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/github_dark.svg"><img src="datalib/ui/src/assets/github.svg" width="40" height="40" alt=""></picture><br><b>GitHub</b></a></td>
  </tr>
  <tr>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#gitlab"><img src="datalib/ui/src/assets/gitlab.svg" width="40" height="40" alt=""><br><b>GitLab</b></a></td>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#google-takeout"><img src="datalib/ui/src/assets/google_takeout.svg" width="40" height="40" alt=""><br><b>Google Takeout</b></a></td>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#lightroom"><img src="datalib/ui/src/assets/lightroom.svg" width="40" height="40" alt=""><br><b>Lightroom</b></a></td>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#linkedin"><img src="datalib/ui/src/assets/linkedin.svg" width="40" height="40" alt=""><br><b>LinkedIn</b></a></td>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#local-files"><img src="datalib/ui/src/assets/fsindex.svg" width="40" height="40" alt=""><br><b>Local files</b></a></td>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#media"><img src="datalib/ui/src/assets/media.svg" width="40" height="40" alt=""><br><b>Media</b></a></td>
  </tr>
  <tr>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#notion"><picture><source media="(prefers-color-scheme: dark)" srcset="docs/assets/notion_dark.svg"><img src="datalib/ui/src/assets/notion.svg" width="40" height="40" alt=""></picture><br><b>Notion</b></a></td>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#pdfs"><img src="datalib/ui/src/assets/pdf.svg" width="40" height="40" alt=""><br><b>PDFs</b></a></td>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#perseus"><img src="datalib/ui/src/assets/perseus.svg" width="40" height="40" alt=""><br><b>Perseus</b></a></td>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#signal"><img src="datalib/ui/src/assets/signal.svg" width="40" height="40" alt=""><br><b>Signal</b></a></td>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#slack"><img src="datalib/ui/src/assets/slack.svg" width="40" height="40" alt=""><br><b>Slack</b></a></td>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#sms-backup-and-restore"><img src="datalib/ui/src/assets/sms.svg" width="40" height="40" alt=""><br><b>SMS Backup &amp; Restore</b></a></td>
  </tr>
  <tr>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#whatsapp"><img src="datalib/ui/src/assets/whatsapp.svg" width="40" height="40" alt=""><br><b>WhatsApp</b></a></td>
    <td align="center" width="16%"><a href="docs/user/getting_your_data.md#yolink"><img src="datalib/ui/src/assets/yolink.png" width="40" height="40" alt=""><br><b>YoLink</b></a></td>
    <td align="center" width="16%"><a href="docs/dev/step_protocol.md"><img src="docs/assets/your_own_source.svg" width="40" height="40" alt=""><br><b>Your own source →</b></a></td>
    <td width="16%"></td>
    <td width="16%"></td>
    <td width="16%"></td>
  </tr>
</table>

A source's `type` names the thing being mirrored. *Where the data comes
from* is one table on the source's ingest step, named under the type:
`api` is the product's own API, `export` an unpacked export, `backup` a
phone backup, `fswalk` a folder to scan. Every shape, fully commented,
is in [`all_sources.toml`](docs/user/config_examples/all_sources.toml).

## How it works

Two layers.

**The lower layer is a small, unopinionated pipeline runner.** A data
store is a file in a folder. A data processor is a program, in whatever
language you like. `datalib-dag` arranges those programs into a graph
(a DAG), runs it, and re-runs only the steps whose inputs changed. Any
executable that speaks a small NDJSON protocol can be a step — see
[`docs/dev/step_protocol.md`](docs/dev/step_protocol.md).

**The upper layer is the batteries.** For each source above, an `ingest`
step that brings the raw data in and a `render_markdown` step that turns
it into readable markdown; then two shared index steps that fan in over
everything rendered — a SQL table of every message and document
(`grid_rows`) and a semantic search index (built with
[qmd](https://github.com/tobi/qmd)). A local web UI, also shipped as a
desktop app, searches and browses the result. The batteries are Rust;
the UI is Vue, wrapped in Tauri for the desktop app.

**The stores are [doltlite](https://github.com/dolthub/doltlite)**:
SQLite's engine over a versioned, content-addressed file format, so a
database file is also a git-shaped history of itself. Every sync is a
commit. That is what makes the pipeline incremental — each stage asks
"what changed since the commit I last read?" rather than rescanning —
and it is what keeps the record of what a source changed or deleted
between syncs.

**Deltas are things too.** If you can render a collection of things,
consider rendering the *difference* between two versions of that
collection — what was added, what was removed, what changed and how.
Code has had this for fifty years; almost nothing else does, and most of
the questions people bring to their own data are questions about change.
Two mechanisms carry it here:

- **Every store keeps its history.** A raw store's commits are the
  syncs; a render store's commits are the renders. `datalib-doltlite`
  reads either at any commit or diffs any two (`dolt_log`, `dolt_diff`),
  and the Manage screen shows a source's commit history with what each
  commit did to each table.
- **A comparison is a source of its own.** "Compare two syncs…" on a
  source makes a *diff group*: the source's own renderer run at both
  commits and subtracted, written as an ordinary source. Its documents
  carry the changes marked — added and removed sections on green and
  red, edited words inside a modified one — and its grid rows say
  `added`, `removed` or `modified` and which columns moved, so
  everything that works on a source works on the difference.

## What we are aiming for

Near term, ingest and understand:

- **Big tent** — popular and unpopular sources alike, discovering each
  one's schema rather than forcing it into ours.
- **Local-first** — file-based storage; views and processing work
  offline.
- **Incremental** — cheap to keep up to date.
- **Stable identity** — a message keeps its id through content edits, so
  links to it survive.
- **Versioned** — notice when the upstream loses or edits data. The
  history is in the stores, and a diff group renders what changed
  between two syncs as a source of its own.
- **Legible** — render raw data from many schemas into markdown.
- **Findable** — search by metadata, keywords, and vectors.
- **Read-only, for now** — ingest-only views of every source.

Longer term: all your data in one place instead of one app per data
type; your own apps that join data across sources; as much of your data
as possible in your own hands, in formats you can use; and publishing it
back into other apps.

## Getting your data out again

A mirror you can't leave is just another silo, so the exits are plain:

- **Markdown** — `<name>/render_markdown/` is ordinary `.md` files, one
  per conversation or document. Nothing to export.
- **SQL** — `datalib-doltlite` ships with the tools and is a `sqlite3`
  shell that understands the versioned format. One pipe writes a plain
  SQLite file for any tool that wants one:

  ```sh
  datalib-doltlite -readonly unified_index/grid_index/db.doltlite_db .dump | sqlite3 grid.sqlite
  ```

  Or browse a store in a GUI: a build of DB Browser for SQLite patched to
  open doltlite files is at
  <https://github.com/thadd3us/sqlitebrowser/releases> (macOS).

Details, and what a snapshot does and doesn't carry, in
[`docs/dev/doltlite.md`](docs/dev/doltlite.md).

## Documentation

- [**First-time user guide**](docs/user/first_time_user.md) — install
  the CLI and mirror your own data.
- [**Getting your data**](docs/user/getting_your_data.md) — per-source
  credentials and exports.
- [**Running in Docker**](docs/user/docker.md) — the sandboxed way in:
  the demo, your own exports, credentials in the container.
- [**Agent user guide**](docs/agent_user.md) — for AI agents operating
  datalib on a user's behalf: config, sync, querying, custom steps.
- [**First-time dev guide**](docs/dev/first_time_dev.md) — build and
  hack on datalib from source.
- [**Contributor runbook**](AGENTS.md) — for humans and AI agents
  working *on* datalib: the doc map, repo layout, testing rules, and
  conventions.

## License

[MIT](LICENSE). A release carries the notices of the third-party
software it bundles under `licenses/` (in the `.app`,
`Contents/Resources/licenses/`).
