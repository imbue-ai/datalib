# The `google_takeout` source

An unpacked [Google Takeout](https://takeout.google.com) export, read
off `export.path` (the directory holding `Takeout/`); the feed switches
sit beside it in the same table. There is no API and no network: the
download step walks a directory tree the user exported and unzipped
themselves.

## A config turns each feed on; the form ticks them all

`SyncFlags` (`src/ingest/mod.rs`) is one boolean per feed, mirrored by
the `export` table in `google_takeout_config`, and `Default` sets every
one of them to `false`. A config that names no feed reads nothing, so a
feed is never read because a key was left out.

The "Add source" form ticks every box, spam included, and writes each
flag out, because an export holds what its owner asked Google for and
reading all of it is what they expect; unticking is how to leave a
product out. `google_voice_include_spam` stays a second switch under
`google_voice`: spam is bulky and rarely worth searching.

`tests/fixture_walk.rs`'s `sync_flags_default_disables_everything` is
the regression test for the config side: it runs the walk with a
default `SyncFlags` and asserts that the Maps, YouTube, Chat and Gemini
feeds land nothing. `datalib/ui/tests/export_wizards.test.ts` pins the
form's.

## The feeds and what they write

| flag | raw tables |
|---|---|
| `maps_reviews` | `maps_reviews` |
| `maps_saved_places` | `maps_saved_places` |
| `maps_photos` | `maps_photos` (bytes in the blob CAS) |
| `youtube_watch_history` | `youtube_watch_history` |
| `youtube_subscriptions` | `youtube_subscriptions` |
| `google_chat` | `chat_groups`, `chat_users`, `chat_messages`, `chat_attachments` |
| `gemini_apps` | `gemini_activity`, `gemini_attachments` |
| `google_voice` | `voice_messages`, `voice_bills`, `voice_greetings`, `voice_attachments` |

`DATA_TABLES` and `EDGE_TABLES` in `src/ingest/schema_raw.rs` list the
tables the store is created with. Google Voice keeps its own
`schema_raw` under `src/ingest/google_voice/`.

Ids are uuidv5 under a per-provider namespace, minted from a recipe
string (`"maps_review:{ftid}:{date}"`, `"youtube:watch:{id}:{ts}"`);
see `ns_id` in `schema_raw.rs` and
[`docs/dev/entity_ids.md`](../../../../../docs/dev/entity_ids.md).

## A feed that fails costs only itself

`fetch` runs each feed through `RunProblems::run_phase`, which turns an
error or a panic into a `phase:<feed>` row in `problems` and goes on to
the next feed.
The export is someone else's HTML and JSON, so one odd entry must not
stop nine products from syncing; the row is what puts the failure on
the Manage screen. `feeds_failed` in the step summary counts them. A
feed that failed stamped nothing, so the next run tries it again and the
row goes with the run that succeeds.

## When part of a feed fails

A feed reads only the files that changed since it last stamped them
(`file_checkpoint`), so what failed has to be either read again next run
or stamped with its problem. Each kind is a `problems` row:

- **An entry read but not stored** — a review with no place id or
  date, a saved place with no date or nothing to key it on (a place id,
  a `cid`, or for a pin dropped on an address, its `q=` query), a
  subscription row short of columns or with no channel id, a
  watch-history entry that is not a video, a Gemini entry with no date
  line — is a `skipped:<feed>:<hash>` row (`RunProblems::skipped`).
  A feed replaces only its own rows, and only when it read its file, so
  an unchanged file keeps last run's rows. A Maps file with no
  `features` list is one `file:google_takeout/<feed>:<path>` row,
  stamped with the file.
- **A file that will not read or parse** — a Chat `user_info.json`,
  `group_info.json` or `messages.json`, a Maps photo sidecar, a Voice
  record, `Bills.html` or a greeting — is a `skipped:<feed>:<hash>` row
  and is left unstamped, so the next run reads it again. The rest of the
  feed lands. So is a file that reads as nothing: a `messages.json`
  listing entries none of which has a `message_id`, a Voice thread with
  no message, a call record with no time, a `Bills.html` with no table
  header. It deletes nothing, where a `messages.json` whose list is
  empty, or a bills table with no rows, empties what it holds.
- **A Chat or Gemini attachment** that is not in the export is a
  `not_found` warning on its edge (`chat_attachments:<message>#<name>`,
  `gemini_attachments:<activity>#<name>`), and one that is there but
  will not read an error. The file naming it is stamped, so each run
  looks again for every edge with no bytes, and the row goes when the
  bytes land.
- **A Maps photo whose media is missing or unreadable** lands its row
  without bytes and a `maps_photos:<stem>` problem, and its sidecar is
  left unstamped so the next run looks for the media again.
- **A Voice attachment** that is missing or unreadable is a
  `skipped:google_voice:<hash>` row on the file naming it, which is
  left unstamped: a Voice message names only the attachments it read, so
  reading the file again is the only way to add one.
- **A file that will not open** is `record:files:<path>`. It is there,
  so a Maps photo whose sidecar will not open keeps its row.
- **Deletions are held back** when a walk had errors (`listing:files`)
  or, for Voice, when files were removed or rewritten but one could not
  be read or would not open (`listing:removed_records`). A rewritten Voice file keeps its
  old stamp on such a run, so the next one still reads every file and
  deletes what the rewrite dropped.

Attachments are flushed before the files naming them are stamped, so a
flush that fails leaves the files to be read again. Chat and Voice
delete what a re-read or gone file no longer holds in the transaction
that stamps the files, so a run that fails before its deletions leaves
the files to be read again too. A run that was
stopped writes a `phase:` or `listing:` row only for what failed before
the stop, and clears none.

## Files are named the way Google names them

What Google writes differs from what one would guess, and the fixture
follows Google:

- A Maps photo's sidecar is named after its media, extension and all:
  `<photo>.jpg.json` describes `<photo>.jpg`.
- A Gemini entry's links are percent-encoded (`Prime%20Directive.pdf`), and
  a generated image can be on disk under another extension than the
  page names it by (`….jpeg` on the page, `….png` on disk). Its cell
  is `Prompted <prompt>`, then optional `N generated image.` and
  `Attached N file.` lines, the date, and the response's HTML; the
  right-hand cell holds a preview `<img>` of an attached image.

## A reader that changes has to reach stores already synced

A feed reads only the files that changed since it stamped them, so a
fix to what a reader makes of an unchanged file reaches nobody's store
on its own. It needs a rung on `schema_raw::LADDER` that forgets that
feed's stamps (`read_again`); the next sync then reads those files
whole. `tests/fixture_walk.rs`'s `rung_1_…` is the test to copy.

## What renders

`datalib_etl_google_takeout_render` draws every feed through
chat-common. A Google Chat space or a Voice conversation is a page per
month; Gemini, YouTube history and Google Maps are each one activity
feed (`google_takeout_render/src/feeds.rs`), a page per year, and YouTube
subscriptions, which have no dates, one page. A Gemini entry is two
items, the prompt by the account and the response by Gemini, its HTML
turned to markdown; attached files ride on the prompt and generated
images on the response. Voice bills and greetings are not rendered.

## Tests

`tests/fixture_walk.rs` points the downloader at the checked-in
TNG-themed Takeout tree (`tests/fixtures/Takeout/`) and asserts, per
Maps, YouTube, Chat and Gemini feed, the rows that land — counts, a
sample of the parsed content, the CAS digest for the photo feed — and
that a second walk over an unchanged tree ingests nothing. Google
Voice's parser and walk are tested in `src/ingest/google_voice/`.
