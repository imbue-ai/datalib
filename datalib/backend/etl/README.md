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

`payload` is content and stays on the object table. `fetched_at`,
`attempt_count`, `last_attempt_at`, `last_error` and `volatile_payload` go
in `<table>_bookkeeping` (see `bookkeeping_ddl_for`).

The split keeps `dolt diff` over the data tables reflecting upstream change
only, not re-fetch churn — which is what makes the `--reset-and-redownload`
"did anything actually change?" assertion mean anything.

Every object row gets a sidecar row in the same transaction; use
`ensure_object_row` to seed both.

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

## Connection pools against a doltlite file are always size 1

Doltlite's HEAD pointer, working set and active branch are **per
connection**. A pool bigger than one lands statements on connections that
disagree about the tree, which shows up as a `dolt_commit` whose hash never
appears in `dolt_log`, or as `commit conflict: another connection committed
to this branch`. The dolt maintainers confirm the same is true of Dolt
itself and recommend the same fix.

`open` pins `max_connections(1)` and also disables `idle_timeout` and
`max_lifetime`. The timeouts matter for the same reason: sqlx would retire
the very connection whose session state is load-bearing, and its replacement
starts on `main` with a clean working set. An fsindex scan on a non-`main`
branch would silently start writing to `main` after 30 minutes and report
success — and multi-million-entry scans reach that window.

Any other code opening a `SqlitePool` against a `.doltlite_db` must do the
same — and one pool, not two. Size 1 is necessary, not sufficient. A second
pool shares the first's working set, so an `-Am` commit through either
sweeps up whatever the other has in flight; and while the two are actually
mid-write they contend for a lock `dolt_commit` takes without waiting, so
one of them fails with `commit conflict: another connection committed to
this branch`. The message names a commit that need not have happened; read
it as "someone else is writing this store right now".

An idle peer costs neither of those —
`//datalib/backend/etl:doltlite_two_process_test` measures a second
read-write open landing in ~2ms with both pools then committing — which is
what makes a second pool a timing bug rather than an immediate one. And a
pool you dropped is not yet a pool that is gone: sqlx closes its
connections on a background task, so a store reopened right after the
previous handle went out of scope can still find the old connection
there.

So there are two ways to be right, and dropping a handle is neither. Hold
one handle for as long as the store is in use. Or, where a fresh connection
is the point — proving a cursor survived the pool that wrote it, or
mirroring a binary that opens the store per run — `await` a `close()`
before the next `open`. Every `RawDb` has one; it closes the blob CAS
alongside the entity pool, which the older `db.pool().clone()` /
`pool.close()` idiom silently left open.

### A download takes the store; it never opens one

Every provider's `FetchOptions` carries `pub db: RawDb` — a live handle,
not a path and not an `Option`. **Whoever opens a store closes it**, and
for a download that is always the caller: the step's processor, the
provider's `*_download` binary, or the test. `fetch` borrows it for the
run and returns.

The rule is there because the alternative was tried. `db` used to be
`Option<RawDb>`, and `fetch` opened its own store when the caller passed
`None`. That pool was never closed, so a caller which then read the store
back — every download test does — had two live connections on one file,
and one of the two `dolt_commit`s could fail. Because sqlx closes
connections on a background task, whether the two actually collided came
down to timing: green on a quiet laptop, intermittently red on a loaded CI
runner, always at `commit schema after DDL` inside the second `open`.

`scripts/lint_repo.py`'s check 6 keeps the field non-optional, and
`two_live_pools_on_one_store_break_each_others_commits` in
`doltlite_raw.rs` pins the underlying behavior: two pools committing in
lockstep on one store, and one of them gets `commit conflict`.

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
dropped rows in history.

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
door `datalib_progress::bus` uses), because losing a cache costs a rehash
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
key, so a whole entity stream reads as "no records". `tests/fixtures/gitlab_api`
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
