# Audit: loose ends, repeated code and the problems sink, 2026-10-05

A record, not reference. It reads the tree at `ecc6f0fe4` (`main` after
#1005) with PR #1004 merged in, and asks four questions:

1. What do recent PRs, audits and plans call "still open", and is each
   one still open in the tree?
2. Where do the ingesters and renderers repeat each other, most of all
   in how they read and write doltlite?
3. Which rules does every provider follow by convention, and which of
   those could a type, a shared function or a lint check enforce?
4. Does the `problems` table report what went wrong accurately, and does
   a row clear when, and only when, the thing was fixed?

It also reviews #1004 itself (§1).

**How it was read.** Seven read-only passes ran in parallel: loose ends;
ingest for the API-backed providers; ingest for the file-backed
providers; the render crates; the problems sink on the ingest side; the
problems sink from render to the UI; and a correctness review of #1004.
Three labels say how far to trust a finding. **Checked** means the claim
was read in the code a second time, by a different reader. **Read**
means one reader traced the code path. **Plausible** means the code was
found but the failure was not traced. No tests were run and nothing was
reproduced, so every bug below wants a failing test before its fix.
Paths are under `datalib/backend/` unless they start with `docs/`,
`scripts/` or `datalib/`; `P/` is `etl/providers/`.

An item already in [`2026-10-02.md`](2026-10-02.md) or
[`2026-10-05_whack_a_mole.md`](2026-10-05_whack_a_mole.md) is marked
**recorded** and kept short.

## The short version

Four changes would close most of what follows. Each is described where
it first comes up.

1. **The ingest session owns the run** (§2.1). Today every processor is
   `fetch(..).await?; session.finish(..)`, so a `fetch` that returns
   `Err` commits nothing and the next open throws the work away. One
   `session.run(|ctx| fetch(..))` decides once what an error keeps,
   closes the pools on both paths, starts and ends the run record, and
   carries the pinned now.
2. **The framework owns the problems of a run** (§3.3). A collector on
   the session that knows which keys were *tried* this run, writes once
   at the end, and writes nothing after a stop. It replaces about 30
   hand-written `report_run` calls, about 20 stop gates and three
   carry-over helpers, and makes "cleared only when retried and it
   worked" true by construction.
3. **Completeness is a type** (§2.3, recorded as #991). Only a listing
   that reached its end can build the value a prune takes.
4. **Render hands back a draft, the framework writes it** (§4.2). One
   `emit(DocDraft)` and one `finish` close six of the uneven render
   patterns, and a `ReadScope` that names the entities looked at closes
   the two problems-never-clear bugs in §3.2.

## 1. PR #1004

The PR is right in its aim and in most of its code. The shared pieces
(`prune.rs`, `file_checkpoint.rs`, `blob_cas.rs`, the Garmin repair, the
YoLink windows table, the Claude stub prune, the `RecordIter` fix) were
read and found sound. About a third of the non-test diff was read in
substance: forge, Garmin, YoLink, Notion, Gmail, Facebook, media, PDF.
The ChatGPT, Claude attachment, JMAP, Takeout and file-provider commits
were not reviewed line by line.

### 1.1 A give-up now discards the whole run in GitHub, GitLab and Notion (checked)

The PR turns "a rate limit or the shared retry giving up" into an `Err`
from `fetch` (`etl/forge-ingest-common/src/lib.rs:250,482,532,551`;
Notion's `run_over` sites in `P/notion/src/ingest/mod.rs`). Those three
providers pass no sealer, so their only commit is `session.finish`,
which the `?` in the processor skips (`P/github/src/processor.rs:63-88`).
The next writer open runs `discard_dirty_working_tree`
(`etl/src/doltlite_raw.rs:1466`). A first GitHub sync that trips the
retry guard at PR 1,400 of 2,000 keeps nothing, moves no cursor, and
starts from zero next time, likely to stop at the same place. Before the
PR these paths logged and returned `Ok`, so the partial work landed.

The tests cannot see it: the helper in
`P/github/tests/github_tests/run_problems.rs:69-83` calls
`db.commit_all("test")` after an `Err`, so `a_give_up_ends_the_run`
asserts a stored state production never keeps. Notion's helper does the
same.

**Fix before merging:** do what Claude and Garmin do in the same PR:
record a `RunProblem::phase`, leave the cursors alone and return `Ok`.
Make the test helpers skip the commit on `Err`. §2.1 is the general fix.

### 1.2 With `max_prs`/`max_mrs`, items that always fail take the cap first (read)

Retried items sort ahead of new ones and each spends a cap slot
(`forge-ingest-common/src/lib.rs` `with_retries`, about `:333-360`). Five
PRs that always come back short, with `max_prs = 5`, mean every run
fetches those five and owes everything else, for good. Owed rows are
also stored as `Reason::OverSizeLimit`, which reads as a size problem,
and their `attempt_count` climbs without an attempt. Fix: order retries
oldest-attempt first or do not charge a repeat failure to the cap, and
give "over this run's cap" its own reason.

### 1.3 Notion: a 403 on comments makes every page a retry, every run (read)

`P/notion/src/ingest/mod.rs:839` records a failed comments listing on the
page row, which puts the page in `failed_page_ids` and exempts it from
the unchanged skip. A credential without the comments capability
therefore refetches the whole workspace on every run (`comments`
defaults to on). Before the PR this was `unwrap_or_default()`. Fix: one
`RunProblem::forbidden("comments", ..)` for the run, then stop asking.
That Notion answers 403 here is from its docs, not probed.

### 1.4 YoLink: every 4xx on a device with no readings is "before the device existed" (read)

`nothing_to_retry` in `P/yolink/src/ingest/mod.rs` treats a refusal as
"nothing there" when the device has no first reading. A new device with
a wrong id gets no window row, no problem and a walk that ends `Done`;
before the PR, 30 failures in a row abandoned it with an error. Fix:
apply the rule only to the status YoLink returns for a window before
deployment, and count 401/403 toward the budget.

### 1.5 Smaller

- **"Gone upstream" leaves nothing to see.** A PR that search lists but
  404s on detail is forgotten with an `info!`. GitHub and GitLab answer
  404 for "not allowed" too, so a token that lost access looks like a
  deletion. A non-root Notion page that 404s is retired with no row.
  One warning row ("upstream answers 404; stored copy kept") would do.
- **Gmail** still holds the history cursor whenever a message failed
  (`gmail_api/mod.rs:203`), though failed ids are now retried from their
  own rows. One message that fails for ever pins the cursor until
  history expires.
- **Docs the PR left wrong:** `docs/dev/data_architecture_ingestion.md`
  still names YoLink's failure budget as the template for returning
  `Err`, and says a give-up "stops cleanly with what it committed".
  `etl/src/download_problems.rs:340-344`: `earlier_records` was inserted
  between `report_records` and its doc comment.
- **The four "noticed, out of scope" items are all real** (read):
  Gmail relabels (`gmail_api/mod.rs:519-523,889`, recorded 10-02 §2);
  mbox never applies `blob_size_limit_bytes` (`mbox.rs:73-216`);
  `signal_render` declares attachment inputs as `pk_recipe(item, ref)`
  where ingest writes `{chat_item_id}#{slot}`
  (`signal_render/src/render/parse.rs:249-252`); `scope_key` is
  `VARCHAR(96)` (`problems/src/lib.rs:326`), unenforced.

## 2. Ingest: how the downloaders write doltlite

### 2.1 An error from `fetch` keeps nothing since the last seal (checked)

Every one of the ~30 ingest processors has the same shape: open the
store, `fetch(..).await?`, format a summary, `session.finish`. On `Err`:

- Nothing is committed. Only chatgpt, claude, slack, JMAP/Gmail and
  garmin seal mid-run; **no file-backed provider does**, nor notion,
  forge, calendar, contacts, yolink, airvisual or beeper. For those an
  error loses the whole run, problem rows included.
- `DownloadRun::finish(&Err, ..)` writes `status='error'` to `sync_runs`
  (`etl/src/download_run.rs:97-101`) and that row is discarded with the
  rest. An error row cannot survive in any provider.
- The pools are dropped, not closed, against the rule in AGENTS.md.

Garmin already works around this by returning `Ok` after failed phases.
A multi-gigabyte first mbox or Takeout run likewise publishes nothing
until the end and loses everything on a stop.

**General fix:** `RawStoreSession::run(|ctx| fetch(..))`. It decides once
what an error commits (the natural rule: seal what was written, then
fail the step), closes on both paths, and is where the next three
patterns go.

### 2.2 Patterns followed by convention

| Pattern | Follow | Deviate | Enforce with |
|---|---|---|---|
| Data, then seal, then cursor | garmin, JMAP, forge (since #1004) | nothing found broken; nothing enforces it | cursor writes only through `sealer.advance(scope, value)`; lint `upsert_scope_state(` elsewhere |
| One pinned `now` | media, pdf, claude export, forge, notion | slack measures its sweep from `Utc::now()` (`P/slack/src/ingest/db.rs:86`, `mod.rs:1497`); sms, takeout, signal, beeper, fsindex, mbox, JMAP and `file_checkpoint.rs:110,181` use `now_local()`; seven representations in all | a required `now` on a shared `FetchCommon { db, now, progress, control, sealer }`; lint `now_local()`/`Utc::now()` under `providers/*/src/ingest` |
| A run record (`DownloadRun`) | 9 of 14 API paths, 2 file ones | contacts, garmin, yolink, airvisual, mbox and every fsscan provider have none; nothing reads `sync_runs` outside tests | start and finish it in the session, or delete it |
| Open through `raw_db!` | most | slack, contacts, facebook, linkedin hand-roll `RawDb` (three recorded) | let the macro take extra fields |
| One UPSERT shape, no `COALESCE` | most | notion writes six hand statements row by row (`P/notion/src/ingest/db.rs:123-336`) and never reaches the upsert metric; mbox and `yolink_windows` bypass it too | a partial-row variant on `BulkUpsertable`; lint `ON CONFLICT` in providers |
| Sidecar DDL | — | every `full_ddl()` lists its tables, then a second hand list for `bookkeeping_ddl_for`; a forgotten name is a runtime "no such table" | a `RawSchema` builder where each derive adds its own sidecar |
| Deleting a record deletes its sidecar, edges and problems | `prune.rs` | seven hand copies (§2.4) | `prune::delete_ids_in_tx` plus child tables declared on the edge derive |
| HTTP through the shared layer | most | yolink and perseus run bare `curl -sSfL` with no timeout and no stop check (`P/yolink/src/ingest/mod.rs:839`, `P/perseus/src/ingest.rs:146`) | lint `Command::new("curl")` under providers |
| Unreadable file: not stamped, retried. Unparseable file: stamped with a `file:` row | sms, airvisual, mbox, ics, agent_sessions, `ingest_snapshot` | takeout chat, voice, maps photos and vcf are on the older unstamped path; agent_sessions stamps a file that "names no session" with no row (`etl/agent_sessions/src/lib.rs:250-253`) | a per-file closure returning `FileRead { Rows, Unreadable, Unparsable }`; the driver decides the stamp |
| Don't read iCloud-evicted files | media | pdf, sms, mbox, takeout, agent_sessions, vcf, ics | make the veto a `ScanOptions` default |
| A cursor records its scope (lint check 8) | `upsert_scope_state` users | cursors written through `file_checkpoint::record_file*` and signal's `ingested_backups` are invisible to the check | extend the check's pattern |
| Sort a bag before storing | chatgpt, claude, gitlab (three private canonicalizers) | github stores `labels`, `assignees`, `requested_reviewers` as received (plausible: not shown that GitHub reorders them) | `sorted = [..]` on the row derive |

Three generations of row derive coexist: `WirePayloadRow` (10 providers),
`RawTable` (4) and nine hand `impl BulkUpsertable`s, most of which exist
only to leave a cursor column out of the upsert. A `#[skip_on_upsert]`
attribute would retire them.

### 2.3 Likely bugs

- **ChatGPT: a listing page with no `items` key prunes every
  conversation (checked).** `P/chatgpt/src/ingest/mod.rs:1033-1037`
  reads a missing array as empty, `:1064` calls an empty page the end of
  a complete listing, and `:385-391` prunes to the keep-set. A 200 whose
  shape changed is enough. Garmin's `listing_array` is the right shape:
  a missing array is an error. Same class as #991.
- **LinkedIn: two CSVs that share a table erase each other (checked in
  code; not shown that real exports contain such a pair).**
  `canonical_table` strips a trailing `_<digits>`
  (`etl/src/export_files.rs:31-35`), and `write_table` prunes per file to
  that file's ids (`P/linkedin/src/ingest/mod.rs:179-180`). Facebook
  groups by table first.
- **mbox: a walk that finds no file reports nothing (checked).**
  `P/email/src/ingest/mbox.rs:248-255` returns before
  `scan.walk_problems()` and `report_run`. An unreadable folder is a
  green run with a `warn!`, and old `listing:` rows go stale.
- **Slack's two prune paths each leave something behind (read).**
  `delete_messages` (`P/slack/src/ingest/db.rs:493-514`) leaves
  attachment sidecars and both tables' problem rows; `prune_thread_replies`
  leaves attachment edges. The hand deletes in calendar and contacts
  leave problem rows too.
- **JMAP falls back to a full enumeration on any `Email/changes` error
  (read)**, a network blip included (`P/email/src/ingest/mod.rs:777-782`).
- **Gemini and the two YouTube feeds prune on a zero-row parse**
  (recorded, still open). **Media deletes a file that grew past
  `max_bytes`** (recorded, that part still open). **fsindex** truncates
  before the walk, so rows under an unreadable folder vanish (read).
- **Maps photos forgets its cursor when the product was not exported**
  (`maps_photos.rs:158-159`); the cost is a re-read, not data (read).
- **Signal writes four tables in four transactions.** Safe only while
  nothing seals mid-run; make it one before giving signal a sealer.

### 2.4 Repeated code

| What | Sites | Shared form |
|---|---|---|
| "Content-keyed, overlapping snapshot files": read-all, deletes, stamp filter, prune, held-back problem (~60 lines) | sms `mod.rs:84-223`; takeout voice `mod.rs:64-276`; mbox `:278-481`. The three disagree on when the held-back problem is raised | `fsscan::OverlapRead` |
| "Path-keyed files": scan, read changed, stamp, delete gone | `vcf_dir.rs:57-136`, `ics_dir.rs:35-116`, `google_chat.rs:223-256`; only ics checks stop | a `read_by_path` driver; airvisual and agent_sessions fit it too |
| Write a payload table and prune it | facebook `mod.rs:220-278`, linkedin `:322-374`, claude `export.rs:328-366` (which skips the problems delete) | `export_files::write_payload_table`; a no-sidecar flag on `prune_scope_in_tx` |
| media and pdf `fetch`; `untried_records` ×3 | `P/media/src/ingest/mod.rs:102-289`, `P/pdf/src/ingest/mod.rs:65-190`, `agent_sessions/src/lib.rs:294-309` | `Scan::carry_untried` |
| Begin, `bulk_upsert_in_tx`, commit | ~15 sites: beeper `db.rs:78-144`, calendar, garmin, claude, chatgpt, contacts, yolink | it is `bulk::bulk_upsert`; add a variant taking the stamp |
| `record_object_error` in its own transaction | chatgpt, claude, garmin, forge, yolink | one pool-level helper |
| Delete ids with their sidecars and problems | garmin `db.rs:78-112` (a line-for-line copy of `prune_scope_in_tx`), slack, calendar, contacts ×3, email | `prune::delete_ids_in_tx` |
| chatgpt and claude attachment plumbing (~250 lines a side), with a third `retry_attachments` in slack | `P/chatgpt/src/ingest/mod.rs:579-938`, `P/claude/src/ingest/mod.rs:1434-1602`, and the `db.rs` halves | a generic `AttachmentPass<E: CasEdgeRow>` in `blob_cas` |
| Sweep-marker helpers | claude `db.rs:34-71`, slack `db.rs:71-96` | `scope_state::{sweep_age, record_sweep}(pool, key, now)`; fixes slack's wall clock |
| JSON GET client and error enum | chatgpt `api.rs:67-105`, claude `api.rs:62-91`; forge has the typed version | move `ForgeError`/`get` into `etl/src/http.rs` |
| The DAV per-collection loop and token get/set | calendar `caldav/mod.rs:73-278`, contacts `mod.rs:92-282` | a `DavStore` trait and `dav::sync::run_collections` |
| `if let Some(sealer) = .. { sealer.wrote(n) }` | ~10 sites | a no-op `Sealer`, drop the `Option` |
| sqlite_mirror glue | four users, four shapes; whatsapp copies `MirrorOptions` into `MirrorKnobs` | one entry point |
| `ns_id` ×5, `guess_content_type` ×4 (recorded as the MIME tables), byte-bounded CAS batching ×2, takeout's eight feed arms | — | — |

Dead or stranded: `sqlite_mirror::ingest::fetch`'s `pool: None` branch
(no caller, never closes); `failed_conversation_ids` in chatgpt and
claude; `block_on_load_all` in six providers (test-only, opens a writer,
five never close); `event_store.rs` (fixture tooling living in the ingest
crate); `RawStoreSession::open_with_blobs`'s unused `_entity_path`. Six
processor headers describe a "register interrupt hook" that does not
exist.

## 3. The problems sink

### 3.1 The surface

Nine functions write fetch-stage rows under eight key shapes, all
free-text `format!`. Getting a provider right depends on four unwritten
rules: call the whole-set replace exactly once, do not call it after a
stop, carry forward the rows this run did not retry, and keep keys
unique. What follows from that (read unless marked):

- **The same failure has four keys.** "A file would not read" is
  `record:<table>:<rel>` in pdf, media, agent_sessions, airvisual and
  whatsapp; `listing:file <rel>` in facebook, linkedin, sms, calendar,
  contacts and mbox; `skipped:<part>:<hash>` in three Takeout feeds; and
  `file:<scope>:<rel>` once stamped.
- **Carry-forward is written three times** (`earlier_records`, airvisual
  `carried_over`, gmail `earlier_record_problems`) and missing in
  whatsapp and the Takeout `skipped:` parts, whose rows clear under a
  partial walk without a retry.
- **Any upsert of a row clears its fetch problem** (`etl/src/bulk.rs:103`).
  A child's failure parked on its parent is therefore cleared by the
  parent's next write, not by the child's success. Two cases:
  - *Notion comments.* The retry run upserts the page first
    (`mod.rs:727`), which clears the row and `last_error`. If the run
    stops before the comments fetch, nothing records it again, and an
    unchanged page is skipped from then on.
  - *Slack threads.* Recorded on the root message; the history walk
    upserts roots before replies, and failures are not re-recorded on a
    stop.
- **A duplicate key drops the whole report (checked).** `problem_uuid`
  is a primary key minted from the key, the insert is a plain `INSERT`,
  and `report`/`report_records` do not de-duplicate
  (`etl/src/download_problems.rs:141-163`). A config list that names one
  label twice rolls the replace back, leaves the old rows and says so
  only in a `warn!`. The same plain `INSERT` on the render side fails a
  document's transaction if one item gets two rows with the same field
  and reason.
- **`problems` has become control state.** ChatGPT, Claude, Notion and
  Gmail read it as their retry queue (for Gmail the carried detail is
  the 80-character sample); garmin, notion, forge and email delete from
  it with their own SQL. A reset that empties the table changes what is
  retried.
- **Orphans.** Nothing calls `file_checkpoint::clear_scope*`, so `file:`
  rows outlive a removed AirVisual device or a disabled Takeout feed.
- **Never reported.**
  - A CardDAV `addressbooks` name that matches nothing is skipped with a
    bare `continue` (`P/contacts/src/ingest/mod.rs:95-101`, **checked**);
    calendar reports the same case.
  - `sqlite_mirror` writes no rows at all, so Apple Messages and Apple
    Photos have none; its `VACUUM INTO` falling back to a plain file
    copy (`etl/sqlite_mirror/src/mirror.rs:138`) is the quiet fallback
    AGENTS.md warns about.
  - Slack's attachment flush failure (`mod.rs:872`); Garmin's three
    no-id drops (`mod.rs:713,1272,1396`); pdf and media size-limit skips
    (a counter, though `Reason::OverSizeLimit` exists).
- **Vocabulary.** Listing keys are display names, so two calendars with
  one name collide. "Could not parse the export file" is stored as stage
  `fetch`. `Reason::Noted` is never constructed; `ProblemReason` and
  `RunProblemKind` are never stored, so their `parse` and serde tests
  guard nothing.

### 3.2 Downstream: render, the index, the badge

The path works as designed for the common case: fetch rows are copied
wholesale into the render store on each render and from there into the
index, so a resolved fetch problem clears with no re-render. The gaps:

- **A source removed from the config keeps its problems and its grid
  rows in the index for ever** (recorded 10-02 §3, still open, read
  again). No delete in `etl/render/src/grid_index.rs` names a source the
  inputs no longer list. The `unified_index` badge keeps counting them.
- **ChatGPT: a "valid JSON, not a conversation" problem vanishes on the
  next incremental render (checked).** The check runs only over the
  changed conversations (`P/chatgpt_render/src/render/parse.rs:530-540,
  765-772`), but the report is `ReadScope::Whole(["conversations"])`
  (`processor.rs:58-62`), which deletes every `conversations:` row first.
- **Slack: a message's parse problem never clears on an incremental
  run**, because its narrowed scope is `Whole(["users"])`.
  Both come from `ReadScope` having no "these entities were looked at"
  form; `ReadScope::Partial` exists and is never constructed.
- **`put_entity_problems` sweeps by key prefix with no stage filter
  (checked; recorded 10-02 §16).** It deletes the carried fetch rows
  for that table (`etl/render/src/indexed_markdown.rs:949-953`), which
  come back only at the end of the run. A checkpoint commit or a stop in
  between publishes a store without them. Fix: `AND stage = 'parse'`.
- **A standing problem makes the whole chain run every sync (read, not
  observed).** Every report stamps `last_seen_at_utc` with the wall
  clock (`etl/src/download_problems.rs:501`,
  `etl/src/doltlite_raw.rs:2031`), so the row changes, the raw head
  moves, and render, `grid_index` and `keyword_index` are all stale.
  One misspelled label defeats "skipped, up to date" for that source.
  Keep the stamp out of the versioned content, or move it only when
  something else changed.
- **The badge is not a count of the rows.** It is the newest metric
  sample among retained runs (`runs/src/store.rs:542-583`). So:
  - it disappears when the counting run ages out (100 runs or 30 days),
    and "no sample" draws the same as "zero", though
    `problems/src/lib.rs:150-153` says otherwise;
  - a failed ingest or render reports no count
    (`datalib_step/src/ingest.rs:89` returns before `:94`), so the badge
    keeps the last good number;
  - `grid_index`'s own row about an unreadable store is in the list but
    not in the source's badge, and a source's rows count again under
    `unified_index`.
  Serving the count from the index (`/problems/groups` already exists)
  fixes all three.
- **The banner drops the jump link for dropped records**
  (`applets/src/unified_index/problems.rs:139-141`), the rows
  `carry_fetch_problems` works to link.
- **Only five render crates map a fetch problem to its document**
  (chatgpt, claude, slack, yolink; takeout by another route). Email,
  notion, pdf, whatsapp, airvisual and garmin write per-record fetch
  rows that reach the grid but no banner.
- **`keyword_index`, `embed`, `qmd_aggregator` and `embedding_map`
  report through logs only.**
- **UI:** `problemLabel` has no case for `over_size_limit` or `silent`
  (`datalib/ui/src/cards/problems.ts:8-32`); the chip title says
  "records dropped" for every error, wrong for `listing:` and `config:`
  rows.

### 3.3 A tighter surface

- **One typed `ProblemKey`** (`Fetched`, `Unreached`, `File`, `Config`,
  `Listing`, `Phase`, `Skipped`, `Silent`) with one `scope_key()` that
  hashes an over-long tail. Render matches on it instead of splitting a
  string on `:`.
- **The collector #990 asks for**, on the session from §2.1:
  `.phase(name, fut)` (catches an error or a panic), `.listing`,
  `.config`, `.attempted(key)`. At the end it replaces only the keys
  attempted this run, carries the rest, de-duplicates, writes in one
  transaction and does nothing after a stop.
- **A part's failure has its own key**, `<table>:<id>#<part>`, cleared
  only by that part succeeding.
- **Retry queues read the sidecars**, never `problems`.
- **`ReadScope::Entities(ids)`** on the render side, and the stage
  filter.
- **De-duplicate by `problem_uuid`** in the two insert paths now; it is
  a few lines and removes a way to lose a whole report.

### 3.4 What no test covers

The fixture pins the problem rows and checks they are unchanged across
runs; it never resolves one. Not covered anywhere: "resolved upstream,
gone from the render store, the index, the badge and the banner" end to
end; a retry interrupted by a stop (the Notion and Slack cases); a
duplicate key; a key longer than 96; a source removed from the config;
the incremental parse-scope clears. The Playwright badge spec injects
its chips by rewriting `/api/manage/rows`, and no spec touches the
banner. "Survives a stopped run" has a test in six providers (yolink,
Google Calendar, garmin, github, takeout, lightroom) and none in the
rest.

## 4. Render

Every renderer sets a bucket key, declares a `RENDER_VERSION` and
commits only through the driver. No renderer opens the render store
itself. That much of the contract is central already.

### 4.1 Likely bugs

- **Two render knobs are missing from `render_params` (checked for
  beeper).** Beeper's `period` (`P/beeper_render/src/processor.rs:38-40`;
  signal includes its own) and Claude's `max_project_doc_bytes`.
  Changing either re-renders only documents whose raw rows move, leaving
  a mixed tree. This is the render twin of "a cursor is only valid under
  the config that set it".
- **The shared loaders drop a row that will not deserialize, with no
  trace (read).** `load_payloads` and `load_payloads_with_id`
  (`etl/src/doltlite_raw.rs:2469-2515`) `continue` silently; eight
  render crates use them. One fix covers all eight: return the
  `Unparsed` list, as email's private copy does. This is most of #990's
  "render-side parse reporting in the remaining render crates".
- **Facebook and LinkedIn read any load error as "table absent"**
  (`.await.unwrap_or_default()` at `P/facebook_render/src/processor.rs:157`
  and four LinkedIn sites). On a narrowed run the stale buckets are then
  declared empty and their documents swept: a read error deletes
  documents and reports success (read).
- **Codex aborts the whole render on one bad line; Claude Code skips it
  silently.** Neither reports it.
- **LinkedIn posts mint a thread key from row position**
  (`P/linkedin_render/src/posts.rs:135,151`), so a linkless share's key
  shifts when an earlier row appears (read; the fixture may have no such
  row, in which case the contract test cannot see it).
- **WhatsApp** runs a store call on a runtime it then drops
  (`render/parse.rs:77`, `render/render.rs:83`; reached only with no
  ambient runtime) and opens the raw store twice per pass, which on a
  first run can pin two different heads.
- **email_render selects a column that does not exist**
  (`"references"` for `references_header`, `render/parse.rs:457`;
  SQLite reads it as a string literal). Recorded in #853, still open.
- **Dropped records that reach only the log:** claude conversations,
  whatsapp chats and messages, contacts vCard blocks, signal chat items,
  garmin (`unwrap_or(Value::Null)`), and a chat-common blob failure that
  leaves every attachment of a document a placeholder
  (`etl/chat-common/src/render.rs:316-322`).

### 4.2 Patterns followed by convention

| Pattern | Deviate | Enforce with |
|---|---|---|
| Every knob that shapes output is in `render_params` | beeper, claude; whatsapp and apple_messages hard-code the default period | an associated `type Config: Serialize` on `SourceRender`, serialized whole by default; or a contract-test leg that flips each config field |
| End the run with `ctx.finish` | the three time-series crates, pdf and linkedin hand-roll it; perseus never calls `consumed` | `run` returns `{ buckets, head, summary }`; the wrapper finishes |
| `source_id` on the document | forge and notion pass `""` and lean on a fallback in `grid_index.rs:1327` | the framework stamps it |
| `sections` joined are the `.md` | nothing checks it; five families pass none | `ctx.emit(DocDraft)` writes the file from the sections |
| `bucket_key` | `Option`, though every production site sets it | make it `String` on the draft |
| Absent raw store | four spellings (recorded) | `RenderCtx::with_raw(path, \|db, pin\| ..)`, which also owns the pin, the CAS and the close |
| A fetch problem names its document | six crates leave `item_of_entity` at its default | make the method required, so "none" is written down |
| Parse is total | see §4.1 | fix the shared loader; lint `warn!` then `continue` in `*_render/src` |
| Declare `gone` buckets | slack discards `narrow_by(..).gone`; safe today only because of one `COALESCE` in its scan SQL | the shared plan below |

AGENTS.md says the uuid recipes live in `ingest/schema_raw.rs`; 18
render crates carry an `ids.rs` that mints through `Identity::mint`.
Either the rule means only keys that are also stored raw columns, or
the doc is stale. Not resolved here.

### 4.3 Repeated code

| What | Sites | Shared form |
|---|---|---|
| Open a pinned reader (recorded) | ~28 sites; six chat parsers repeat the 15-line "open the CAS if it exists" block; several put a `?` between open and close | `RenderCtx::with_raw` |
| Map stale bucket uuids back to raw ids, narrow, wrap in a private `ScanResult`; then "declare looked-at empty, declare gone, finish" | chatgpt, claude, signal, beeper, email, slack parsers and their processors | lift chat-common's `changed_chats` into `etl/render` as a plan plus `ctx.finish_plan` |
| The three time-series processors | airvisual, yolink, garmin `processor.rs:35-66` | `timeseries_render::run_page`; garmin also rebuilds what `render_all` does |
| `item_of_entity` | chatgpt and claude identical; slack and takeout the same shape | an `owner_of` table in `etl/render` |
| Write the `.md`, build `RenderedMarkdown` | nine sites | `ctx.emit(DocDraft)` |
| claude_code and codex (recorded); github and gitlab processors and parse preambles | identical but for names | move the entry and scan into `agent_sessions_render`; `forge_render_common::run` |
| Dispatch arms | 28 mechanical arms in `datalib_step/src/dispatch.rs:214-377`; the two macros' ingest arms are one block | a `Provider` trait with `plan_render -> Option` |

`render_contract_test` asserts that incremental equals cold under
per-table delete, edit, insert and blob swap. It does not cover a
render-config change, a row shape the fixture lacks, problem reporting,
or `sections == .md`.

## 5. Loose ends

Each was checked against the tree, not taken from the record.

### 5.1 Still open

| Item | Recorded in | State |
|---|---|---|
| A source removed from `grid_index`'s inputs keeps its rows | 10-02 §3 | open; `docs/dev/config_model.md:260` still says otherwise |
| Gmail relabels of known ids never land | 10-02 §2, #1004 | open; `docs/dev/email_download_modes.md:193` still says they are fetched |
| A failed read of the saved layout overwrites it | 10-02 §9 | temp name fixed by #998; `ContainersView.vue:199-230` still saves a fresh tree after a failed load |
| Quick restart can run a second copy of a step still stopping | 10-02 §7 | open (`dag/src/supervisor/host.rs:179-201`, no liveness check) |
| Listing completeness as a type; Gmail empty-labels guard; Takeout zero-row prunes; Google Calendar's unbuildable event left out of `seen` | #991 | open |
| Framework collector, `Scan::read_each`, the lint check | #990 | open; the per-provider instances are closed by #1004 |
| Typed markup wrappers; `data-handle` trusted by DOM position | #992 | escapers and YAML unified by #1003; no types yet |
| "Move into Default": no lock check or rollback | 10-02 §8 | open; carries `TODO(after 2026-11-01): remove` |
| Temporary code due out at 0.41 | in tree | `runtime/src/legacy_qmd_dir.rs` (its test fails from 0.41) and `DOLTLITE_CAS` in `etl/src/blob_cas.rs:102`; the next minor bump must remove both |
| Desktop prefs reset on every launch | whack-a-mole §6, never filed | open |
| Dashboard copies the row actions and never pauses off-screen | 10-02 §19 | open (`ui/src/cards/useDashboard.ts:103-168`) |
| The server's loop ends for good if `Store::open` fails | 10-02 §11 | open (`http/src/supervisor.rs:190-203`) |
| Standalone `datalib-fsindex` commits without publishing | 10-02 §15 | open |
| Search card's second filter parser splits quoted filters | 10-02 §14 | open (`ui/src/cards/search.ts:21-40`) |
| The desktop shell is not compiled on PRs | 10-02 §17 | open |
| 42 dead `diff_type != 'unchanged'` filters | #893 | open, mechanical |
| Manage status key is twelve strings with a silent fallback | 10-02 P3 | open; against "name a closed set of strings" |
| Three sign-in bugs pinned as `test.fail()` | #1001 | open |
| `always_clear_before_ingest`: keep or delete | #898 | undecided |
| `beeper_render` writes `author_handle: None` | #999 | open |
| `?token=` accepted; no `Origin` check; release workflow permissions; unpinned `buildkit:latest` | 09-17, 10-02 P3 | open where checked |

### 5.2 Records that say open, tree says done

- Issue #791: the `carddav` step names, garmin on `timeseries_render`,
  and most private `identity()` copies are gone. The issue is still open.
- Whack-a-mole §4 (`has_table` copies, closed by #1000), §5
  (`RULES_VERSION`, one tel cleaner), §7 (`write_atomic` with lint check
  14, `LANG` for build actions), and §3's header and front-matter holes
  (#1003). The audit file's "still open" lists are not annotated.
- 10-02's "What was done" header does not record #998 (finding 9's temp
  name), #1003 (the YAML quoters) or #1004 (finding 10, the first half
  of 16, the media reconcile in 4).
- `docs/dev/plans/problem_visibility.md` says `Problem::lossy` has no
  callers; it has two. Its "provider tail" is largely closed by #1004.
  Its D3 line "drops a source's rows when it leaves the config" and D4's
  "unknown drawn as such" are not built; R3 (the lossy-rules table) and
  R4 (the drop budget) are not started.
- `docs/dev/plans/source_wizard.md`'s list of `wizard: false` sources no
  longer matches `catalog.ts`.
- Issue #633 (the FCIS checklist): several boxes name code the
  supervisor replaced. It needs re-triage, not work.

## 6. Lint checks worth adding

`scripts/lint_repo.py` has fourteen. In rough order of value:

1. No `FROM problems` / `DELETE FROM problems` under `etl/providers/**`
   or `forge-ingest-common` (flags ten sites today).
2. A `warn!` or `error!` beside a `continue`, a `+= 1` or a `return Ok`
   under `providers/*/src/ingest`, `sqlite_mirror/src` and
   `*_render/src`, with no problem token in the block. This is #990's
   check.
3. `now_local()` / `Utc::now()` under `providers/*/src/ingest`.
4. `Command::new("curl")` under providers.
5. A whole-set report (`report_run`, `report_records`, `report`) in a
   function with no stop check. Goes away with the collector.
6. `ON CONFLICT` in provider code outside an allowlist.
7. Extend check 8 to cursors written through `file_checkpoint`.

## Not covered

- The UI beyond the problems badge, grid and banner; the supervisor;
  `datalib-http`; the desktop shell. 10-02 read those.
- Line-by-line: `blob_cas.rs`, `bulk.rs`, the macros crate,
  `sqlite_mirror`'s engine, beeper's and garmin's inner walks, notion's
  search cursor, gmail's history replay, the signal-backup and
  whatsapp-backup crates.
- On the render side: reader statements against the allowlist in
  `etl/README.md`, the `GridRow` field matrix, timestamps and their UTC
  twins, `data-handle` emission, edges, and the `msg` div wrappers
  outside the chat family.
- Two thirds of #1004's non-test diff (see §1).
- GitHub and CI settings.
