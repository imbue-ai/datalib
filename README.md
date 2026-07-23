# datalib

Liberate your data from silos. datalib mirrors your personal data — chat
exports, email, messages, contacts, and more — into a single queryable local
store, so you can run SOTA AI and data tools on it, on your own terms.

> Codenames in this project (`frankweiler`, etc.) are inspired by
> [_From the Mixed-Up Files of Mrs. Basil E. Frankweiler_](https://en.wikipedia.org/wiki/From_the_Mixed-Up_Files_of_Mrs._Basil_E._Frankweiler).

## Supported data sources

| Source | `type` | Input mode | What it mirrors |
|--------|--------|------------|-----------------|
| Claude.ai | `claude_api` | Web API (latchkey) | Conversations across every org |
| Claude export | `claude_export` | File on disk | An unpacked Claude data export |
| ChatGPT | `chatgpt_api` | Web API (latchkey) | Conversations |
| Slack | `slack_api` | Web API (latchkey) | Channels, opt-in DMs + file attachments |
| GitHub | `github_api` | Web API (latchkey) | Pull requests |
| GitLab | `gitlab_api` | Web API (latchkey) | Merge requests |
| Notion | `notion_api` | Web API (latchkey) | Pages (inbox + page subtrees) |
| Email | `email` | JMAP server (latchkey) **or** Google Takeout `.mbox` | Mail messages |
| Google Takeout | `google_takeout` | Export tree on disk | Google Chat + Voice messages (rendered to markdown); Maps reviews / saved places / photos, YouTube watch history + subscriptions, and Gemini Apps activity (extracted to the raw store, not yet rendered) |
| Contacts | `carddav` | CardDAV server (latchkey) **or** local `.vcf` files | Contacts |
| Beeper | `beeper` | Local Beeper Texts data dir | Signal, Google Chat, etc. |
| Perseus | `perseus` | Download | TEI editions from PerseusDL |
| YoLink | `yolink` | Web API | Per-device sensor CSV history |
| Signal | `signal_backup` | Android backup file | Messages + media |
| WhatsApp | `whatsapp_backup` | Android `crypt15` backup | Messages + media |
| SMS Backup & Restore | `sms_backup_restore` | Android export dir on disk | SMS / MMS / calls (one chat per number) |
| LinkedIn | `linkedin` | "Get a copy of your data" export | Messages + connections as contacts |
| Local files | `fsindex` | Local directory tree | An index of every entry (path, kind, size, blake3) — download-only, no rendered markdown |

See [`docs/user/config_examples/all_sources.yaml`](docs/user/config_examples/all_sources.yaml)
for one fully-commented step pair per source.

## Getting started

- [**First-time user guide**](docs/user/first_time_user.md) — download the
  CLI and mirror your own data.
- [**Agent user guide**](docs/agent_user.md) — for AI agents operating datalib
  on a user's behalf: config, sync, querying, custom steps.
- [**First-time dev guide**](docs/dev/first_time_dev.md) — build and hack on
  datalib from source.
