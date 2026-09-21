# `datalib-etl` — shared ingest machinery

Everything a provider needs but should not re-invent: the doltlite-backed
raw store (`doltlite_raw.rs`, `bulk.rs`), the blob CAS (`blob_cas.rs`), the
render cursor, the local-tree walker (`fswalk.rs`), and the HTTP/auth
plumbing under `http.rs`.

Provider-specific code does **not** belong here. A provider crate lives in
`providers/<name>/` and describes only its own tables and its own upserts.

The rules below are the ones you cannot recover by reading the code, and
which break things quietly when ignored.

## Raw stores: the primary key is the upstream id

Every object table's PK is the **upstream identifier**, stored as TEXT. No
surrogate autoincrement integers, no ROWID-as-PK tricks.

- `dolt diff` compares rows by PK, so a re-fetch of the same upstream row
  has to land on the same row for the diff to mean "content changed".
- `ON CONFLICT(id) DO UPDATE` is only meaningful when `id` is the upstream
  id.
- Pre-seeding `(id, NULL payload)` — recording that something exists before
  fetching its body — needs both writers to know the PK up front.
- Cross-table references (`blocks.parent_id`, `messages.conversation_id`)
  only mean something if they point at upstream ids.

Ordering within a parent is a separate concern from identity: carry an
explicit integer column for it. Never borrow the PK, and never
`ORDER BY rowid` — doltlite hides it.

`sync_runs` is the one exception, and uses `AUTOINCREMENT INTEGER`: a sync
invocation is a local event with no upstream identity.

## Bookkeeping lives in a sidecar table

`payload` is content and stays on the object table. `fetched_at_utc`,
`attempt_count`, `last_attempt_at_utc`, `last_error`, `volatile_payload`
and `tz_offset` go in `<table>_bookkeeping` (see `bookkeeping_ddl_for`).
A failed attempt is also a row in the store's `problems` table (next
section), keyed `<table>:<id>`, an error when the record has never
fetched and a warning when an earlier fetch left a copy; a successful
attempt — through `record_object_attempt` or the bulk path — clears
it. **Report a per-record failure through `record_object_error`**, not
only through `warn!`: a log line is about a run, a problem row is about
the record, stays until the record fetches, and reaches the screen.
The two stamps are UTC and `tz_offset` is the offset the writer's clock
was in — the pair every stamp we mint is stored as (AGENTS.md,
"Timestamp convention").

The split keeps `dolt diff` over the data tables reflecting upstream change
only, not re-fetch churn — which is what makes the `--reset-and-redownload`
"did anything actually change?" assertion mean anything.

Every object row gets a sidecar row in the same transaction; use
`ensure_object_row` to seed both.

## Problems flow downstream with the data

Every store a step owns holds a `problems` table (`datalib_problems`,
in every raw store's `SHARED_DDL`): one row per thing the step could
not fully do to one record, with a severity, a deterministic id and a
sweep key. The owner sweeps it — per document in a render store, per
entity in a raw store — and each consumer that reads a store pinned
copies that store's rows for the source **wholesale** into its own,
then adds its own: render copies the raw store's fetch-stage rows
(minting them again under the source's id, which a download does not
know), `grid_index` copies every render store's into the index. The
pinned store is the complete truth about its source's problems at that
commit, so the copy is the sweep and there is nothing to diff. Stamps
travel with the row. The step then reports whole-store counts as
`problems{severity=…}` metrics, which the Manage screen reads. Design
and surfaces: `docs/dev/plans/problem_visibility.md`.

## Volatile fields: split them out, don't diff them

Some payloads carry per-fetch fields that describe the fetch rather than the
object — Slack bumps a channel's `updated` millis spuriously. Left in the
content payload they make `dolt_diff_<table>` report a change on every
re-download, which defeats incremental render.

Declare them per-provider as `VolatilePath`s next to the table definition;
`split_volatile` moves them into the sidecar's `volatile_payload`, and
`overlay` reconstructs the wire object exactly.

This is different from sorting an unordered array (see AGENTS.md): volatile
means *the value carries no information*. If losing the value would lose
signal, sort it instead.

## JSONB payloads

`payload` columns store JSON as SQLite JSONB. Inserts wrap the bound text in
`jsonb(?)`; reads select `json(payload)` so Rust still gets text for
`serde_json`. Ad-hoc CLI queries need `json(payload)` too.

`sync_runs.config` / `summary` stay plain TEXT — tiny, single-row, and worth
being greppable.

## Connection pools: one writer per file, readers pinned

Doltlite's HEAD pointer, working set and active branch are **per
connection**, and the working set is also **per file**, shared across
processes. Two facts, two rules, both built into `doltlite_raw` rather
than left to convention.

### Every pool is size 1 and never recycled

A pool bigger than one lands statements on connections that disagree
about the tree, which shows up as a `dolt_commit` whose hash never
appears in `dolt_log`, or as `commit conflict: another connection
committed to this branch`. The dolt maintainers confirm the same is true
of Dolt itself and recommend the same fix. So every open here pins
`max_connections(1)` and disables `idle_timeout` and `max_lifetime`:
sqlx would otherwise retire the very connection whose session state is
load-bearing, and its replacement starts on `main` with a clean working
set — an fsindex scan on a non-`main` branch would silently start
writing to `main` after 30 minutes and report success.

### One writer per file, by construction

`open` and `open_derived` are the only ways to a handle that can commit,
and each takes the file's writer lock — `flock(2)` on the sibling
`<store>.doltlite_db.lock`, `datalib_flock` — and gives it to the
connection, which holds it until it closes. A second writer on the same
file, in another process or in this one, is refused at open with the
holder named (`<program> (pid N)`) instead of sharing the first's
working set: an `-Am` commit through either pool sweeps up whatever the
other has in flight, and two mid-write pools contend for a lock
`dolt_commit` takes without waiting. The kernel releases the lock when
the holder dies, so a killed run leaves no stale claim; the next `open`
finds its dirty rows and seals them into a rescue commit.

The lock lives exactly as long as the connection: `close().await` waits
for the connection to close, and that is the moment the store is free.
A dropped-but-never-closed handle keeps its connection, and so the lock,
until sqlx's worker thread gets to it a moment later — the window the
rule "close, not drop" has always been about. A writer that finds its
own process still holding the lock waits up to two seconds for that
close and logs that it had to, so a stray drop is a warning rather than
a refusal that depends on the machine's speed; a second *live* writer in
the same process is refused after the wait.

Per file, not per root, so two sources with nothing in common can be
written by two runners at once (#247): the lock says who owns *this*
store, and nothing about the others.

`datalib-fsindex` and the provider `*_ingest` binaries write through
`RawDb::open`, so they take the lock; `datalib-dirtree-diff` reads the
stores it is given through `datalib_pin::open_reader` and writes only
its own scratch. `datalib-doltlite` is the raw shell and takes no lock:
run it `-readonly` against a store a sync may be writing.

### A download takes the store; it never opens one

Every provider's `FetchOptions` carries `pub db: RawDb` — a live handle,
not a path and not an `Option`. **Whoever opens a store closes it**, and
for a download that is always the caller: the step's processor, the
provider's `*_download` binary, or the test. `fetch` borrows it for the
run and returns. A `fetch` that opened its own store while the caller
held one would now be refused at open rather than failing the caller's
commit later.

### A reader opens read-only and pinned, and never asks `dolt_status`

`open_reader(path, commit)` is the read path: read-only, so "a reader
must not write" is the engine's rule (`attempt to write a readonly
database`); never creates the file; takes no lock; and pins at open —
at the commit the caller names (the render driver's) or at HEAD — with
the `pinned_<table>` views installed, so every content read through
[`Reads::At`] names one commit however long the pass runs. It hands
back a `Reader`, or `None` when the store has nothing committed, which
the caller must decide about (a consumer does nothing that pass) rather
than fall through to the working set. The `pin.rs` `Pin` refuses `HEAD`
by name, and the shared loaders take a mandatory `Reads`, so a call site
has to say whose store it is reading.

The one unpinned reader is the blob CAS (`open_cas_reader`): most
downloads never commit it, so a read at HEAD would find no blob, and
content addressing is what makes the working-set read safe — a row is
keyed by the blake3 of its own bytes.

The two-process test measures what a read-only connection may issue
beside a live writer — `dolt_hashof`, `sqlite_master`,
`pragma_module_list`, `CREATE TEMP VIEW`, reads through `dolt_at_`
views, `dolt_diff_*`, `dolt_log()`, `dolt_commit_ancestors`,
`dolt_diff_summary`, `dolt_diff_stat`, a `COUNT(*)` per table — and
that list is the allowlist. **`dolt_status` is not on it**: issued from
a read-only connection while the writer commits, it fails that commit
and the rows inserted before it are gone (dolthub/doltlite#2832). The
same goes for a hand-run `datalib-doltlite -readonly … dolt_status`
against a store a sync is writing. Any other statement a reader adds is
presumed guilty until `doltlite_two_process_test` has run with it.

Two traps for a reader that holds its connection across another
process's commits, both measured in `datalib_pin`'s tests. A scalar
function answers from the session's last view of the store, so a bare
`dolt_hashof('HEAD')` keeps reporting the HEAD the connection opened at;
`datalib_pin::head` reads `sqlite_master` first, which reloads the root.
And the `dolt_at_<table>` modules are registered when the connection
opens, from the commits that exist then: a table another process commits
later has no module on this connection, and never will. A long-lived
reader — the search applet — checks `has_unpinnable_tables` and reopens.
Everything that opens per pass sees neither.

Open the store once per pass — a stage that needs to load rows, run a
`dolt_diff` scan and probe for ids does all three on one pool — and
`close().await` before the next open, on the error path too. And never
run a store call on a runtime you are about to drop: sqlx returns a
checked-out connection from a task spawned at drop, and a per-call
`Runtime::new().block_on(..)` dies before that task runs, leaving the
next open a second handle on the same file (`indexed_markdown::blocking`
keeps one process-wide runtime for the no-runtime case).

[`Reads::At`]: src/pin.rs

## What a write costs: the transaction is the unit, and the key decides the size

A doltlite file is a bag of content-addressed chunks. A table is a
prolly tree — a B-tree whose pages are chunks named by their hash —
and a chunk is never edited in place: a write produces a new leaf page
holding the changed rows *and every unchanged row that shared the
page*, plus a new copy of each page on the path to the root. The old
pages stay in the file until `dolt_gc()` finds nothing that reaches
them. A commit is a small chunk naming one root; it makes that root's
pages reachable, forever, and does nothing else.

Three consequences, each measured with `scripts/doltlite_commit_cost.py`
(doltlite 0.50.3, 100k rows of ~100 bytes; the dated table is in
`hack/doltlite_commit_cost/`):

- **A SQL transaction rewrites each page it touched once, at
  `COMMIT`.** 200 statements in 200 transactions wrote 430 MB of pages
  for 15 MB of rows; the same 200 statements in one transaction wrote
  24 MB. Every store here already batches — a render store's transaction
  is one checkpoint interval, the grid index's is the whole run, a
  SQLite mirror's is one table — so within one run the order rows
  arrive in does not matter.
- **The pages a transaction touches are the pages its keys fall in.**
  The tree is sorted by primary key. 500 rows whose keys are adjacent
  land in one or two leaves (~10 KB written); 500 rows with random keys
  land in ~500 leaves (~2 MB written, 99% of it copies of neighbours).
  Random keys are uuidv4s, uuidv5s and content hashes. Adjacent keys are
  `(device_id, ts_ms)`, `"{metric}#{date}"`, a time-prefixed uuid.
- **A commit pins whatever its transaction wrote.** Commit once at the
  end and `dolt_gc()` reclaims every intermediate page: 430 MB → 15 MB.
  Commit after each of 200 transactions and gc reclaims nothing
  (→ 419 MB), because each intermediate tree is now history. With
  adjacent keys the same 200 commits cost 1 MB, since each pinned only
  the leaf it touched.

So commit cadence is free in time (a `dolt_commit` is ~20 ms whatever it
seals) and free on disk until two things are both true: the store's
keys scatter, and something runs `dolt_gc()`. Without gc every store
carries every transaction's pages regardless, and only `sqlite_mirror`
and `fsindex` gc today. What accumulates for a scattered-key store is
the *incremental* case: a sync that adds 50 documents rewrites ~1000
leaves, and the run's commit keeps them. It is invisible on a fresh
root and compounds with every sync.

Two recoveries, both available:

- **Squash.** `dolt_reset('--soft', <base>)` then `dolt_commit` folds
  the intermediate commits into one; the table hash is unchanged and
  the next gc reclaims what only they reached (419 MB → 15 MB in the
  bench). It deletes commit hashes, so a consumer whose cursor named
  one falls back to a full pass, and a reader pinned at one loses its
  chunks at the next gc. Squash only commits older than every
  consumer's cursor, with the writer lock held.
- **Key for adjacency.** The right fix where the key is ours to
  choose: see the practice note in
  `docs/dev/data_architecture_ingestion_practices.md` § "Key a table for
  what one run writes together", and `docs/dev/entity_ids.md` for the
  time-prefixed `entity_id` proposal.

## Schema self-healing, and why the DDL runs in two passes

`open` applies `CREATE TABLE`s, reconciles each table against its DDL
(`reconcile_table_schema`: add missing columns, else drop and recreate), then
applies indexes.

The order is load-bearing. An index over a column introduced by a later
schema change cannot be created against an older store, so a single pass
fails with `no such column` and returns before the reconcile that would have
added it — leaving every older store unopenable.

Dropping and recreating is safe here specifically because raw-store rows are
a cache of upstream, re-fetched on the next sync, and doltlite keeps the
dropped rows in history. "Re-fetched" has to be made true, though: a cursor
that says "read through here" would let the next run resume past rows the
recreated table no longer has, and the table would stay empty until upstream
changed, with nothing saying why. So a recreate also clears every store-wide
cursor (`sync_scope_state`, `sync_scope_config`, `ingested_files`) and logs
that it did; the next run walks from the start, and the tables that kept
their rows absorb it as no-op upserts. Per-row cursors — a sidecar's
`last_ts_ms`, an address book's `ctag` — live on the table that holds them
and go with it.

`declared_columns` learns a DDL's columns by parsing it into a probe table in
an **in-memory** database. Never against the store being opened: a
create+drop nets to nothing in the working tree but has already appended
chunks to the file, and nothing collects them, so every `open` cost bytes
whether or not anything was ingested.

## Writes: one UPSERT shape, everywhere

Every entity table is written the same way — `INSERT INTO <t> (id, …cols)
VALUES (…) ON CONFLICT(id) DO UPDATE SET <every non-id col> = excluded.<col>`.
No `COALESCE`-style per-column policies: each write is complete, and the
newest upstream state is by definition the truth.

A provider declares its row struct and its `BulkUpsertable` impl next to the
DDL in `schema_raw.rs`, then calls `bulk_upsert_in_tx`, which chunks the rows,
emits one multi-row statement per chunk, and stamps `<table>_bookkeeping` for
every id in the same transaction. The caller commits.

`insert_sql` exists for the one path where upserting would be wrong. In
`grid_index`, a primary-key collision is a *finding* — two sources minting
the same `grid_rows.uuid` is a correctness emergency — so that path wants the
generated column list and binds without the `ON CONFLICT` clause, and lets
the error surface so it can name the other document.

`BulkUpsertable` itself is defined in `datalib_schema::bulk` and re-exported
here, because `datalib_etl` depends on `datalib_schema` and the render-schema
structs could not implement a trait that lived in this crate.

## Blob CAS and per-provider edge tables

Each source's raw directory holds two doltlite files: `entities.doltlite_db`
(entities plus that provider's CAS edge table) and `blobs.doltlite_db` (pure
CAS). Bytes are keyed by their blake3 hash and stored exactly once in
`cas_objects`; each provider declares its own `(owning_id, ref_id, blake3)`
edge table via `#[derive(CasEdgeRow)]` over a four-field struct, in that
order. The derive reads the second and third field names to emit
`OWNING_COLUMN` / `REF_COLUMN`, so a provider's `schema_raw.rs` is the struct
and the attribute and nothing else.

The bundle is the common vocabulary at both ends. Download adds bytes as they
arrive and drains the bundle at end of bucket; parse loads one document's
refs in two queries regardless of how many attachments it has; render then
consumes an already-loaded bag of bytes — no SQL, no `block_in_place`, no dyn
blob reader.

**A source that keeps a CAS opens its session with the CAS attached**
(`RunCtx::open_store_with_blobs`), so every seal commits `blobs.doltlite_db`
before `entities.doltlite_db`. The order is the point: a reader pinned at an
entities commit must never find a row naming bytes that are not committed
yet, and a CAS with no commits can be neither pinned nor versioned. Nothing
in the CAS uses doltlite's diff or history — a hash is either present or it
is not — so which container the bytes should live in at all is an open
question; the `BlobCas` API is narrow enough that changing it is contained.

**Filenames dedupe on the content hash, not on the derived name.** A blob's
rendered filename has a content-addressed stem and an extension derived from
*this ref's* metadata, so one payload reaching the bundle under two refs with
different metadata derives two names and gets written twice — a Google
Calendar invite arrives once as an inline `text/calendar` part and once as a
named `.ics` attachment. Collapsing on `blake3` generalizes where collapsing
on the derived name does not: an `application/octet-stream` ref paired with a
`report.pdf` ref collapses too. The winner is picked by a total order over
the candidate names (prefer one with an extension, then lexicographically
smallest), never by `HashMap` iteration order, which would make the rendered
tree nondeterministic.

## The fingerprint cache is host state, and deliberately not versioned

Every tree-scanning provider keeps a Unison-style cursor so a rescan can skip
hashing a file whose `(mtime, size, inode, dev)` has not moved. It lives in a
host-local cache rather than in the provider's versioned store:

- **It is host state.** An inode number means nothing on another machine, so
  a branch fetched from elsewhere carries a cursor that cannot match — and
  nothing records which host a cursor came from, so you cannot detect it.
- **Branching it is a category error.** The cursor describes the live
  filesystem, which has no history; rolling a branch back does not un-modify
  the files on disk.
- **It was half the store.** Measured at 100k entries, `files` + `file_stats`
  in one doltlite store is 291 B/row against 148 B/row for `files` alone,
  because the cursor re-stores the full path as its own primary key.

It is plain SQLite (via the `doltlite_engine=sqlite` URI parameter, the same
door `datalib_runs::store` uses), because losing a cache costs a rehash
rather than correctness, and it needs no commits, no history and no prolly
tree.

Keys are **absolute paths**, so one chain per host rather than per root. This
is the part Unison gets wrong: its `fpcache` is per replica *pair*, so
syncing one tree against two peers hashes the same bytes twice, and scanning
a directory tells you nothing about its parent.

## Event store: order is part of the contract

`load_latest_by_key` returns a `Vec` in **first-seen order** — the order
records appear in `created/`, with an `updated/` record replacing its
predecessor in place rather than moving it to the end. For an append-only
stream that is document order, which is what a synthesizer replaying a
listing endpoint has to reproduce.

It used to return a `HashMap`, and the ordering was silently whatever Rust's
per-process hash seed produced. That cost the notion fixture its
reproducibility: the replayed `/children` listing came back shuffled, the
downloader's BFS assigned different `blocks.page_order` values every run, and
the rendered markdown emitted the same blocks in a different order. The bug
is invisible *within* one process, because the seed is fixed per process — a
"render twice and compare" test passes against it.

Not a `BTreeMap` either: sorting by key is not document order and would
silently reshape the page.

**An unkeyable record is an error, not a skip.** Every `key_of` in this tree
is built from `unwrap_or_default()` over a few field lookups, so a record
whose fields don't match yields `""`. Tolerating that loses data twice — every
unkeyable record collapses onto one entry, and callers then skip the empty
key, so a whole entity stream reads as "no records". `datalib/backend/etl/providers/gitlab/tests/fixtures/gitlab_api`
spelled the project path `project_path` while every consumer had moved to
`project_full_path`; gitlab contributed zero rows for three months with no
failing test.

## Answering "did it change?" for a file-backed source

`fsscan` walks a tree and hashes only what the host cache cannot vouch for;
`file_checkpoint` stores what one feed already ingested. The split is the
point:

- the **fingerprint cache** is host-wide, shared and unversioned — expensive
  to compute, identical for every consumer, and a description of a machine
  rather than of a history;
- the **cursor** is what *this* source already ingested, and lives in that
  source's own store.

The cache cannot answer "since I last looked" alone, and that is not a gap to
close: it is shared, so another consumer's scan moves it. The question is only
well-posed relative to a particular looker.

```text
let scan    = fsscan::scan(cache, root, opts, accept).await?;
let changes = scan.changes_since(&load_cursor(pool, SCOPE).await?);
for f in changes.needs_reading() { …; record_file(&mut tx, SCOPE, f).await?; }
```

A scope namespaces cursor rows per `(provider, feed)`, so two feeds can claim
the same file without colliding. Stamping is per file and inside the caller's
transaction, so a crash partway through keeps what landed and re-reads only
the rest.

**Why content and not `(size, mtime)`.** The stat pair was chosen when hashing
every run was too expensive; the cache removed that cost. What the stat pair
got wrong was the *false re-ingest* — touching a file (`rsync` without `-t`, a
restore from backup, re-downloading the same export) re-read and re-parsed the
whole thing though not one byte had moved.

**What it does not fix**, because "content hash" invites the wrong assumption:
the cache still decides whether to re-hash from Unison's
`(mtime, size, inode, dev)` cursor, so an edit preserving all four is still
invisible. That was equally true before — the gain is that the assumption
lives in one place instead of once per provider.
`an_edit_preserving_the_whole_stat_is_still_invisible` pins it.

**Some files must not be read at all.** A macOS file evicted to iCloud is
"dataless": it has a size and an mtime, and reading one byte silently pulls
the whole thing back over the network. Only the stat can see that, so
`scan_with` takes a veto consulted after the stat and before any read. A
refused file is absent from the results and leaves the cache untouched, so
nothing later mistakes "we declined to look" for "we looked and it was empty".
