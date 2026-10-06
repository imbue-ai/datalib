# `datalib-etl` — shared ingest machinery

Everything a provider needs but should not re-invent: the doltlite-backed
raw store (`doltlite_raw.rs`, `bulk.rs`), the blob CAS (`blob_cas.rs`), the
diff scan a render cursor drives (`doltlite_raw::scan_buckets`), the
local-tree walker and scanner (`fswalk.rs`, `fsscan.rs`), the
content-line grammar iCalendar and vCard share (`content_line.rs`:
unfolding, quoted parameters, TEXT unescaping, splitting a structured
value on its unescaped `;`), and the HTTP/auth plumbing (`http.rs`,
`latchkey.rs`). The render side's shared code is `render/`.

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
`ORDER BY rowid`: on a table keyed by text, doltlite's rowid is a hash
of the key and carries no order
([query plans](/docs/dev/doltlite.md#query-plans-and-indexes)).

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
only, not re-fetch churn — which is what makes the reset-then-resync
"did anything actually change?" assertion mean anything, and what
keeps a render from re-doing every document on every run: render
diffs the tables a document declared as inputs, and a `last_seen_at_utc`
on one of them is a change every time. That includes a scan's
`*_scan_meta` row, which every document of a file-backed source reads
its root from. A provider that opts out of the sidecar for scale
(`bulk_upsert_entity_in_tx`, fsindex's entry tables) keeps no stamp
at all on those rows. `content_tables_changed` is the check: ingest
the same input twice under two nows and it must name no table — each
file-backed provider's tests do exactly that.

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
know, and filling in `item_uuid` — see below), `grid_index` copies
every render store's into the index. The
pinned store is the complete truth about its source's problems at that
commit, so the copy is the sweep and there is nothing to diff. Stamps
travel with the row. The step then reports whole-store counts as
`problems{severity=…}` metrics, which the Manage screen reads. A
download reports them at every seal as well, right after it has written
the problems its run has found so far (`run_problems::collecting_sealed`),
and again when it ends, failed or not: a checkpoint's rows, its
problems and its count move together. Design
and surfaces: `docs/dev/plans/problem_visibility.md`.

A download's problem names only its raw entity (`<table>:<id>`): the
grid row it is about has an id minted under the source's id, which the
download never sees. So render asks each processor
(`RenderProcessor::item_of_entity`) which row that entity is or belongs
to — a Slack attachment belongs to its message — and fills in
`item_uuid` only when the render store holds that row. A filled
`item_uuid` therefore always opens something: the problems table links
the row's document, and the document's banner lists the problem. A
provider without the hook leaves `item_uuid` empty, and `scope_key` is
the only pointer.

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

Doltlite gives each *connection* its own active branch and HEAD, and
keeps each *branch's* uncommitted rows in the file, shared by every
connection on that branch in any process
([branches, HEAD and the working set](/docs/dev/doltlite.md#branches-head-and-the-working-set)).
The rules below follow from those two facts, and each is built into
`doltlite_raw` rather than left to convention.

### Every pool is size 1 and never recycled

A pool bigger than one lands statements on connections that disagree
about the tree ([locks and writers](/docs/dev/doltlite.md#locks-and-writers)).
So every open here pins `max_connections(1)` and disables
`idle_timeout` and `max_lifetime`: sqlx would otherwise retire the very
connection whose session state is load-bearing, and its replacement
starts on the default branch — an fsindex scan on a non-`main` branch
would silently start writing to `main` after 30 minutes and report
success.

Every *other* sqlx pool in the tree turns them off too, for a second
reason: either setting gives the pool a maintenance task, and in sqlx
0.9.0 that task can spin forever. It loops `for _ in
0..pool.num_idle()`, and `num_idle` is an unsigned counter that
`release` increments only after it has handed back the permit, so a
concurrent `acquire` can decrement it first and wrap it to
`usize::MAX` (sqlx issue
[#3645](https://github.com/launchbadge/sqlx/issues/3645), fixed after
0.9.0 by #4289). The task never leaves that poll, and dropping the tokio
runtime waits on its worker forever — a step that has reported its
outcome and will not exit. `lint_repo.py` check 12 refuses a pool
built any other way.

### One writer per file, by construction

Doltlite does not refuse a second writer; it only makes it wait its
turn. Two writers on one branch would then commit each other's rows.
So `open`, `open_derived` and `open_curated` (a store a person
writes by hand, which refuses a schema break like a raw store) are the
only ways to a handle that can commit, and each takes the file's writer lock — `flock(2)` on the
sibling `<store>.doltlite_db.lock`, `datalib_flock` — and gives it to
the connection, which holds it until it closes. A second writer on the
same file, in another process or in this one, is refused at open with
the holder named (`<program> (pid N)`).

The kernel releases the lock when the holder dies, so a killed run
leaves no stale claim; the next `open` finds its dirty rows and
**discards them**, so the store starts at its last commit
([a writer's open discards the working set](/docs/dev/doltlite.md#a-writers-open-discards-the-working-set)).
Those rows were never at a seal boundary — a row whose blobs are still
in flight, half a channel — and no reader was promised them: readers
pin commits. The delta since the last seal is refetched from the
cursor, which is what idempotency is for. The same rule holds on
Ctrl-C: nothing commits on the way out; the last seal stands.

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
its own scratch. `datalib-doltlite` is the raw shell and takes no lock
of *ours*: run it `-readonly` against a store a sync may be writing.
Doltlite's own lock sidecar, `.<name>.doltlite_db-lock`, is a different
file and only makes writes take turns.

### A writer works on its own branch and publishes when it seals

A reader on the writer's branch would see its uncommitted batch,
including tables the writer has created and not committed. So a writer
does not work on `main`. `connect_pool` puts every connection on
`WRITER_BRANCH` (`datalib_writer`) in `after_connect`, not once at
open: sqlx replaces a connection that breaks, and a replacement starting
on the default branch would put half-finished work where every reader
can see it.

It gets there with **`dolt_connect_branch`, never `dolt_checkout`**,
because `dolt_checkout` writes a few hundred bytes on every call and
`dolt_connect_branch` writes nothing, so an untouched store stays
byte-identical (`reopening_an_untouched_store_does_not_grow_it`).
`dolt_checkout('-b', …)` still creates the branch, once per file.

**Then it asks the connection back which branch it is on, and a wrong
answer fails the open.** A selection that quietly did nothing is the one
failure this construction cannot survive: the writer stays on the
default branch and every row it writes is visible to every reader the
moment it lands rather than when it is sealed — and nothing else would
notice, because the rows are all there and the commits all happen.
`a_writers_pool_is_on_the_writer_branch` is the guard.

**The seal is the commit *and* its publication.** `commit_run` does
`dolt_commit` on the branch and then `dolt_branch('-f', 'main', …)`.
A force-move rather than a `dolt_merge`, because one writer per file
means `main` only ever moves there: the branch is always a descendant,
so a merge would be a fast-forward anyway. That force-move is also why a branch per writer is no way
around the one-writer rule: it is right only while one process moves
`main`.

A process that only *moves a ref* is a writer too — a reader that keeps
a branch of its own and fast-forwards it with `dolt_merge('main')`
writes to the file on every refresh and waits behind the real writer's
transactions ([locks and writers](/docs/dev/doltlite.md#locks-and-writers)).
Read a commit instead (below).

A crash between the commit and the publication leaves the branch ahead
of `main`; that commit is a seal the last run meant to make, so the
next `open` finishes it.

Two things deliberately stay off this path. `fsindex` keeps one branch
per scan root and publishes none of them — the reserved branch name is
the signal, so a connection on any other branch seals without touching
`main`. And the app stores in `core` open plain sqlx pools
(`core/src/store.rs`), so they never enter the scheme at all.

`publish_to_main` is public for the one shape `commit_run` cannot
express: a commit needing an argument of its own, such as the `--date`
the yolink fixture generator pins so `dolt_log()` does not report build
time. That caller commits by hand and then publishes — a commit nobody
can see is not a seal.

**A reader lands on the file's stored default branch**, which is `main`
because seeding set it there and nothing moves it. That is the rule;
"a connection starts on `main`" is only its consequence. Nothing here
calls `dolt_default_branch`, and if anything did, every reader would
silently start on a writer's branch.
`a_fresh_connection_starts_on_main` asserts the default itself for that
reason.

### A download takes the store; it never opens one

Every provider's `FetchOptions` carries `pub db: RawDb` — a live handle,
not a path and not an `Option`. **Whoever opens a store closes it**, and
for a download that is always the caller: the step's processor, the
provider's `*_download` binary, or the test. `fetch` borrows it for the
run and returns. A `fetch` that opened its own store while the caller
held one would now be refused at open rather than failing the caller's
commit later.

### A reader opens read-only and pinned

`open_reader(path, commit)` is the read path. It resolves the commit —
the one the caller names (the render driver's) or HEAD — and opens
`<store>@<hash>` read-only: a detached connection on which every plain
table name reads that commit and nothing a writer has in flight, the
schema is that commit's, and "a reader must not write" is the engine's
rule (`attempt to write a readonly database`). It never creates the file
and takes no lock. It hands back a `Reader`, or `None` when the store
has nothing readable committed, which the caller must decide about (a
consumer does nothing that pass) rather than fall through to the
working set. **A commit holding no table counts as nothing committed.**
That is the shape an owner's `open` leaves behind between creating the
file and its first commit, and under streaming a consumer opens there
often enough to matter. The `pin.rs` `Pin` refuses `HEAD` by name.

This is the detached open, one of
[three ways to read one commit](/docs/dev/doltlite.md#three-ways-to-read-one-commit).
The one long-lived reader, the search applet, holds a read transaction
instead (`DoltRepo::pinned`), and moves to the newest `main` with
`COMMIT; BEGIN` rather than reopening.

**The allowlist.** The two-process test measures what a reader may do
beside a live writer — `dolt_hashof`, `sqlite_master`,
reads through `dolt_at_` modules,
`dolt_diff_*`, `dolt_log()`, `dolt_commit_ancestors`,
`dolt_diff_summary`, `dolt_diff_stat`, `dolt_status`, a `COUNT(*)` per
table, `BEGIN`/`COMMIT` around plain reads (the held read transaction),
`dolt_branches`, and a read-only open of `<file>@<hash>` with a
`COUNT(*)` and a `_datalib_meta` read on it — and that list is the
allowlist. Any other
statement a reader adds is presumed guilty until
`doltlite_two_process_test` has run with it. Looking like a read is not
enough: a read-only `dolt_status` once failed the writer's commit and
lost its rows
([what a read-only connection may do](/docs/dev/doltlite.md#what-a-read-only-connection-may-do)),
and `a_reader_asking_dolt_status_never_makes_the_writers_commit_fail`
is what now says it is safe.

A reader that holds its connection across another process's commits
has two engine facts to respect, both in
[what a read-only connection may do](/docs/dev/doltlite.md#what-a-read-only-connection-may-do):
a bare `dolt_hashof('HEAD')` answers from the session's last view, so
`datalib_pin::head` reads `sqlite_master` first; and `pragma_module_list`
is no census of what a commit holds, so ask by reading.

Open the store once per pass — a stage that needs to load rows, run a
`dolt_diff` scan and probe for ids does all three on one pool — and
`close().await` before the next open, on the error path too. And never
run a store call on a runtime you are about to drop: sqlx returns a
checked-out connection from a task spawned at drop, and a per-call
`Runtime::new().block_on(..)` dies before that task runs, leaving the
next open a second handle on the same file (`indexed_markdown::blocking`
keeps one process-wide runtime for the no-runtime case).


## What a write costs: the transaction is the unit, and the key decides the size

A write rewrites every page its keys fall in, a SQL transaction writes
each page once at `COMMIT`, and a commit keeps what its transaction
wrote for good
([what a write costs](/docs/dev/doltlite.md#what-a-write-costs),
[disk space and `dolt_gc`](/docs/dev/doltlite.md#disk-space-and-dolt_gc)).
What that means here:

- **Batch writes in one transaction.** Every store already does — a
  render store's transaction is one checkpoint interval, the grid
  index's is the whole run, a SQLite mirror's is one table — so within
  one run the order rows arrive in does not matter.
- **Key for adjacency where the key is ours.** Rows a run writes
  together should sort together: `(device_id, ts_ms)`,
  `"{metric}#{date}"`, and the time-prefixed ids `datalib_id` mints (a
  message's `grid_rows.uuid` starts with its `created_at`, so a sync's
  new rows land at the tree's right edge). Random keys — uuidv4s,
  uuidv5s, content hashes — touch one page per row, and every run's
  commit keeps those pages. See
  `docs/dev/data_architecture_ingestion_practices.md` § "Key a table
  for what one run writes together" and `docs/dev/entity_ids.md`
  § "The layout".
- **Commit cadence is free until something runs `dolt_gc()`**, and only
  `sqlite_mirror` and `fsindex` do today. Without gc every store keeps
  every transaction's pages regardless.
- **A squash deletes commit hashes.** Folding old commits into one
  (`dolt_reset('--soft', <base>)` then `dolt_commit`) lets the next gc
  reclaim what only they reached, but a consumer whose cursor named one
  falls back to a full pass, and a reader pinned at one loses it. Squash
  only commits older than every consumer's cursor, with the writer lock
  held.

## Schema self-healing: additive changes land, anything else refuses

`open` plans every declared table against the file before it touches
anything (`plan_table_schema`), then applies the plans, then the indexes.
A table is compared on its whole shape — each column's name, declared
type, nullability, default, place in the primary key and whether it is
generated — not on its column names.

- **Absent**: created. If the store already had other tables, its
  cursors are cleared (below), because the new table is as empty as a
  recreated one.
- **Additive** — every difference is a declared column the file lacks,
  and each can go through `ALTER TABLE … ADD COLUMN` (no key, no
  `NOT NULL` without a default, no STORED generated column; a VIRTUAL
  generated column is fine): added, with the clause verbatim from the
  DDL. Rows and cursors are kept.
- **Anything else** — a column removed, renamed or retyped, a key or a
  `NOT NULL` changed: the open **refuses** (`SchemaBreak`, naming every
  such table and what differs) and the file is exactly as it was. A raw
  store's rows may be the only copy — an export whose source is gone, a
  window upstream no longer serves — so nothing is dropped on the way
  in. The refusal names the two ways out: a rung on the provider's
  migration ladder (below) or `datalib-dag --reset <source>/ingest`,
  which empties every table without needing the DDL; an empty table
  has nothing to lose, so the next open rebuilds it to the new shape and
  the sync refills it. Derived stores (`open_derived`:
  render, index, CAS) always rebuild, since every row is a function of
  another store.

### The migration ladder

A non-additive change that has to reach existing stores is a rung
(`datalib_store_meta::Migration`) on the provider's ladder, passed to
`open_migrating(path, ddl, LADDER)` instead of `open`. Rungs are
numbered densely from 1; `_datalib_meta.schema_version` is how many
have run; an open runs the rest in order before it compares the DDL,
each in one transaction with the version bump and then its own commit
(`migrate v<n>: <name>`), so a crash between two rungs is resumed by
the next open. A store above the ladder's top is refused: a newer
build migrated it. The DDL still has to match once the rungs have run
— a rung that leaves the shape wrong is a bug in the rung, and the
open refuses on it like any other break.

```rust
pub const LADDER: &[Migration] = &[Migration {
    version: 1,
    name: "messages.body becomes messages.text",
    apply: |conn| Box::pin(async move {
        sqlx::query("ALTER TABLE messages RENAME COLUMN body TO text")
            .execute(&mut *conn).await?;
        Ok(())
    }),
}];
```

A rung may read the old shape through `dolt_at_<table>('HEAD')` and
write the new one, and it clears a cursor table itself (`DELETE FROM
sync_scope_state`) when the change alters what the cursor means. The
test for a rung is always the same shape: build the store at
`version - 1` by hand, open with the ladder, assert the rows —
`app_store.rs`'s `a_store_from_before_the_utc_columns_is_migrated_on_open`
is the template. A provider's `RawDb` takes its ladder as the third
argument of `raw_db!` (email's `schema_raw::LADDER`); contacts, which
opens by hand, passes it to `open_migrating`. The contacts app's store
(`datalib_contacts::LADDER`) passes its own to `open_curated`: a rung
there is how a change to the handle rules reaches the links a person
made, and its test fails until one is added. The app stores' rungs
(`core/src/app_store_migrate.rs`) are applied by `AppStore::open`
itself rather than through `open_migrating`.

A rung that adds a table the download fills from upstream creates the
table itself and fills it from what the store already holds (contacts'
`contact_group_members`). Left to the DDL, the new table would clear
the store-wide cursors, but not a per-row one like an address book's
sync-token, and the table would stay empty until upstream changed.

The two-pass order is load-bearing. An index over a column introduced by
a later schema change cannot be created against an older store, so a
single pass fails with `no such column` before the column it needs is
added — leaving every older store unopenable.

A cursor is only valid under the schema that set it. A recreated or
newly created table is empty, and a cursor that says "read through
here" would let the next run resume past rows the table does not have,
and the table would stay empty until upstream changed, with nothing
saying why. So either clears every store-wide cursor
(`sync_scope_state`, `sync_scope_config`, `ingested_files`) and logs
that it did; the next run walks from the start, and the tables that kept
their rows absorb it as no-op upserts. Per-row cursors — a device
row's `last_ts_ms`, an address book's `ctag` — live on the table that
holds them and go with it.

Not checked: a table in the file that no DDL declares. The mirror
engine writes exactly such tables, so "undeclared" is normal in a store
and cannot be a warning. A renamed table is therefore an orphan plus a
new empty table — and the new table clears the cursors, which is what
keeps the rename from being silent.

`declared_columns` learns a DDL's columns by parsing it into a probe table in
an **in-memory** database. Never against the store being opened: a
create+drop there still appends chunks to the file
([what a write costs](/docs/dev/doltlite.md#what-a-write-costs)), so
every `open` would cost bytes whether or not anything was ingested.

## `_datalib_meta`: which build wrote this store

`open` writes six rows into `_datalib_meta` before the schema commit:
`datalib_version`, `git_hash`, `doltlite_version`, `schema_hash`
(`recorded_shape`: blake3 over `_datalib_meta`'s DDL and the DDL the
store was opened with, leaving out the lookup indexes
`open_derived_indexed` was handed, which change no row), `schema_version` (the
migration ladder position, `0` until there is a ladder) and
`store_kind`. Only a row whose value moved is rewritten, so an
unchanged store costs no commit, and a schema commit that did move
one is titled `schema: apply DDL (datalib <version>)`. The table is in
`SHARED_TABLES`, so it is neither mirrored nor diffed nor counted.
`datalib_store_meta::read` is how anyone asks; `None` means the store
predates the table. A reader of a derived store asks it once per pass
rather than probing for each table: `grid_index` compares a render
store's `schema_hash` with `indexed_markdown::schema_hash`, the same
value the scheduler fingerprints, and leaves a store in any other shape
as the index had it, with a warning. `docs/dev/plans/completed/schema_migrations.md`
is the record of the program this was the first step of.

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

`BulkUpsertable` itself is defined in `datalib_table` (`backend/table/`, a
crate of its own with only `sqlx` beneath it) and re-exported as
`datalib_etl::bulk::BulkUpsertable`, because the render-schema structs
implement it too and this crate must not reach `datalib_schema`.

## Blob CAS and per-provider edge tables

A source that keeps attachment bytes has two files in its raw directory:
`entities.doltlite_db` (entities plus that provider's CAS edge table) and
`blobs.sqlite` (pure CAS). Bytes are keyed by their blake3
hash and stored exactly once in `cas_objects`; each provider declares its
own edge table, `(id, <owning>, <ref>, blake3)`, with `#[derive(CasEdgeRow)]`
([`macros/README.md`](macros/README.md)).

The bundle is the common vocabulary at both ends. Download adds bytes as they
arrive and drains the bundle at end of bucket; parse loads every document's
bundle at once with `BlobBundle::load_many`; render then consumes an
already-loaded bag of bytes — no SQL, no `block_in_place`, no dyn blob reader.
Parse reads the edge table in one query for every document it loads,
not one per document.

**The CAS is plain SQLite, not doltlite**, created through the
`doltlite_engine=sqlite` URI parameter like the run store. A
content-addressed table is its own history — a hash is present or it is
not, and a row never changes — so nothing ever read its doltlite log,
diffs or pins, while every checkpoint's pages stayed in the file: on
two real stores a doltlite CAS was 2.1× and 4.7× its payload
(`docs/dev/plans/completed/blob_cas_plain_sqlite.md`). So the one-writer
lock, the writer branch, the seal and the pin do not apply here.

**Bytes commit before the rows that name them, by construction.**
`BlobCas::put_many` commits its own transaction, and `flush_cas_edges`
calls it before it writes the edge rows. A reader pinned at any entities
commit therefore finds every blob that commit names, and a reader of the
CAS sees committed transactions only, so there is nothing to pin. The
connection runs `synchronous=FULL` so that order survives a power cut.
`RunCtx::run_store` is handed the CAS only so the session closes it,
whichever way the run ends.

**Nothing resets the CAS.** Delete `blobs.sqlite` and reset the ingest
step together; deleting the file alone leaves edge rows naming bytes
that are gone, and the download will not refetch them. For the same
reason `BlobCas::open` converts a `blobs.doltlite_db` an older build
left: it copies every blob into a temporary plain file, checks the row
count and byte total, renames it into place and deletes the old store.
A crash part-way leaves the old store whole and the next open starts
over.

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
- **It was half the store.** Measured at 100k entries on doltlite 0.50.13,
  `files` + `file_stats` in one store is 322 B/row against 171 B/row for
  `files` alone,
  because the cursor re-stores the full path as its own primary key
  (`providers/fsindex/src/ingest/STORAGE_NOTES.md` has the table).

It is plain SQLite (via the `doltlite_engine=sqlite` URI parameter, the same
door `datalib_runs::store` uses), because losing a cache costs a rehash
rather than correctness, and it needs no commits, no history and no prolly
tree.

Keys are **absolute paths**, so one chain per host rather than per root. This
is the part Unison gets wrong: its `fpcache` is per replica *pair*, so
syncing one tree against two peers hashes the same bytes twice, and scanning
a directory tells you nothing about its parent.

It lives at `$DATALIB_CACHE_DIR/fingerprints.sqlite`, else under
`$XDG_CACHE_HOME/datalib`, else `~/Library/Caches/datalib` (macOS) or
`~/.cache/datalib`. **A test names its own `DATALIB_CACHE_DIR`**, and under
a bazel test (`TEST_TMPDIR` set, which the processes a test spawns inherit)
`default_cache_path` refuses to fall through to the host's. A test scans a
sandbox path that is gone by the next run, so in the host cache every one
stays as a dead row: one mac measured 191k of them, 94% of its cache. The
fixture pipeline (`tests/fixtures/run_sync_pipeline.py`) keeps its cache
inside the data root it builds.

## Event store: order is part of the contract

`load_latest_by_key` returns a `Vec` in **first-seen order** — the order
records appear in `created/`, with an `updated/` record replacing its
predecessor in place rather than moving it to the end. For an append-only
stream that is document order, which is what a synthesizer replaying a
listing endpoint has to reproduce: notion's replayed `/children` listing
decides `blocks.page_order`, and so the order of the rendered page.

Not a `HashMap`: its order is Rust's per-process hash seed, so a replay
comes back shuffled — and invisibly, because the seed is fixed within one
process and a "render twice and compare" test passes against it. Not a
`BTreeMap` either: sorting by key is not document order and would silently
reshape the page.

**An unkeyable record is an error, not a skip.** Every `key_of` in this tree
is built from `unwrap_or_default()` over a few field lookups, so a record
whose fields don't match yields `""`. Tolerating that loses data twice — every
unkeyable record collapses onto one entry, and callers then skip the empty
key, so a whole entity stream reads as "no records". A fixture that spells a
field the old way (`project_path` for gitlab's `project_full_path`) would
otherwise contribute zero rows with no failing test.

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

**A file that is gone takes its records with it.** `changes.removed` names
every path the cursor has and the scan does not. A source whose rows are keyed
by path (a `.vcf` file is one address book, an `.ics` file one calendar) reads
`needs_reading_by_path()`, which counts a moved file as new at its new path,
then deletes the rows of each path in `gone_by_path(&read)` and calls
`forget_file` in the same transaction. The path is removed from the cursor
only then, so a crash in between just retries. `gone_by_path` is empty
whenever the walk reported an error, because a folder that failed to list
looks the same as one whose files were deleted. Report `scan.walk_problems()`
to the run's `RunProblems`, so the skipped deletions show on
the Manage row. Key rows against `scan.given_resolved`, not the configured
path: the scan's paths are resolved, and stripping an unresolved prefix
fails whenever a symlink is in the way.

A source keyed by *content* (mbox, SMS backups, Takeout Voice) cannot map
a path to rows, and its files overlap: two exports hold one message. When
`changes.may_have_dropped_records()` (a file removed or rewritten, after a
clean walk) it reads every file, and a run that read every file without a
failure prunes what it did not see (`prune::prune_scope`, then
`prune::delete_owned` for CAS edges) and `forget_files` the removed paths.
A failed read holds the prune back and reports
`Scan::deletions_held_back`. The read costs a pass over every file, but only
on the run where the input shrank.

A feed whose one file is its whole table (Takeout's Maps reviews, YouTube
subscriptions, …) goes through `file_checkpoint::ingest_snapshot`: a
changed file is upserted and the table pruned to what it lists, in one
transaction. A file the parser cannot read as a whole list — no list at
all, or entries none of which it could read — is an error: nothing is
stored or deleted, and the file is not marked read. A file missing from
the scan deletes nothing either: for an export,
a product left out of the request looks exactly like that. Takeout's
folder feeds hold to the same rule one level up (`product_exported`).

**A file as the root is that file.** `scan` of a file walks only that
file's folder, one level deep, and keeps that one name: its `rel` is the
bare file name and its cache key is the one a scan of the folder would
use. A caller that wants a few files side by side — a SQLite file and its
`-wal` — scans the folder with `max_depth: Some(1)` and names them in
`accept`; no folder beside them is opened. The cache read is not
depth-limited: `load_under` still fetches every cached entry under the
folder, which is a database read, not a walk.

**Why content and not `(size, mtime)`.** A cursor on the stat pair
re-ingests a file that was only *touched* (`rsync` without `-t`, a restore
from backup, re-downloading the same export), re-reading and re-parsing the
whole thing though not one byte moved. The cache makes hashing cheap enough
that the cursor can be the content.

**What it does not fix**, because "content hash" invites the wrong assumption:
the cache still decides whether to re-hash from Unison's
`(mtime, size, inode, dev)` cursor, so an edit preserving all four is still
invisible — in one place rather than once per provider.
`an_edit_preserving_the_whole_stat_is_still_invisible` pins it.

**Some files must not be read at all.** A macOS file evicted to iCloud is
"dataless": it has a size and an mtime, and reading one byte silently pulls
the whole thing back over the network. Only the stat can see that, so
`scan_with` takes a veto consulted after the stat and before any read. A
refused file is absent from `files` and leaves the cache untouched, so
nothing later mistakes "we declined to look" for "we looked and it was empty".
It is listed in `present_unread`, as is a file over `max_bytes`: both are
there, so a source keyed by path keeps their rows.
