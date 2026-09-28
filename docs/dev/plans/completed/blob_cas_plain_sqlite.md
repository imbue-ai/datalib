# The blob CAS as a plain SQLite file

**Status: built (2026-09-28), kept as the record.** The reference is
`datalib/backend/etl/README.md` §"Blob CAS and per-provider edge
tables". Where the build differs from the text below: `RawStoreHandle`
gained `versioned_pools()` so `commit_all` leaves the CAS out; the
refusal prints the conversion command itself, with the real paths,
rather than pointing at a doc; and `introspect.rs` lists `blobs.sqlite`
by name, since it had only been counting `*.doltlite_db` files.

*The sizes were measured on 2026-09-28 on copies of two stores from
`~/datalib/stay_alive_1`, with the doltlite 0.50.5 shell; the stores
themselves were written by the pinned 0.50.12.*

A source that keeps attachment bytes has a second store beside its
entities: `<group>/ingest/blobs.doltlite_db`, one table, `cas_objects`,
keyed by the blake3 hash of the bytes (`etl/src/blob_cas.rs`). This makes
that file ordinary SQLite, created through doltlite's
`doltlite_engine=sqlite` URI parameter — the door the run store, the
supervisor store and the fingerprint cache already use.

The change is meant to be **subtractive**: it deletes the machinery that
exists only because the CAS is a doltlite store, and adds as little as it
can in its place.

## Why

**The CAS uses nothing doltlite adds.** A content-addressed table is its
own history: a hash is present or it is not, and a row is never updated.
Nothing diffs the CAS, nothing pins it (it is the one unpinned reader,
`open_cas_reader`), nothing reads its log, and the version an ingest step
reports is the *entities* store's head alone
(`datalib_step/src/ingest.rs::raw_store_version`). The 2026-09-17 audit
(§7) reached the same conclusion from the code; this adds the numbers.

**It costs two to five times the bytes it holds.** A commit rewrites every
leaf page its inserts touched, blake3 keys are random so a checkpoint's
inserts touch pages all over the tree, and every commit keeps its pages
reachable forever. So the file grows with the number of checkpoints, not
with the payload:

| store | blobs | payload | commits | `blobs.doltlite_db` | as plain SQLite | after `dolt_gc()` |
|---|---|---|---|---|---|---|
| slack | 697 | 355 MB | 31 | 760 MB (2.1×) | 356 MB | 757 MB |
| gmail | 27,008 | 940 MB | 149 | 4,389 MB (4.7×) | 968 MB | — |

`dolt_gc()` reclaims almost nothing, because the history is what keeps the
pages alive. Copying the rows into a plain file took 1.0 s for slack and
4.5 s for gmail.

## Decisions

1. **Plain SQLite is the only format**, not an option. An option would
   mean two open paths, two seal paths and every CAS test run twice, to
   keep a container whose features nothing reads.
2. **The `+blobs` reset goes.** Emptying the CAS from the app is too easy
   to do by accident for what it costs. A person who wants the bytes gone
   deletes the file by hand; a pruning mechanism (#284) comes later.
3. **Existing stores get two minor releases** of the refusal described
   under "Existing stores" below, and then it is deleted.

## What is deleted

**The CAS's part in sealing.** In a plain file the SQL `COMMIT` at the end
of `BlobCas::put_many` *is* the commit, and `flush_cas_edges` already calls
`put_many` before it writes the edge rows. So "blobs are committed before
the entity rows that name them" holds by construction and no longer needs
code to keep it:

- `SealState::seal` and `commit_final` (`etl/src/raw_store.rs`) lose their
  CAS commits. The session keeps the CAS pool only to close it, so
  `open_store_with_blobs` and its fifteen callers stay as they are.
- `RawStoreHandle::commit_all` stops committing a `BlobCas` field: the
  derive keeps listing it for `close_all` and leaves it out of the commit.
- `a_seal_commits_the_blob_store_too` is deleted.
- AGENTS.md's "use `commit_all` for a handle that carries a blob CAS"
  goes.

**The doltlite open.** `BlobCas::open` stops calling
`doltlite_raw::open_derived`, and with it the CAS no longer gets the
writer lock, the crash rescue, the schema commit, the writer branch or
`_datalib_meta`. `StoreKind::Blobs` is deleted: nothing reads the CAS's
meta, its one table has never changed shape, and a future change to it is
a rebuild. `store_meta/src/guard.rs` needs nothing, since its walk only
finds `*.doltlite_db` and the run store.

**The unpinned-reader exception.** `doltlite_raw::open_reader_unpinned`
is deleted, and so are the notes defending it in `blob_cas.rs`,
`doltlite_raw.rs`, `introspect.rs` and the etl README's §"A reader opens
read-only and pinned". A plain-SQLite reader sees committed transactions
and nothing else, so there is nothing to pin.

**The `+blobs` reset, end to end:**

- `dag/src/scheduler.rs::ResetTarget` loses its `part`; `--reset` takes
  step ids, and the usage text in `dag/src/bin/datalib_dag.rs` loses
  `[+blobs]`. `DATALIB_DAG_RESET` still carries `store`, so the step
  protocol keeps its shape.
- `datalib_step/src/reset.rs` loses its `(Ingest, "blobs")` arm.
- `http/src/lib.rs::ResetRequest` takes step ids only. A `+blobs` target
  then names a step that does not exist, which the existing refusal
  already covers.
- `ui/src/config/rowMenu.ts` loses "Reset (drop attachments)…", and
  `SourcesCard.ce.vue` loses the `blobs` flag on `resetTargets` and
  `resetRows` and the `reset_blobs` case; `rowMenu.test.ts` loses its
  cases.
- `tests/fixtures/tng_fuzz_test.py` (two places),
  `dag/tests/manual_e2e_live_sync_golden.rs`, `dag/src/subprocess.rs`'s
  reset test and `http/src/supervisor.rs`'s test reset plain `<id>`. A
  plain reset keeps the bytes and the refetch `INSERT OR IGNORE`s onto
  them. As far as the entities store can tell, that is still a download
  from scratch.
- Docs: `step_protocol.md` § Reset, `agent_user.md` § Resetting,
  `data_architecture_ingestion.md` (the reset paragraph, and "nothing
  deletes them outside a `+blobs` reset"), `chatgpt/INGEST.md`
  § Attachments, `plans/http_driven_e2e.md`.

**The blobs store's rows in the history card.** `http/src/history.rs`
lists `*.doltlite_db`, so they drop out with no code change. The test
fixtures in `history.rs`, `commitHistory.test.ts` and
`compareCommits.test.ts` lose their blobs rows.

## What replaces the open

One plain-SQLite open in `BlobCas::open` / `open_cas_reader`, the shape
`runs/src/store.rs::options` already has. Journal mode is `DELETE`, as the
other three plain stores set it: the plain engine answers `wal` when asked
for WAL and stays in rollback-journal mode, so asking would be a lie. The
pool has one connection with recycling off (`lint_repo.py` check 12).
`synchronous` is left at sqlx's default, not the run store's `OFF`: the
ordering above rests on a blob being on disk before its edge row is. To
verify: that sqlx 0.9's default is `FULL`.

The `file:…?doltlite_engine=sqlite` string is built by a copied
`connect_string` in `runs/src/store.rs`, `dag/src/supervisor/store.rs` and
`etl/src/fingerprint_cache.rs`. Rather than a fourth copy, it moves to one
function in `datalib_runtime`, which everything can link, and all four
call it. That is three copies deleted.

The file is renamed `blobs.sqlite` (`runtime/src/layout.rs::BLOBS_DB`,
`etl/src/raw_layout.rs` and its test), so its name stops claiming a format
it is not. `http/src/usage.rs` follows the constant. Add `blobs.sqlite` to
`http/src/watch.rs`'s quiet-files test.

`BlobCas`'s API does not change, and neither do `BlobBundle::load_many`,
`introspect.rs` or any render parser: they issue the same SQL against the
same table.

In rollback-journal mode a reader waits while a writer commits. The window
is short because `put_many` is handed bytes already in memory, so the
transaction lasts as long as the disk write, not the download. Measure
once, by hand, before merging: the longest reader stall during a large
bucket's `put_many` against the gmail copy. Nothing needs adding unless
that number is bad.

## Existing stores

No conversion code. A root with a `blobs.doltlite_db` would otherwise go
wrong silently: the new open creates an empty `blobs.sqlite`, the edge
rows still name every hash, the download's skip check treats a named hash
as fetched, and every attachment renders as "not yet fetched" for good.

So `BlobCas::open` refuses while `blobs.doltlite_db` sits beside it: one
`exists()` check and a message giving the two ways out.

- **Keep the bytes** (seconds), with the checkout's doltlite shell:

  ```sh
  datalib-doltlite <raw>/blobs.doltlite_db "
    ATTACH 'file:<raw>/blobs.sqlite?doltlite_engine=sqlite' AS out;
    CREATE TABLE out.cas_objects (blake3 TEXT PRIMARY KEY, byte_len INTEGER NOT NULL,
      content_type TEXT NULL, bytes BLOB NOT NULL, CHECK (length(blake3) = 64));
    INSERT INTO out.cas_objects SELECT blake3, byte_len, content_type, bytes FROM cas_objects;"
  rm <raw>/blobs.doltlite_db <raw>/blobs.doltlite_db.lock <raw>/.blobs.doltlite_db-lock
  ```

- **Refetch**: delete `blobs.doltlite_db` and reset the ingest step.

The refusal stops the source's sync with that message, and a normal sync
never reaches the render with the CAS missing. The one gap left: a
render-only pass on a root that hasn't been fixed yet renders attachments
as missing. Resetting the render once the ingest is fixed repairs it.

The refusal is deleted two minor releases later, the way
`datalib-migrate-config` keeps one rewrite at a time. The recipe then
lives on in `docs/dev/history.md`.

## Deleting the file by hand

The rule the `+blobs` reset encoded, written down instead of coded:
**delete `blobs.sqlite` and reset the ingest step, together.** Deleting
only the file leaves edge rows naming bytes that are gone, and the
download will not refetch them. That goes in `step_protocol.md` § Reset
and `agent_user.md`. There is no guard in code; pruning (#284) is what
should make hand-deletion unnecessary.

## Tests

Deleted: the seal test above, the `+blobs` cases, and any CAS case in
`doltlite_two_process_test`, since the CAS is no longer a doltlite store.

Added, two:

- `a_new_cas_is_plain_sqlite`: the file starts with `SQLite format 3\0`.
  Reuse the check `runs_tests/stock_sqlite_engine.rs` already makes.
- `a_cas_beside_a_doltlite_one_is_refused`: the message names both ways
  out. Deleted with the refusal.

The TNG fixture pipeline builds its roots from scratch, so it covers the
new format with no change.

## Order of work

One PR. The commit message says what breaks: the file is renamed; an
existing root's ingest refuses until the recipe or a refetch is run; and
`--reset …+blobs` and the menu entry are gone. It is a minor version bump.

Later, separately: pruning orphaned blobs (#284), and not keeping the
bytes twice (#29 — each render tree also copies its attachments into a
`blobs/` directory).
