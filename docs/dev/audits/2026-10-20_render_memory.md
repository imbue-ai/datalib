# Render memory audit — 2026-10-20

A dated record, not reference: it describes the tree as it stood at
`e1981741c` and what was decided from it, for issue #1258 (renders load
a whole source into memory). Read the code before repeating any claim
here as current. How a render reads a page at a time now is
[`data_architecture_parse_and_render.md`](../data_architecture_parse_and_render.md)
§"Memory: a page at a time".

## What each render held

Every render emitted its documents one at a time through `emit_doc`,
and the step driver kept only keys (§"The driver"). The memory went in
the provider's parse, before the first document: almost every one read
its primary tables whole, parsed every payload into a
`serde_json::Value`, built every chat, and kept every bucket's
`Vec<Input>` until the end of the run. Narrowing to the stale buckets
mostly happened in Rust, after the load.

| render | full walk | incremental | blobs up front |
|---|---|---|---|
| **email** | every thread's rows and joins (`load_buckets` over every thread) | stale threads only, one bound array | no: `.eml`s 200 threads at a time |
| **slack** | every message parsed (`load_all_messages`), twice over (`loaded_to_message` cloned the payload) | stale threads, `?` per key in chunks of 500 | every thread's attachments, then cloned again into `blobs_by_chat` |
| google_takeout | every table as `Value` (`load_rows`, `load_file_rows`, `load_payloads_with_id`) | loads all, filters | every Chat, Voice, Gemini and Maps blob into one `HashMap` |
| whatsapp | every chat, message, media row | loads all; narrows after | every chat's, even incremental |
| sms_backup_restore | every message and call | loads all | every chat's, even incremental |
| linkedin | every message table, connection, share | loads all | every photo, one CAS query each |
| beeper | rooms, users, every media edge and CAS row; events | events by one bound array | every document's, one `cas.get` each |
| chatgpt, claude, notion, facebook | every conversation / page / table | loads all, filters | only for the filtered set |
| signal | every chat's items on a full walk | stale chats, one bound array | only for the loaded chats |
| contacts | every vCard, inline photos decoded | loads all | photos are inline |
| claude_code, codex, github, gitlab, calendar, apple_messages, garmin | every row | loads all, filters | none |
| airvisual, yolink, pdf, perseus | one page, one file at a time, or a fixed corpus | — | — |

No render paged its primary table. Prior art for the pieces: bound
`json_each` arrays (signal, beeper), keyset paging on the ingest side
(`email/src/ingest/db.rs`).

## The driver

`datalib_step/src/render.rs` never holds a document: `put_document`
writes it and `put_inputs` writes a bucket's inputs as they are
declared. What it keeps for the run is keys — every emitted
`markdown_uuid`, every bucket's end and whether it is new — a few sets
of short strings per document, which grow with the source but slowly.

## Measured

A synthetic Slack store: the TNG fixture's 16 messages copied 20,000
times under new channel ids, each message's text padded with ~1.2 KB
of random hex — 320,016 messages in 240,012 threads, no attachments,
944 MB on disk. One cold `datalib-step render_markdown` (no render
store), `/usr/bin/time -l`, on the same mac, nothing else rendering:

| build | peak RSS | wall |
|---|---|---|
| `main` (`e1981741c`), two runs | 2.6 GB, 3.5 GB | 421 s, 336 s |
| paged email and slack, four runs | 0.7–1.3 GB | 343–410 s |

RSS of the paged build sampled every 10 s rose and fell between about
190 MB and 890 MB through the run rather than climbing with the
documents written: the remaining peak is the writer's, below.

## Where the rest goes: doltlite's write buffers

From `doltlite.c` (v0.50.16 amalgamation):

- **Every b-tree keeps its own map of pending edits for the SQL
  transaction** (`ProllyMutMap`, on `TableEntry.pPending`), drained
  into the tree only when it reaches 64 MB or 1M entries
  (`PROLLY_MUTMAP_PENDING_FLUSH_BYTES`, `..._LIMIT`) or the transaction
  commits. A render store has about ten written b-trees (`markdowns`,
  `grid_rows` and its index, `edges`, `render_inputs` and its index,
  `problems`, the contact tables). The env override
  `DOLTLITE_MUTMAP_DRAIN_BYTES` exists only in test builds.
- **Built chunks are buffered up to 64 MB** (`CS_PENDING_DRAIN_LIMIT`)
  before they go to the file's WAL: a transaction's bulk is on disk,
  not in memory. Same test-only override.
- **The page cache is 64 MiB per connection**
  (`DOLTLITE_DEFAULT_CACHE_SIZE -65536`); `PRAGMA cache_size` sets it.
  A render holds two: the raw store it reads and the render store it
  writes.

`dolt_commit` is not what releases memory — the SQL `COMMIT` is. The
driver held one SQL transaction across each 15 s checkpoint.

## What was tried on the writer

Two knobs, as a throwaway build, on the same store (one run each; peak
RSS varied ±15% or more between identical runs, and some runs shared
the machine with a disk scan):

| setting | peak RSS | wall | render store |
|---|---|---|---|
| neither | 949 MB | 343 s | 6.9 GB |
| SQL `COMMIT` every ~32 MB of documents | 835 MB | 399 s | 10.4 GB |
| `PRAGMA cache_size` 16 MiB | 845 MB | 380 s | 7.4 GB |
| both | 726 MB | 403 s | 10.3 GB |
| `COMMIT` every ~16 MB, cache 8 MiB | 572 MB | 448 s | 13.2 GB |

`main`'s render store for the same data was 5.7 GB; the paged one is
about 20% larger, because buckets declared per page put
`render_inputs` writes inside the checkpoints. `dolt_gc` alone gave
back 3% of the paged store (7.51 → 7.30 GB) and 9% of the 32 MB
variant (10.6 → 9.65 GB): the checkpoint commits pin those pages.
Folding the history to one commit and then `dolt_gc` brought both to
1.47 GB, every row intact.

## What was decided

- Email and Slack render a page at a time (`datalib_etl_render::pages`),
  with the page size in a config's `[render_pages]`, defaults 500
  buckets and 64 MB of blobs.
- Neither writer knob ships: what they save in memory they cost in
  disk that only folding history gives back. Folding history, for raw
  and render stores alike, is planned in #1266.
- Doltlite's 64 MB per-table pending-edit threshold is a compile-time
  constant; a runtime setting upstream is the lever for the rest.

## Measured after the follow-up

A synthetic SMS Backup & Restore store: the TNG fixture plus 2,000
conversations of 10 MMS messages, each with one distinct 100 KB
attachment (20,000 attachments, 2.05 GB of blob bytes). One cold
render each:

| build | peak RSS | wall |
|---|---|---|
| every chat's attachments loaded up front (#1267) | 2.59 GB | 15.2 s |
| a batch of chats' attachments at a time | 356 MB | 9.6 s |

## Still open

- The other renders read their stores whole; `pages` is the pattern.
  Every render with attachments loads them a batch at a time since
  #1297 and its follow-up (`chat_common::blobs`), but only email and
  Slack page their rows.
- The `embed` steps (3–5.5 GB in the issue's run log).
- A test per large source that fails on a peak over budget.
