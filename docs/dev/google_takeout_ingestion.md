# Google Takeout ingestion — design (draft)

**Status:** draft, iterating with thad.
**Scope:** raw-extract only. No translate, no `GridRow`s, no
`rendered_md/` sidecars in this first pass. Wire-tape JSONL is **not**
emitted (the data didn't come off a wire).

## Motivation

Google Takeout is a single, free, comprehensive export of everything
Google holds about a user. It's already on disk, for example at
`~/backups/Takeout/`. We want to ingest the slices that aren't already
covered:

  - Maps reviews / saved places / photos
  - YouTube watch history + subscriptions
  - Google Chat (DMs + bot conversations + attachments)
  - Gemini Apps chat history (from "My Activity")

Email is **out of scope** — the existing mbox extractor
([`providers/email/src/extract/mbox.rs`](../../frankweiler/backend/etl/providers/email/src/extract/mbox.rs))
already handles Takeout-exported Gmail.

## Shape of the provider

One new provider crate at
[`frankweiler/backend/etl/providers/google_takeout/`](../../frankweiler/backend/etl/providers/),
named `frankweiler-etl-google-takeout`. Local-file ingestion, no API,
no auth.

Follows the schema-first / shared-bulk-helpers conventions the
architecture doc spells out under
[Bulk-upsert as the standard write path](data_architecture_ingestion.md#bulk-upsert-as-the-standard-write-path)
and
[One writer per row](data_architecture_ingestion.md#one-writer-per-row-load-bearing-rule):
row structs and DDL constants live in `extract/schema_raw.rs`,
deriving
[`WirePayloadRow`](../../frankweiler/backend/etl/macros/src/lib.rs)
and
[`CasEdgeRow`](../../frankweiler/backend/etl/src/blob_cas.rs) where
applicable; all bulk writes go through
[`frankweiler_etl::bulk`](../../frankweiler/backend/etl/src/bulk.rs)
and
[`flush_cas_edges`](../../frankweiler/backend/etl/src/blob_cas.rs).
There is **no provider-side bulk SQL**.

This work also lands a new shared module
[`frankweiler_etl::file_checkpoint`](../../frankweiler/backend/etl/src/file_checkpoint.rs)
that owns the `(scope, path, size_bytes, mtime_ns)` skip-cursor
pattern the mbox extractor pioneered. Takeout's seven file-driven
feeds were the trigger; mbox can migrate onto it as a follow-up.

However we do want a resume cursor based one file properties, so that we don't re-process the same files over and over again.
Size and mtime are fine.
We should probably build some general utilities to do file-based cursors because this pattern comes up in other places, like Signal and WhatsApp and Beeper, I think. 

The provider conceptually bundles several mostly-unrelated sub-feeds
into one configurable importer. Each sub-feed is opt-in via a boolean
in the YAML `sync:` block:

```yaml
sources:
  - type: google_takeout
    name: my_takeout
    input_path: ~/backups/Takeout
    sync:
      maps_reviews: true
      maps_saved_places: true
      maps_photos: true
      youtube_watch_history: true
      youtube_subscriptions: true
      google_chat: true
      gemini_apps: true
```

Each `true` enables one walker module. Defaults are all `false` so a
fresh user has to opt-in feed-by-feed.

### Why one provider, not five

The Maps / YouTube / Chat / Gemini slices share zero upstream schema
but share *every* operational concern:

  - same input root (`Takeout/`)
  - same auth (none)
  - same check the mtime and size before processing files.
  - same lack of wire-tape
  - all of these Google Takeout sources ingest into the same single raw doltlite db on disk

Splitting them into five `frankweiler-etl-takeout-*` crates would
multiply the wiring (workspace `Cargo.toml`, `MODULE.bazel`,
`SourceConfig` variants, sync dispatch) without separating any logic
worth separating. The walkers themselves are independent modules
inside one crate, so a problem in `youtube_watch_history` doesn't
disturb the maps tables.

## Layout on disk

```
<data_root>/raw/<name>.doltlite_db              # all entity tables
<data_root>/raw/<name>.blobs.doltlite_db        # CAS for attached bytes
```

Same shape as every other provider. `<name>` is the source's `name:`
from YAML.

## Tables

### Maps

| Table                    | PK recipe                                                                              | `when_ts`                  | Source file                                  |
|--------------------------|----------------------------------------------------------------------------------------|----------------------------|----------------------------------------------|
| `maps_reviews`           | uuidv5(NS, `maps_review:{ftid}:{date}`); `ftid` = the hex id after `1s` in the URL     | `properties.date`          | `Maps (your places)/Reviews.json`            |
| `maps_saved_places`      | uuidv5(NS, `maps_saved:{ftid_or_cid}:{date}`)                                          | `properties.date`          | `Maps (your places)/Saved Places.json`       |
| `maps_photos`            | file-stem (`2026-06-04-af8bb6e0`)                                                      | `creationTime.timestamp`   | `Maps/Photos and videos/*.json` + `*.jpg`    |

Each row stores the source JSON object verbatim in a `payload` column.
The JSON files are GeoJSON `FeatureCollection`s; we iterate `features[]`
and one feature → one row.

**Why uuidv5 for reviews / saved places.** The upstream JSON has no
explicit primary key. The `google_maps_url` embeds an ftid (the hex
after `!1s`) that's stable per *place*, but a user can review the same
place twice. `(ftid, date)` is the smallest natural key; we hash it
into a uuidv5 so the row id stays a fixed-width opaque string.

**Maps subdirectories with empty test data.** `Added dishes, products,
activities`, `Electric vehicle settings`, `Vehicle profiles`,
`Questions and Answers`, `Requests for services`, `Your local followed
places` — all empty in thad's takeout. **Not in scope for first
pass.** When real data shows up, add a table + walker.

### YouTube

| Table                      | PK                                                                       | `when_ts`                  | Source file                                  |
|----------------------------|--------------------------------------------------------------------------|----------------------------|----------------------------------------------|
| `youtube_watch_history`    | uuidv5(NS, `youtube:watch:{video_id}:{iso_ts}`)                          | parsed from MDL HTML       | `YouTube and YouTube Music/history/watch-history.html` |
| `youtube_subscriptions`    | `Channel Id` from the CSV                                                | null (not event-shaped)    | `YouTube and YouTube Music/subscriptions/subscriptions.csv` |

`youtube_watch_history.payload` carries the extracted fields as JSON:
`video_url`, `video_id`, `video_title`, `channel_url`, `channel_id`,
`channel_title`, raw `when_str`. Original HTML is not retained per-row
(it's the same `MyActivity` boilerplate for every entry).

### Google Chat

| Table               | PK                                                                  | `when_ts`                  | Source                                                |
|---------------------|---------------------------------------------------------------------|----------------------------|-------------------------------------------------------|
| `chat_groups`       | takeout dir name (`DM 2ZwEI8AAAAE`)                                 | null                       | `Google Chat/Groups/<dir>/group_info.json`            |
| `chat_users`        | takeout dir name (`User 101328725412981032774`)                     | null                       | `Google Chat/Users/<dir>/user_info.json`              |
| `chat_messages`     | `message_id` verbatim (`{group}/{topic}/{msg}` — globally unique)   | parsed `created_date`      | `Google Chat/Groups/<dir>/messages.json`              |
| `chat_attachments`  | `(message_id, export_name)`                                         | inherits parent message    | `Google Chat/Groups/<dir>/{export_name}`              |

Attached file bytes go in the sibling CAS db (`cas_objects` keyed by
blake3). The `chat_attachments` table itself is a
[per-provider CAS edge table](data_architecture_ingestion.md#per-provider-cas-edge-tables):
each row carries
`(message_id, export_name, blake3, content_type, byte_len, …)` and
implements the [`CasEdgeRow`](../../frankweiler/backend/etl/src/blob_cas.rs)
trait so it flushes through the
[shared attachment-flush primitives](data_architecture_ingestion.md#shared-attachment-flush-primitives)
the other providers use.

`unsentmessages.json` per user — empty in test data; populated case
treated as messages with a `kind = "unsent"` discriminator, same PK
shape if `message_id` is present, uuidv5 fallback otherwise.

**DM display name — yes, we have what we need to derive it later.**
A 2-person DM is named by an opaque id in the directory tree
(`DM 2ZwEI8AAAAE`). We leave the row keyed by that id at extract time
and defer "show as the peer's name" to translate. The information
translate needs is already in the raw store after extract:

  - `chat_groups.payload` holds the full `group_info.json`, including
    the `members[]` list with `{name, email, user_type}` per member.
  - `chat_users` (seeded from `Users/User <id>/user_info.json`) holds
    the self identity (`user.email`).

Translate picks the non-self member by comparing emails and uses that
name as the DM's display name. Group chats with >2 members keep the
opaque id and translate renders a comma-separated participant list.

### Gemini Apps

| Table              | PK                                                                            | `when_ts`            | Source                                              |
|--------------------|-------------------------------------------------------------------------------|----------------------|-----------------------------------------------------|
| `gemini_activity`  | uuidv5(NS, `gemini:{blake3_hex(prompt + "\0" + when_str)}`)                   | parsed MDL timestamp | `My Activity/Gemini Apps/MyActivity.html`           |
| `gemini_attachments`| `(activity_id, filename)`                                                    | inherits parent      | `My Activity/Gemini Apps/<filename>`                |

The `Gemini/` top-level directory holds two HTML files
(`gemini_gems_data.html`, `gemini_scheduled_actions_data.html`) that
are 11-byte empty stubs in thad's takeout. Skipped until a populated
example shows up.

The actual chat history lives at `My Activity/Gemini Apps/MyActivity.html`.
Same MDL grid layout as YouTube's watch-history but with prompt text
+ response markdown + attached file references inline in the cell.
**Attached files are sibling jpgs/pngs/pdfs in the same directory**,
named like `Hello World-4294095041467489.pdf` (URL-escaped link from
the HTML cell). Bytes land in `cas_objects`; the `gemini_attachments`
table is the per-provider CAS edge (same `CasEdgeRow` shape as
`chat_attachments`).

`payload` carries the parsed fields: `prompt_text`, `response_html`
(verbatim), `attached_files: [{name, href}]`, raw `when_str`.

**Why hash the prompt text instead of using a Google id.** The MDL
HTML has no machine id per entry. The cell timestamp + prompt text is
the smallest natural key. blake3 of `prompt + "\0" + when_str` is
overkill but stable across re-exports. blake3 (not sha256) for
consistency with the rest of the codebase — `frankweiler_etl::blob_cas::blake3_hex`
is the same helper the email mbox extractor uses for `.eml` blob ids,
which switched off sha256 because it was a profile hotspot on Apple
Silicon (the `sha2` crate has no ARMv8 hardware acceleration). The
NUL byte between the two fields is a cheap "no ambiguous join" guard
in case a prompt ever ends with characters that look like a timestamp
prefix.

## Bulk-write idiom (shared helpers)

Every write goes through the shared
[`frankweiler_etl::bulk`](../../frankweiler/backend/etl/src/bulk.rs)
chokepoint — the doctrine is spelled out under
[Bulk-upsert as the standard write path](data_architecture_ingestion.md#bulk-upsert-as-the-standard-write-path).
**No hand-rolled bulk SQL in this provider.**

For each entity table the walker:

  1. Declares its row struct in `extract/schema_raw.rs` next to the
     table's DDL constant, deriving
     [`WirePayloadRow`](../../frankweiler/backend/etl/macros/src/lib.rs)
     for the common `(id, payload, …extra columns)` shape. The derive
     macro emits the `BulkUpsertable` impl; no provider-side SQL.
  2. Per-walker `Accumulator` collects rows in memory.
  3. At `FLUSH_BATCH = 2000` rows, hands the `Vec<Row>` to
     [`frankweiler_etl::bulk::bulk_upsert_in_tx`](../../frankweiler/backend/etl/src/bulk.rs),
     which writes chunked multi-row UPSERTs and bumps the paired
     `<table>_bookkeeping` sidecar in one entity-pool transaction.
  4. For CAS-edge tables (`maps_photos`, `chat_attachments`,
     `gemini_attachments`), the per-row struct implements
     [`CasEdgeRow`](../../frankweiler/backend/etl/src/blob_cas.rs) and
     flushes through
     [`CasEdgeAccumulator`](../../frankweiler/backend/etl/src/blob_cas.rs)
     +
     [`flush_cas_edges`](../../frankweiler/backend/etl/src/blob_cas.rs),
     which writes one entity-pool tx for the edge rows + bookkeeping
     and one CAS-pool tx for the `cas_objects` bytes.

Everything that used to be in the mbox extractor's
hand-rolled `PendingBatch` / `push_placeholders` / per-table bulk
INSERT helpers now lives in the shared crate. This provider is just
row structs + walker loops + accumulator instances.

## Wire fidelity

Per the
[ingestion-doc rule](data_architecture_ingestion.md#wire-fidelity-of-the-raw-store),
`payload` holds the upstream JSON object verbatim (`jsonb(?)` on
write, `json(payload)` on read).

  - GeoJSON walkers: per-`feature` object, no normalization.
  - Chat walkers: per-message object from `messages.json`, no
    normalization.
  - HTML walkers (YouTube, Gemini): the HTML cell *is* itself a
    rendering. Google took some internal activity record, formatted it
    into MDL-styled HTML with inlined CSS, and handed us the result.
    Parsing the cell back into a JSON object isn't a deviation from
    wire-fidelity — it's the closest thing to the pre-render
    structured form we can recover. The HTML is *downstream* of the
    truth; our `payload` is upstream of the HTML. We extract every
    field that isn't template chrome: prompt/response, anchors,
    timestamp, `Products:` discriminator, `Why is this here?`
    settings provenance string. Nothing semantic in the cell gets
    dropped.

    Concretely, the YouTube watch-history file is ~98% inlined
    Material Design Lite CSS (one ~140KB `<style>` block stapled to
    every Takeout HTML export) and ~2% actual data: each entry is ~1
    KB of HTML around 5 strings. Retaining the per-row div would mean
    persisting the same MDL CSS class boilerplate verbatim on every
    row to no benefit. The full upstream file still lives on disk at
    `input_path` for anyone who wants to see the rendered form.

## Timestamps

Routed through
[`frankweiler-time`](../../frankweiler/backend/time/) per the
[doc's rule](data_architecture_ingestion.md#time-and-ordering-discipline).
Two parsers we need to add (in `provider::time`):

  1. **Google Chat long-form English**:
     `"Tuesday, February 11, 2025 at 11:33:35 AM UTC"`
  2. **Takeout MDL grid timestamp** (YouTube + Gemini):
     `"Jun 4, 2026, 11:48:37 AM PDT"`

Both carry the timezone as an abbreviation. We hard-code the
North-American abbreviations Google emits (UTC, PST, PDT, EST, EDT,
CST, CDT, MST, MDT, AKST, AKDT, HST) → fixed offset and refuse to
guess on anything else. A parse failure surfaces as a per-row warning
and leaves `when_ts = NULL` (per the doc's "no fabricated timestamps"
rule).

`youtube_subscriptions`, `chat_groups`, `chat_users` are not
event-shaped and carry `when_ts = NULL` always.

## Identity / Ship-of-Theseus

Per the
[doc's rule](data_architecture_ingestion.md#object-identity-ship-of-theseus-on-uuids):

  - Where Google gives us a stable id (Chat `message_id`, YouTube
    `Channel Id`, photo file-stem) we use it verbatim.
  - Where it doesn't (reviews, saved places, watch history, Gemini
    entries) we synthesize a uuidv5 from the most stable available
    fields. Recipes documented per table above.

A per-provider namespace constant lives in `extract/schema_raw.rs`:

```rust
pub const GOOGLE_TAKEOUT_NS: uuid::Uuid =
    uuid::Uuid::from_bytes([...]);  // generate once with uuidv5(DNS, "google-takeout.frankweiler")
```

## Resume / monitoring

  - **Resume**: shared `(path, size_bytes, mtime_ns)` file checkpoint
    DB table, built **as part of this work**. The mbox extractor
    already proved out the pattern
    ([`MBOX_FILES_CHECKPOINT_DDL`](../../frankweiler/backend/etl/providers/email/src/extract/schema_raw.rs)),
    and Takeout's seven feeds would each need an equivalent — six
    or seven copies of the same four-column DDL is the trigger for
    pulling it into a shared module rather than copy-pasting again.

    **New shared module:** `frankweiler_etl::file_checkpoint`,
    alongside `bulk` and `render_cursor`. Surface:

      - One `ingested_files` table shared across all consumers, with
        a `scope` column that namespaces rows per (provider, feed)
        so two scopes can claim the same on-disk path without
        colliding:

        ```sql
        CREATE TABLE IF NOT EXISTS ingested_files (
            scope TEXT NOT NULL,
            path TEXT NOT NULL,
            size_bytes INTEGER NOT NULL,
            mtime_ns INTEGER NOT NULL,
            last_finished_at TEXT NOT NULL,
            PRIMARY KEY (scope, path)
        )
        ```

      - `FileFingerprint::of(path) -> Result<FileFingerprint>` —
        one `stat`; returns `(size_bytes, mtime_ns)`.
      - `should_skip(tx, scope, path, fp) -> bool` — looks up the
        row, returns `true` on match.
      - `record_finished(tx, scope, path, fp, now)` — UPSERT,
        called inside the same transaction that flushes the file's
        last batch (so a crashed run doesn't leave a stamped row
        for partially-ingested content).

    Each Takeout walker calls `should_skip` with its own scope
    (`google_takeout/<feed_name>`) before opening a file, and
    `record_finished` at the end of the file's transaction. No
    provider-specific checkpoint DDL.

    **Migration follow-up (not blocking):** the email mbox extractor
    keeps its `mbox_files_checkpoint` table for now. Migrating it
    to the shared `ingested_files` (scope = `email/mbox`) is a
    natural cleanup once Takeout is landed and proves the API. Same
    for Signal's content-hash-based `INGESTED_BACKUPS_DDL` — it
    serves a stricter equality goal so it shouldn't move under
    `file_checkpoint` until the API grows a "content-hash fallback"
    mode.

    Why mtime+size rather than content hash: cheap to check, good
    enough for the Takeout shape ("download a new export, point me
    at it"), and consistent with what mbox already does. A user who
    renames a file is fine because the path is part of the cursor
    key.
  - **Monitor**: standard `obs::ObsArgs` flow. One progress bar per
    walker (`maps_reviews`, `youtube_watch_history`, …) with length
    set from a pre-pass count where cheap (JSON, CSV) or from a
    streaming counter for HTML walkers.
  - **`--reset-and-redownload`**: wipes the entity tables; the photo
    blobs in the CAS survive (same convention as every other
    provider).

## Errors

Per the
[doc's two-axis rule](data_architecture_ingestion.md#error-handling):

  - A malformed `Reviews.json`, a 0-byte `MyActivity.html`, or a
    missing referenced attachment file → log + increment counter +
    keep going.
  - A non-readable Takeout root (permission denied on `input_path`) →
    fatal.

There is no upstream auth, so no "401 → fatal" axis.

## File layout

```
providers/google_takeout/
  Cargo.toml
  BUILD.bazel
  src/
    lib.rs                         # pub mod extract; (no translate yet)
    extract/
      mod.rs                       # fetch() entry; per-feed dispatch
      schema_raw.rs                # row structs (#[derive(WirePayloadRow,
                                   # CasEdgeRow)]) + DDL constants + PK
                                   # recipes + GOOGLE_TAKEOUT_NS
                                   # (no per-feed file-checkpoint DDLs —
                                   #  goes through file_checkpoint module)
      db.rs                        # thin RawDb wrapper around the shared
                                   # bulk/checkpoint helpers; no SQL of
                                   # its own
      time.rs                      # the two timestamp parsers
      maps_reviews.rs              # per-feed walker
      maps_saved_places.rs
      maps_photos.rs
      youtube_watch_history.rs
      youtube_subscriptions.rs
      google_chat.rs
      gemini_apps.rs
      mdl_html.rs                  # shared MDL outer-cell parser
  tests/
    fixtures/
      Takeout/                     # TNG-themed fictional Takeout tree
        Maps (your places)/
          Reviews.json             # Picard reviews Ten Forward, etc
          Saved Places.json
        YouTube and YouTube Music/
          history/watch-history.html
          subscriptions/subscriptions.csv
        Google Chat/
          Groups/DM TNG-CREW/...
        My Activity/
          Gemini Apps/MyActivity.html
    fixture_walk.rs                # one integration test per walker
```

## Sync wiring

  1. Add `SourceConfig::GoogleTakeout` variant to
     [`core/src/config.rs`](../../frankweiler/backend/core/src/config.rs)
     with the `sync:` struct above.
  2. Dispatch in
     [`sync/src/main.rs`](../../frankweiler/backend/sync/src/main.rs)
     to `frankweiler_etl_google_takeout::extract::fetch(...)`.
  3. Add crate to workspace `members =` in
     [`backend/Cargo.toml`](../../frankweiler/backend/Cargo.toml) and to
     `crate.from_cargo` in `MODULE.bazel`.

Load needs no changes (it's provider-agnostic and there are no
sidecars yet).

## Out of scope (first pass)

  - **Translate / render.** No `.md` output, no `GridRow`s yet. We'll
    decide per sub-feed which `GridRow.kind` they want once raw is
    landing cleanly. Watch-history → likely its own family.
    Gemini-apps → fits the chat-LLM family alongside Claude/ChatGPT.
    Google Chat → fits the chat-human family alongside Slack/Beeper.
  - **My Activity for Search, YouTube, Drive, Calendar, Gmail.** Same
    HTML format as Gemini Apps; we can extend the
    `My Activity` walker per-product once the Gemini one is shaken
    out. Not in this pass.
  - **Timeline (location history).** Separate file format, separate
    privacy story, separate UI shape. Not in this pass.
  - **Maps subdirectories that were empty in test data.** Listed
    above; pick them up when populated examples land.

## Resolved calls

1. **HTML parser** — hand-rolled `outer-cell` state machine. No
   `scraper` / html5ever dep for now.
2. **Maps photos** — JPEG bytes go into the CAS, same plumbing as
   Chat and Gemini attachments. Photos are first-class.
3. **DM display name** — not denormalized into the raw tables.
   Stays opaque-id-keyed; translate derives the display name from
   `chat_groups.payload.members[]` and `chat_users.payload.user.email`.
4. **uuidv5 namespace** — one provider-wide constant
   `GOOGLE_TAKEOUT_NS`. Entity kind goes in the recipe string
   (`"maps_review:..."`, `"youtube:watch:..."`, etc.).
5. **Gemini Apps response content** — the outer `<div class="outer-cell">`
   MDL scaffolding is the rendering we discard. The inner response
   chunk (`<p>` / `<blockquote>` / `<ul>` / `<code>` / `<strong>` /
   `<li>` markup that is Gemini's actual answer) is extracted data
   and lands in `payload.response_html` verbatim, the same way
   `prompt_text` does. Translate decides later whether to convert
   to markdown or pass the HTML through.
6. **Watch-history (and Gemini Apps HTML) at scale** — a 100k-entry
   YouTube history is ~100 MB (the inlined MDL CSS block is a
   constant ~140 KB; entries are ~1 KB each). 100 MB fits in RAM
   with room to spare on any laptop. Plan: `std::fs::read_to_string`
   the whole file, walk the resulting string with `str::find` for
   `<div class="outer-cell"` boundaries. No chunked I/O, no streaming
   parser, no over-engineering for a worst case that doesn't exist.