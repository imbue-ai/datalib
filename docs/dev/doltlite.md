# `doltlite` — poking at a `.doltlite_db` file from the shell

> **GUI option:** For a macOS sqlite-browser build patched to load
> doltlite, grab a release from
> <https://github.com/thadd3us/sqlitebrowser/releases>. The CLI recipes
> below all still apply; the GUI is just nicer for exploring schema and
> running ad-hoc SELECTs.


Our raw ETL captures (under `<data_root>/<name>/raw/`) and the per-mirror
backend index (`<data_root>/unified_index/grid_index/db.doltlite_db`) are
[doltlite](https://github.com/dolthub/doltlite) databases: SQLite with
content-addressed prolly-tree storage and a `git`-shaped commit history
exposed through SQL. The bazel build statically links doltlite into our
Rust binaries (see `third-party/doltlite/README.md`), but the
**`doltlite` CLI** (a SQLite shell with the dolt extensions baked in)
is the everyday tool for poking at one of these files by hand.

**Where to get it.** A release install already has it: the tarball
`scripts/install.sh` unpacks contains **`datalib-doltlite`**, so it
lands in `~/.local/bin` next to `datalib-dag`. In the docker image it
is on `PATH` under the plain name `doltlite`. From a checkout, build
it — `bazelisk build //third-party/doltlite:doltlite` — and prefer that
over any host `doltlite` you may also have, because the Bazel target is
version-locked to `MODULE.bazel`'s pin and so cannot disagree with what
the pipeline wrote.

Whichever you run, the argv is identical to `sqlite3`:
`datalib-doltlite [OPTIONS] DBFILE [SQL...]`. Dot-commands, `-json`,
`-csv`, `-box` and the interactive REPL all work, plus the `dolt_*` SQL
surface. The recipes below are written with the `doltlite` name for
brevity; type `datalib-doltlite` if that is what you installed.

> **Always pass `-readonly`** when you're just exploring. A writable
> CLI session does **not** get a branch of its own — it lands on the
> file's default branch, `main`, which is the one every reader reads
> and the one a sync fast-forwards when it seals. Its `dolt_commit -Am`
> sweeps up whatever is in that branch's working set, and it contends
> with a live run for the file: `commit conflict` and `database is
> locked`, on both sides. The writer lock in
> `datalib_etl::doltlite_raw` keeps a second *Rust* writer out; nothing
> keeps the CLI out but you.

## Getting the data out: export to plain SQLite

A `.doltlite_db` is not a SQLite *file*. It is a prolly-tree store, and
stock `sqlite3` opens one with `Error: file is not a database`. That is
a fact about the on-disk format, not about lock-in — the shell above
dumps any store to ordinary SQL, which stock SQLite loads:

```sh
datalib-doltlite -readonly unified_index/grid_index/db.doltlite_db .dump \
  | sqlite3 grid.sqlite
```

That is the whole export. Measured against a real 16 MB grid store on
2026-09-05: 15 MB of SQL, well under a second each way, and
`grid_rows` / `markdowns` / `edges` arrive with their schemas, primary
keys and indexes intact. It works for the raw stores too — BLOB columns
come through as hex literals, so a `blobs.doltlite_db` round-trips its
attachment bytes.

What crosses and what doesn't:

- **Crosses:** every user table, its schema, its indexes, its rows — the
  current state of the checked-out branch.
- **Doesn't:** the version history. `.dump` emits the working set, so
  the commit chain that lets you ask what upstream deleted last month
  stays behind in the `.doltlite_db`. The `dolt_*` tables are virtual
  and simply aren't in the dump, which is what you want — a
  `dolt_log()` frozen into a plain SQLite file would be a lie.

Keep the original if you care about history; the export is a snapshot
for tools that speak only SQLite.

Two variants worth knowing:

- **One table:** `.dump grid_rows` (the dot-command takes a table name,
  or a `LIKE` pattern).
- **No pipe, one session:** doltlite's `doltlite_engine=sqlite` URI
  parameter selects the stock B-tree engine for a database, so an
  `ATTACH` can *write* a real SQLite file from inside the shell:

  ```sh
  datalib-doltlite copy.doltlite_db \
    "ATTACH 'file:grid.sqlite?doltlite_engine=sqlite' AS out;
     CREATE TABLE out.grid_rows AS SELECT * FROM main.grid_rows;"
  ```

  Verified 2026-09-05; the result is a file `sqlite3` opens directly.
  Two caveats, both measured the same day. `-readonly` is missing from
  that command on purpose — under it the `ATTACH` cannot create the
  output and the next line fails with `unknown database out` — so run
  this against a **copy** of the store rather than adding a second
  writer to a live one. And `CREATE TABLE … AS SELECT` copies rows and
  column types but not primary keys or indexes, which leaves `.dump`
  the higher-fidelity route.

  `VACUUM INTO` looks like it should be the one-step version and is
  not: it treats its argument as a literal filename rather than a URI,
  so it writes a file *named* `file:out.sqlite?doltlite_engine=sqlite`
  in doltlite's own format.

## Recipes

### `git log` for the current branch

```sh
doltlite -readonly slack/ingest/entities.doltlite_db \
  "SELECT commit_hash, committer, date, message
     FROM dolt_log()
    ORDER BY date DESC
    LIMIT 20;"
```

The app shows the same thing: right-click a row on the Manage screen
and pick **Show commit history**, or ask `GET
/api/pipeline/history?tree=<group or step id>` for it as JSON — every
store under that tree, each commit with its per-table row counts and
what the commit added, deleted and modified (`datalib/backend/history/`).

Each row is one `dolt_commit()` call from the ETL — e.g.
`download slack: msgs=29 replies=51 media[...]` for a successful Slack
sync, or `checkpoint slack: entities` for a seal partway through one.
A cancelled download stops at its next consistent point and commits
there under the same `download …` message; whatever a *killed* run
wrote after its last commit is discarded by the next writer's `open`.
`dolt_log()` walks back from `HEAD` on the active branch
(use `active_branch()` to check which one that is).

### Which branch is checked out / what branches exist

```sh
doltlite -readonly slack/ingest/entities.doltlite_db "SELECT active_branch();"
doltlite -readonly slack/ingest/entities.doltlite_db "SELECT * FROM dolt_branches;"
```

Every connection lands on the file's stored default branch, `main`,
whatever a previous one was on — a connection's branch is per
connection and is not written into the file. So `dolt_log()` and a
plain `SELECT` show you `main`: sealed state, not whatever a run has in
flight on `datalib_writer` (`etl/README.md` §"A writer works on its own
branch" for why that branch exists).

**To look at any other branch you have to give up `-readonly`**:
`dolt_checkout` on a non-default branch needs a writable connection and
otherwise fails with `checkout failed`. Since a writable open is the
thing this page keeps telling you not to do against a live store, copy
the file first and check the branch out in the copy.

### Uncommitted changes (`git status`)

```sh
doltlite -readonly slack/ingest/entities.doltlite_db "SELECT * FROM dolt_status;"
```

Columns are `(table_name, staged, status)`. A non-empty result used to
mean "a writer died between its last seal and its next one." The next
writer's `doltlite_raw::open` discards that working set and starts from
HEAD — see [Operational notes](#operational-notes). A non-empty
`dolt_status` against a file you opened with the CLI just means an ETL
run is mid-flight (or recently was); what you see there will be thrown
away, not committed.

With `-readonly` it is safe against a store a sync is writing right
now: `a_reader_asking_dolt_status_never_makes_the_writers_commit_fail`
in `doltlite_two_process_test` holds it to that. Before doltlite 0.50.10
it failed the writer's `dolt_commit` and lost the rows behind it (#400,
dolthub/doltlite#2832): check `doltlite --version` before pointing an
older shell at a live store.

### What changed between two commits

**Per-table summary** — which tables differ, and is it a data or schema change:

```sh
doltlite -readonly slack/ingest/entities.doltlite_db \
  "SELECT from_table_name, to_table_name, diff_type, data_change, schema_change
     FROM dolt_diff_summary
    WHERE from_ref = 'HEAD^1' AND to_ref = 'HEAD';"
```

`HEAD`, `HEAD^1`, `HEAD~N`, branch names, and full commit hashes all
work as refs. The cheapest "git status between commits" view.

**Per-table row counts.** Two forms, and the vtab is usually the one
you want — it covers every table that changed in one query:

```sh
doltlite -readonly slack/ingest/entities.doltlite_db \
  "SELECT table_name, rows_added, rows_deleted, rows_modified
     FROM dolt_diff_stat
    WHERE from_ref = 'HEAD^1' AND to_ref = 'HEAD';"
```

The table-valued form answers for one named table:

```sh
doltlite -readonly slack/ingest/entities.doltlite_db \
  "SELECT * FROM dolt_diff_stat('HEAD^1', 'HEAD', 'messages');"
```

Returns `(table_name, rows_unmodified, rows_added, rows_deleted, rows_modified,
cells_added, cells_deleted, cells_modified, old_row_count, new_row_count,
old_cell_count, new_cell_count)`.

**Row-level diffs of one table** — each `dolt_diff_<table>` vtab has
`from_<col>` / `to_<col>` columns paired with a `diff_type` of
`added` / `removed` / `modified`:

```sh
doltlite -readonly slack/ingest/entities.doltlite_db \
  "SELECT to_id, to_ts, diff_type
     FROM dolt_diff_messages
    WHERE from_ref = 'HEAD^1' AND to_ref = 'HEAD'
    LIMIT 20;"
```

Watch the filter column names — these vtabs accept `from_ref` /
`to_ref` (not `from_commit` / `to_commit`, even though the result row
has `to_commit` / `from_commit` data columns).

### What the upstream deleted on you

The reason to run the diff the other way around: a `removed` row is one
the *provider* no longer has, and the raw store still does. This is the
capability the versioned store buys that a plain mirror can't — see
[Noticing when the *upstream* loses data](/docs/dev/data_architecture_ingestion.md#noticing-when-the-upstream-loses-data)
for the preconditions (chiefly: the downloader has to re-enumerate, or
this diff is empty no matter what happened upstream).

```sh
doltlite -readonly claude/ingest/entities.doltlite_db \
  "SELECT from_id, diff_type
     FROM dolt_diff_conversations
    WHERE from_ref = 'HEAD^1' AND to_ref = 'HEAD'
      AND diff_type = 'removed';"
```

To read back what a deleted row actually said, ask the commit where it
still existed — the table name goes *in* the vtab name and the ref is
the argument, which is the opposite order from `dolt_diff_<table>`:

```sh
doltlite -readonly claude/ingest/entities.doltlite_db \
  "SELECT * FROM dolt_at_conversations('HEAD^1') WHERE id = '<the id>';"
```

A short download makes one commit, at the end
(`download <name>: <summary>`), so `HEAD^1` is usually the previous
sync. A long one also seals checkpoints on the way
(`checkpoint <name>: entities`), so check `dolt_log` messages before
trusting `HEAD^1`, and walk further back (`HEAD~10`, or a hash from
`dolt_log`) for a wider window.

### Which commits changed one row

Leave `from_ref` / `to_ref` off `dolt_diff_<table>` and it walks every
adjacent commit pair on the branch, one row per row that changed
between them, with `to_commit` / `from_commit` / `diff_type`:

```sh
doltlite -readonly slack/ingest/entities.doltlite_db \
  "SELECT to_commit, to_commit_date, diff_type
     FROM dolt_diff_messages
    WHERE coalesce(to_id, from_id) = '<the id>';"
```

Each pair costs a diff proportional to what changed in it, and the
`WHERE` can name any column, not just the key. The total cost is the
table's churn over its life — a full re-render adds a table's worth
permanently — so bound it with the range form,
`WHERE from_ref = '<old>..HEAD'`, when the store has years behind it.

Which commits touched which tables at all is the commit-level
`dolt_diff` vtab, and costs nothing:

```sh
doltlite -readonly slack/ingest/entities.doltlite_db \
  "SELECT commit_hash, date, table_name FROM dolt_diff WHERE table_name = 'messages';"
```

### `dolt_history_<table>` and `dolt_blame_<table>`: not for our tables

Both look like the tool for the question above and neither is. Their
primary-key pushdown exists only for **integer** keys
(`doltliteBestIndexIntPkRange` in `doltlite_history.c` and
`doltlite_blame.c`); every table in this tree keys on a `VARCHAR`, so
`WHERE id = ?` is applied after each commit's whole table has been
read. Measured on doltlite 0.50.3 against a 200k-row table with 63
commits: 20s for either, against ~1s for the `dolt_diff_<table>` walk.
`dolt_history_<table>` also lists a row at every commit it *existed*
in, changed or not.

They still work, just slowly, and blame's `commit` column has to be
quoted because it is a keyword:

```sh
doltlite -readonly slack/ingest/entities.doltlite_db \
  "SELECT \"commit\", commit_date, message
     FROM dolt_blame_messages
    WHERE id = '<uuid>';"
```

### Pretty output

`-box` / `-table` / `-markdown` all work and match `sqlite3`'s
behavior. Handy for one-off queries on the terminal:

```sh
doltlite -readonly -box slack/ingest/entities.doltlite_db "SELECT * FROM dolt_log() LIMIT 5;"
```

## Inventory: what `dolt_*` symbols exist

Doltlite registers a few dozen scalar functions and virtual tables on
every connection. To enumerate them against your binary:

```sh
doltlite :memory: "SELECT name FROM pragma_function_list WHERE name LIKE 'dolt_%' ORDER BY name;"
doltlite :memory: "SELECT name FROM pragma_module_list WHERE name LIKE 'dolt_%' ORDER BY name;"
```

The per-table modules (`dolt_at_<table>`, `dolt_diff_<table>`,
`dolt_history_<table>`) are registered the first time a statement names
them, so the second list leaves them out until something has.

The common-use subset:

| Symbol | Kind | Notes |
|---|---|---|
| `dolt_version()` | scalar fn | doltlite build string. Sanity check. |
| `active_branch()` | scalar fn | current HEAD's branch name. |
| `dolt_commit('-Am', msg)` | scalar fn | stage + commit; returns hash. **Don't** run by hand against a live DB. |
| `dolt_log()` | table-valued fn | `(commit_hash, committer, email, date, message)`. |
| `dolt_branches` | vtab | all branches with their head commit. |
| `dolt_status` | vtab | uncommitted-changes summary. |
| `dolt_schemas` | vtab | per-branch schema diff. |
| `dolt_diff_stat` | vtab + table-valued fn | per-table row/cell counts. As a vtab, filter with `from_ref` / `to_ref` and get every changed table; as a 3-arg call, one named table. |
| `dolt_diff` | vtab | which tables each commit on the branch changed. |
| `dolt_diff_summary` | vtab | which tables differ, data vs schema. Filter with `from_ref` / `to_ref`. |
| `dolt_diff_<table>` | vtab | row-level diff for one table. Filter with `from_ref` / `to_ref`, or leave both off for every adjacent pair on the branch. |
| `dolt_history_<table>` | vtab | every committed version of every row in one table. Full scan per commit on a text key — see above. |
| `dolt_blame_<table>` | vtab | per-row `git blame`. Same caveat. |
| `dolt_conflicts_<table>` | vtab | merge conflicts surviving a `dolt_merge`. |
| `dolt_commit_ancestors` | vtab | the commit DAG. |

## Operational notes

### Doltlite upgrades and the file format

Upstream freezes chunk-store format 12 for the DoltLite beta: every
version-12 file stays readable and writable by later version-12 builds.
So a doltlite bump that stays on 12 needs no store migration.
`third-party/doltlite/README.md` § "Upgrading doltlite" says how to
check a new release's format before bumping.

### `sqlite3_open_v2` is loop-bound — build doltlite at `-O2`

doltlite's open path walks the prolly chunk store's root pages and
blake3-hashes each one before any query can run. On a multi-GB raw
store that's a *lot* of tight inner-loop C code. We learned this the
hard way: a 3.5GB `slack/ingest/entities.doltlite_db` took **~60 seconds** to open
from Rust (sqlx blew its 30s `acquire_timeout`, the render phase
died, the UI grid silently went empty), while the upstream CLI on the
same file opened it in 2.4 seconds.

The diff turned out to be the C compile flags. Bazel's `fastbuild`
default for `cc_library` is `-O0`, which is a 15-25× hit specifically
for this workload (prolly-tree page walks + blake3 are pathologically
sensitive to compiler optimizations). Our `third-party/doltlite/BUILD.bazel`
now forces `-O2` regardless of `--compilation_mode` — we never step-
debug doltlite C from Rust anyway, so paying for optimized code under
fastbuild is a strict win.

Other compile-flag lesson learned along the way: don't add
`-DSQLITE_DEFAULT_FOREIGN_KEYS=1`. The upstream CLI builds without it,
and any caller that wants FK enforcement should send
`PRAGMA foreign_keys = ON` after connect (sqlx already does).

The standalone reproducer lives in `//hack/slack_open_debug/`. It
times raw `sqlite3_open_v2` (via `extern "C"` against our static
archive — no libsqlite3-sys, no sqlx) against the same open through
the sqlx pool, so a future regression in either layer is easy to
attribute.

### A writer's open discards the working set

`doltlite_raw::open` checks `dolt_status` at every open and, if
non-empty, runs `dolt_reset --hard` and then `dolt_clean()`, before
applying any DDL. Both halves are needed, and they split the way git's
do: reset restores the tracked tables and leaves an untracked one
alone, clean takes the untracked one. A writer that died after a
`CREATE TABLE` and before its first commit leaves exactly that. Every commit is `-Am`, so anything left dirty here
would ride into the schema commit a moment later; that is why the
untracked tables go too.

The rows it throws away were written after the last seal and never
committed, so no reader — every reader pins a commit — was ever
promised them. Keeping them would have meant committing a state the
writer never vouched for: an entity row whose blobs never arrived, half
a channel. The next pass refetches from the cursor.

`commit_run` is tolerant of "nothing to commit, working tree clean": a
pass that fetched nothing new leaves the working set clean.

If the `discard_dirty_working_tree` warning shows up in the run log
often, something is crashing between seals. Look upstream for the
cause (network timeout, panic, OOM, etc.).

## When not to use the CLI

- **During a live ETL run.** Every writable open on the Rust side
  takes the store's writer lock and pins its pool at
  `max_connections = 1`, so doltlite's per-connection HEAD stays
  coherent and no second writer gets in
  ([`etl/README.md`](/datalib/backend/etl/README.md) §"Connection
  pools"). The CLI takes no such lock, so a writable CLI session is
  exactly the second writer that rule exists to keep out.
- **For routine reads from app code.** Open the file via `sqlx` like
  everything else in the backend; the CLI is for ad-hoc inspection.
- **To "fix" a wedged DB.** The next writer's `open` discards the
  uncommitted state itself. Only reach for `dolt_reset` /
  `dolt_checkout` against a copy of the file, never against the live
  one.
