# datalib

Liberate your data from silos. datalib mirrors your personal data — chat
exports, email, messages, contacts, and more — into a single queryable local
store, so you can run SOTA AI and data tools on it, on your own terms.

A goal of the project is to get *all* of your data under one roof: every
account, export, and backup you care about, mirrored side by side in one
place you own, where it can finally be searched and cross-referenced as a
whole.

## Supported data sources

| Source | `type` | Input mode | What it mirrors |
|--------|--------|------------|-----------------|
| Claude.ai | `claude_api` | Web API (latchkey) | Conversations across every org |
| Claude export | `claude_export` | File on disk | An unpacked Claude data export |
| ChatGPT | `chatgpt_api` | Web API (latchkey) | Conversations |
| Slack | `slack_api` | Web API (latchkey) | Channels + file attachments |
| GitHub | `github_api` | Web API (latchkey) | Pull requests |
| GitLab | `gitlab_api` | Web API (latchkey) | Merge requests |
| Notion | `notion_api` | Web API (latchkey) | Pages (inbox + page subtrees) |
| Email | `email` | JMAP server (latchkey) **or** Google Takeout `.mbox` | Mail messages |
| Google Takeout | `google_takeout` | Export tree on disk | Google Chat + Voice messages (rendered to markdown); Maps reviews / saved places / photos, YouTube watch history + subscriptions, and Gemini Apps activity (extracted to the raw store, not yet rendered) |
| Contacts | `carddav` | CardDAV server (latchkey) **or** local `.vcf` files | Contacts |
| Beeper | `beeper` | Local Beeper Texts data dir | Signal, Google Chat, etc. |
| Perseus | `perseus` | Download | TEI editions from PerseusDL |
| YoLink | `yolink` | Web API | Per-device sensor CSV history, rendered as one page of interactive plots |
| Signal | `signal_backup` | Android backup file | Messages + media |
| WhatsApp | `whatsapp_backup` | Android `crypt15` backup | Messages + media |
| SMS Backup & Restore | `sms_backup_restore` | Android export dir on disk | SMS / MMS / calls (one chat per number) |
| LinkedIn | `linkedin` | "Get a copy of your data" export | Messages + connections as contacts |
| Local files | `fsindex` | Local directory tree | An index of every entry (path, kind, size, blake3) — download-only, no rendered markdown |
| Photos | `lightroom` | Adobe Lightroom Classic catalog (`.lrcat`) | A deduplicated, versioned mirror of every table — an incremental backup with full history; download-only, no rendered markdown |

See [`docs/user/config_examples/all_sources.toml`](docs/user/config_examples/all_sources.toml)
for one fully-commented step pair per source.

## Getting your data out again

A mirror you can't leave is just another silo, so the exits are plain:

- **Markdown** — `<name>/rendered_md/` is ordinary `.md` files, one per
  conversation or document. Nothing to export.
- **SQL** — the stores are
  [doltlite](https://github.com/dolthub/doltlite) databases (SQLite's
  engine over a versioned, content-addressed file format, which is what
  lets datalib tell you what a source deleted between syncs). The
  shell ships in the release tarball, and one pipe writes a plain
  SQLite file for any tool that wants one:

  ```sh
  datalib-doltlite -readonly unified_index/grid/db.doltlite_db .dump | sqlite3 grid.sqlite
  ```

  Details, and what a snapshot does and doesn't carry, in
  [`docs/dev/doltlite.md`](docs/dev/doltlite.md).

## Getting started

- [**First-time user guide**](docs/user/first_time_user.md) — download the
  CLI and mirror your own data.
- [**Agent user guide**](docs/agent_user.md) — for AI agents operating datalib
  on a user's behalf: config, sync, querying, custom steps.
- [**First-time dev guide**](docs/dev/first_time_dev.md) — build and hack on
  datalib from source.
- [**Contributor runbook**](AGENTS.md) — for humans and AI agents working *on*
  datalib: the doc map, repo layout, testing rules, and conventions.
