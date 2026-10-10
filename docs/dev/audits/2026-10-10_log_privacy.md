# Audit: private data in the log store, 2026-10-10

A record, not reference. It reads the tree at `f39632e41` (`main`
after #1155) against the rule in
[`logging.md` § "What a line may carry"](../logging.md#what-a-line-may-carry),
which the same PR wrote down, for issue #978. Read the code before
repeating any line below as current: the next slice is meant to empty
this list.

**What the PR fixed.** The request log keeps a query string's keys and
only the values of keys we mint, and logs an app route as its cards'
names, so a search typed into the bar no longer reaches it
(`http/src/request_log.rs`). The run store writes the home directory
as `~` in every line and step error (`runs/src/redact.rs`). The
search applet's two "search failed" lines no longer carry the query.
`//datalib/backend/http:request_log_test` sends a search through the
server and fails if the typed text reaches the store; it failed before
the fix.

**How it was read.** Every `tracing` macro, `println!` and `eprintln!`
outside the tests in `datalib/backend` (473 and 82), each expanded
whole and searched for fields and interpolations that name a record's
content, a file, a URL, a person, an account or a query; then the
places lines enter the
store without a macro: the runner's capture of a step's pipes, the
gateway's relay of an applet's lines, and the page's events. Error
chains (`error = %format!("{e:#}")`, about 120 sites) are listed as one
class, not one by one. Every line below was checked against the code;
none was seen to leak in a real root, and none is quoted.

## Still carries private data

Ordered by how often it fires and how plainly private it is. Paths are
under `datalib/backend/` unless they start with `datalib/ui/`.

### What a person typed, outside the request log

- `datalib/ui/src/telemetry.ts:120` (`navigateEvent`), used at :186 —
  the `navigate` line's `msg`, `path` and `from` are the route, which is
  the column stack written as card code: a grid's search sits in it
  (`gridView({ q: "…" })`). Every route change.
- `datalib/ui/src/telemetry.ts:163` — `page_load.path`, the same route.
- `datalib/ui/src/components/ShadowCard.vue:173` — `card_open.source`,
  the card's whole source, arguments included.
- `datalib/ui/src/toasts.ts:43` — every toast's text is the line; a
  toast that quotes a record or a search carries it.
- `datalib/ui/src/telemetry.ts:97` (`errorEvent`) — an uncaught error's
  message and stack, which may quote what the page was showing.

The server's door for all five is `http/src/ui_events.rs:137`, where
one function builds every page line; the same card-name reduction the
request log uses would fit there.

### Record contents and people

- `contacts/src/lib.rs:174`, `:182`, `:184`, `:190`, `:205` — `from`,
  `to`, `holder`, `handle`: handles, which are email addresses and
  phone numbers.
- `etl/providers/chatgpt/src/ingest/mod.rs:451` — `email`, the signed-in
  account's address.
- `etl/providers/beeper/src/ingest/megabridge.rs:201` — `mxid`, a Matrix
  user id.
- `etl/providers/claude/src/ingest/mod.rs:368`, `:832` — `org`, an
  organisation's display name.
- `etl/providers/airvisual/src/ingest/parse.rs:135` (`text`, a whole
  line of the export) and `:175` (`value`, a raw cell).
- `etl/providers/whatsapp_render/src/render/parse.rs:196` — `?key`, a
  message's natural key (a chat's jid among it), also a `Debug`
  rendering; `:274` — `examples`, attachment file paths.
- `etl/providers/yolink/src/ingest/mod.rs:546`, `:638`, `:647` —
  `device`, the name a person gave a device.
- `etl/providers/slack/src/ingest/mod.rs:443`, `:1334`, `:1341` — a
  channel's name (a DM's is a person's).
- `etl/providers/email/src/ingest/gmail_api/mod.rs:255` (`labels`),
  `:506` (`label`), and `etl/providers/email_render/src/render/render.rs:210`
  (`unmatched`) — mailbox label names; `account` at
  `gmail_api/mod.rs:237`, `:292`, `:380`, `:395`, `:554` is the
  configured account id, usually an email address.
- `http/src/remote_media.rs:406`, `:476` — `url`, a remote image a
  document links to.
- `etl/src/progress.rs:238` — every progress message, at `trace` (the
  default level). Providers put names in them:
  `calendar/src/ingest/google.rs:93` and `caldav/mod.rs:102` (calendar
  name), `ics_dir.rs:79` and `contacts/src/ingest/vcf_dir.rs:101`
  (file path), `slack/src/ingest/mod.rs:428` (channel), `airvisual`
  `mod.rs:268` (device).

### File and folder names from a person's mirror

- `etl/files/src/fsscan.rs:576` (`path`), `:682` and `:699` (`path`
  and the file name in the message, at `info`, for every large file
  hashed).
- `etl/providers/media/src/ingest/mod.rs:153`, `:322`, `:404`;
  `meta.rs:120`, `:131`, `:137`; `payload/mod.rs:147` — a media file's
  path.
- `etl/providers/contacts/src/ingest/vcf_dir.rs:88`, `:230`;
  `calendar/src/ingest/ics_dir.rs:64`;
  `google_takeout/src/ingest/mod.rs:247` and `gemini_apps.rs:308`;
  `sms_backup_restore/src/ingest/mod.rs:160`;
  `linkedin/src/ingest/mod.rs:155`;
  `pdf_render/src/render/mod.rs:195`;
  `fsindex/src/ingest/mod.rs:653` — a file or folder's path or name.
- The request log's path, for two server routes that carry a name:
  `/applet/unified_index/asset/{uuid}/{*rel}` (`rel` is an
  attachment's file name) and `/api/lib/{name}` (a library a person
  named).

### Account-identifying URLs

- `etl/web/src/http.rs:584` — `url` of every request that is retried.
- `etl/src/events.rs:10` — `url` of every item fetched, at `debug`.
- `etl/web/src/dav/sync.rs:599`, `calendar/src/ingest/caldav/mod.rs:68`,
  `contacts/src/ingest/mod.rs:273` — DAV URLs, which name the account.

### Text we pass through without reading

- **A step's pipes**: `dag/src/runs_sink.rs:444` files every line a step
  prints, and `dag/src/subprocess.rs:378`–`411` makes its last lines the
  step's error (`runs_sink.rs:328`). Whatever a custom step prints lands
  as is.
- **Error chains**: `error = %format!("{e:#}")` and `"{e:#}"` in about
  120 lines; `datalib_step/src/main.rs:374` is the one every failed
  built-in step ends on. An `anyhow` context or an upstream error that
  quotes the value it failed on carries it.
- **Subprocess output**: `unified_index/src/qmd/daemon.rs:331` (qmd's
  stderr) and `:160`; `http/src/connect.rs:450`, `:656` and
  `http/src/probe.rs:189` (latchkey and the probe, scrubbed of secrets
  but not of account names).
- **Applet lines**: `http/src/applets.rs:1140` relays each one as the
  server's, so every applet line above arrives this way.

## Not decided

- **Source ids** (issue #978, item 5) are written on every line about a
  step and are often an account name. The rule treats them as fine for
  now.
- **A test over the fixture pipeline**: the request-log test covers one
  path. Seeding the TNG sources with a marker in every content field
  and failing `ingested_tng_test` when a marker reaches the store would
  catch the rest; most of the lines above would fail it.
