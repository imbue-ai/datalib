# Audit: what a sync drops without saying so, 2026-11-05

A record, not reference. The rule, from Thad: anything dropped leaves a trail
that it is absent, without a litany of reasons. A trail is a row in `problems`,
since a log line or a count in a summary string reaches nobody. This read looked
at `etl/providers/`, `etl/sqlite_mirror/`, `etl/agent_sessions/`,
`etl/forge-ingest-common/` and `etl/chat-common/` for what breaks that rule.

**How far to trust it.** Every provider was scanned for patterns; none was read
line by line. Each line under "Still open" that matters on real data was opened
and read, but none was checked against a real export, and those marked (upstream)
rest on what we know of the service's format. Which fields a parser never looks
at was checked only for Slack, Signal, email, SMS, ChatGPT, Beeper and Apple
Messages.

## Fixed in this change

| what was dropped | where it was | now |
|---|---|---|
| a file over `max_bytes` | `file_changes_scan` | `record:files:<path>` warning per file |
| a PDF with no text to read | `pdf_documents` only | one `skipped:documents_without_text:` warning with a count |
| a file evicted to the cloud | media's summary count | one `skipped:cloud_placeholders:` warning with a count |
| a playlist over 16 MiB, whose stored row also went | nothing | keeps its row, `record:media_playlists:<path>` warning |
| a device, item, activity or weigh-in listed with no id | Garmin's log, or nothing | `skipped:<listing>:` row |
| a YouTube history entry with no link | nothing | one `skipped:youtube_watch_history:` row with a count |
| an AirVisual column nothing stores | the log | the file's row |
| an email attached to an email | nothing | listed and linked among the email's attachments, like any other |
| a Slack message's cards, link previews and forwards | nothing | an `info` row on the document, on its banner |
| what a Signal backup holds and no page shows: stickers, chat updates, reactions, quotes, previews, the rest of a long message | nothing | one `skipped:not_shown:` warning, by kind. Stored, but not shown or searchable; rendering them is still to do |
| a chat on an account no bridge here reads | nothing | a row per kind of account; `info` for Beeper's own (`hungryserv`, `$space`), a warning for a bridge the list does not know |
| an MMS part with no data | nothing | one warning a backup file, with the count |
| LinkedIn and Facebook tables no feed reads | nothing | one `unrendered_tables:all` warning a source |
| Google Drive files that are not Docs | nothing | one `unrendered_files:all` warning with a count by kind |

## Measured

937 PDFs from a mac's system folders, 841 documents, synced with the built
binaries. 659 render nothing (647 `scanned`, 9 `image_based`, 3 with fonts that
do not decode), most of them icons and glyphs. That is one warning, and the 182
that render agree with the render store's documents. A second sync committed
nothing. A row per document would have been 659 warnings on the Manage row.

Slack, from the store of a real workspace, counts only: nearly every message's
`blocks` are `rich_text` repeating its `text` (37,586 of them over 40,346
messages), so those are not counted. Messages with cards, previews or forwards
are 4,704 of those, on 4,360 of 15,300 documents; in a second workspace 20,052 of
211,424, on 17,033 of 63,618 documents. As warnings that would be 17,000 on one
Manage row, so the row is `info`, and shows on the document's banner.

Beeper, from a real `index.db`, counts and account kinds only: `slackgo`,
`googlechat`, `local-signal`, and `hungryserv` and `$space` whose chats hold
messages (3 chats). `$other` holds threads and no messages, so it is not reported.

Google Drive, real, counts only: 304 Docs, 44 decks, 41 Sheets.

14 of a mac's system sounds with `max_bytes = "200 KB"`: 13 warnings, none after
the limit rose, and 14 files stored.

## Still open: drops that reach real data

Each needs a decision or more than one site, so none was fixed here.

| where | what is dropped | what it needs |
|---|---|---|
| `apple_messages_render/src/render.rs:288`, `:249`, `:447` | a message in no chat with its attachments, and a tapback whose target is not held; a `debug!` at most (upstream: Recently Deleted leaves such rows) | a report path, which this crate lacks |
| `apple_messages_render/src/render.rs:263-273` | attachment bytes: only `chat.db` is mirrored, so each shows a name and a local path | mirror the attachments |
| `chatgpt_render/src/render/parse.rs:431`, `chatgpt/src/ingest/mod.rs:727` | voice audio parts, neither fetched nor shown, though the comment at `parse.rs:427` says they are attachments (upstream) | fetch them, or a row |
| `slack/src/ingest/api.rs:185-188` | a file with no URL, `hidden_by_limit` on a free plan (upstream) | a row at `render.rs:502` |
| `claude_code/src/ingest/parse.rs:158-171` | line types other than the ones it reads; only a count | decide; a row for every known type would be noise |
| `claude_render/src/render/render.rs:287`, `:333`; `claude_code_render/src/render.rs:502`; `codex_render/src/render.rs:464` | a content block of a type added upstream later | a row, as `Reason::UnsupportedJson` |
| `google_takeout/src/ingest/google_voice/parse.rs:284`, `:121`, `:375`, `:435` | `Err(_) => break` loses the rest of a file (a malformed one) | a row |
| `google_drive/src/ingest/mod.rs:262` | an old revision with no export link for a configured format | a row |
| `calendar/src/ical.rs:91`, `calendar/src/ingest/caldav/mod.rs:268` | to-dos and journal entries, and to-do-only calendars | decide; arguably out of scope for a calendar |
| `etl/sqlite_mirror/src/mirror.rs:463` | a virtual table whose module this build lacks; only a count | a row through `run_or_report`; beeper's bridges and lightroom also call `run`, so every caller needs it, and no test stages such a table yet |

Left as they are, on purpose: `payload_max_bytes` (`media/src/ingest/mod.rs:235`)
leaves a hash empty and no record goes; an HLS manifest (`:456`) is not something
a person made; `dropped_columns` in `sqlite_mirror` are configured, or derived or
instance-local columns.

## Still open: only on a malformed answer or file

A record with no id, and such. Upstream has not been seen to send these; each is a
few lines once a `found` handle reaches it.

`yolink/src/ingest/mod.rs:108`; `calendar/src/ingest/google.rs:327`;
`chatgpt/src/ingest/mod.rs:420`, `:687`; `chatgpt_render/src/render/parse.rs:260`;
`claude/src/ingest/mod.rs:437`, `:510`, `:557`, `:723`, `:816`, `:913`, `:1313`,
`db.rs:191`, `export.rs:268`, `:339`; `claude_render/src/render/parse.rs:544`,
`:652`; `slack/src/ingest/db.rs:267`, `:655`, `:1041`, `mod.rs:1528`;
`notion/src/ingest/listing.rs:165`, `:167`, `:587`, `walk.rs:254`,
`fetchers.rs:654`; `linear/src/ingest/db.rs:107`; `todoist/src/ingest/db.rs:103`;
`email/src/ingest/gmail_api/api.rs:576`; `email_render/src/render/parse.rs:278`,
`render.rs:837`; `google_drive/src/ingest/mod.rs:161`, `:811`;
`github/src/ingest/mod.rs:182`; `gitlab/src/ingest/mod.rs:161`;
`facebook_render/src/albums.rs:59`, `activity.rs:233`, `posts.rs:135`;
`google_takeout/src/ingest/google_chat.rs:182`;
`etl/agent_sessions/src/lib.rs:282`.
