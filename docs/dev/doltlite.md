# `doltlite` — poking at a `.doltlite_db` file from the shell

> **GUI option:** For a macOS sqlite-browser build patched to load
> doltlite, grab a release from
> <https://github.com/thadd3us/sqlitebrowser/releases>. The CLI recipes
> below all still apply; the GUI is just nicer for exploring schema and
> running ad-hoc SELECTs.


Our raw ETL captures (under `<data_root>/<name>/raw/`) and the per-mirror
backend index (`<data_root>/unified_index/grid/db.doltlite_db`) are
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

> **Always pass `-readonly`** when you're just exploring. A second
> writer against a doltlite file commits onto its own branch and can
> wedge later ETL runs with `commit conflict` (the same scenario the
> `max_connections = 1` rule in `datalib_etl::doltlite_raw` exists
> to prevent on the Rust side).

## Getting the data out: export to plain SQLite

A `.doltlite_db` is not a SQLite *file*. It is a prolly-tree store, and
stock `sqlite3` opens one with `Error: file is not a database`. That is
a fact about the on-disk format, not about lock-in — the shell above
dumps any store to ordinary SQL, which stock SQLite loads:

```sh
datalib-doltlite -readonly unified_index/grid/db.doltlite_db .dump \
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
doltlite -readonly slack/raw/entities.doltlite_db \
  "SELECT commit_hash, committer, date, message
     FROM dolt_log()
    ORDER BY date DESC
    LIMIT 20;"
```

Each row is one `dolt_commit()` call from the ETL — e.g.
`download slack: msgs=29 replies=51 media[...]` for a successful Slack
sync, or `download slack: interrupted (Ctrl-C)` for a
ctrl-c'd run. `dolt_log()` walks back from `HEAD` on the active branch
(use `active_branch()` to check which one that is).

### Which branch is checked out / what branches exist

```sh
doltlite -readonly slack/raw/entities.doltlite_db "SELECT active_branch();"
doltlite -readonly slack/raw/entities.doltlite_db "SELECT * FROM dolt_branches;"
```

### Uncommitted changes (`git status`)

```sh
doltlite -readonly slack/raw/entities.doltlite_db "SELECT * FROM dolt_status;"
```

Columns are `(table_name, staged, status)`. A non-empty result used to
mean "an ETL run died before its `commit_run` call landed, and the
next successful run will fold the dirty rows into its own commit."
That implicit folding mixed two runs' work under one `dolt_log` entry,
so since the change documented in [Operational notes](#operational-notes)
below, `doltlite_raw::open` now seals any pre-existing dirty tree into
its own `rescue: ...` commit before doing anything else. A non-empty
`dolt_status` against a file you opened with the CLI just means an ETL
run is mid-flight (or recently was) and a rescue would land on the next
sync.

### What changed between two commits

**Per-table summary** — which tables differ, and is it a data or schema change:

```sh
doltlite -readonly slack/raw/entities.doltlite_db \
  "SELECT from_table_name, to_table_name, diff_type, data_change, schema_change
     FROM dolt_diff_summary
    WHERE from_ref = 'HEAD^1' AND to_ref = 'HEAD';"
```

`HEAD`, `HEAD^1`, `HEAD~N`, branch names, and full commit hashes all
work as refs. The cheapest "git status between commits" view.

**Per-table row counts** — has to be one table at a time, invoked as a
table-valued function (the `dolt_diff_stat` vtab form errors out — use
this 3-arg call):

```sh
doltlite -readonly slack/raw/entities.doltlite_db \
  "SELECT * FROM dolt_diff_stat('HEAD^1', 'HEAD', 'messages');"
```

Returns `(table_name, rows_unmodified, rows_added, rows_deleted, rows_modified,
cells_added, cells_deleted, cells_modified, old_row_count, new_row_count,
old_cell_count, new_cell_count)`.

**Row-level diffs of one table** — each `dolt_diff_<table>` vtab has
`from_<col>` / `to_<col>` columns paired with a `diff_type` of
`added` / `removed` / `modified`:

```sh
doltlite -readonly slack/raw/entities.doltlite_db \
  "SELECT to_id, to_ts, diff_type
     FROM dolt_diff_messages
    WHERE from_ref = 'HEAD^1' AND to_ref = 'HEAD'
    LIMIT 20;"
```

Watch the filter column names — these vtabs accept `from_ref` /
`to_ref` (not `from_commit` / `to_commit`, even though the result row
has `to_commit` / `from_commit` data columns).

### History of a single table

```sh
doltlite -readonly slack/raw/entities.doltlite_db \
  "SELECT commit_hash, commit_date, id, ts
     FROM dolt_history_messages
    ORDER BY commit_date DESC
    LIMIT 20;"
```

One row per (commit, primary-key) pair. Useful for tracing when a
specific row first appeared or last changed.

### `git blame` for a single row

```sh
doltlite -readonly slack/raw/entities.doltlite_db \
  "SELECT commit, committer, commit_date, message
     FROM dolt_blame_messages
    WHERE id = '<uuid>';"
```

(Replace `messages` with any table; the `dolt_blame_<table>` vtab is
created for each.)

### Pretty output

`-box` / `-table` / `-markdown` all work and match `sqlite3`'s
behavior. Handy for one-off queries on the terminal:

```sh
doltlite -readonly -box slack/raw/entities.doltlite_db "SELECT * FROM dolt_log() LIMIT 5;"
```

## Inventory: what `dolt_*` symbols exist

Doltlite registers a few dozen scalar functions and virtual tables on
every connection. To enumerate them against your binary:

```sh
doltlite :memory: "SELECT name FROM pragma_function_list WHERE name LIKE 'dolt_%' ORDER BY name;"
doltlite :memory: "SELECT name FROM pragma_module_list WHERE name LIKE 'dolt_%' ORDER BY name;"
```

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
| `dolt_diff_stat(from, to, table)` | table-valued fn | per-table row/cell counts. Call with 3 args; the vtab form doesn't accept WHERE filters. |
| `dolt_diff_summary` | vtab | which tables differ, data vs schema. Filter with `from_ref` / `to_ref`. |
| `dolt_diff_<table>` | vtab | row-level diff for one table. Filter with `from_ref` / `to_ref`. |
| `dolt_history_<table>` | vtab | every committed version of every row in one table. |
| `dolt_blame_<table>` | vtab | per-row `git blame`. |
| `dolt_conflicts_<table>` | vtab | merge conflicts surviving a `dolt_merge`. |
| `dolt_commit_ancestors` | vtab | the commit DAG. |

## Operational notes

### `sqlite3_open_v2` is loop-bound — build doltlite at `-O2`

doltlite's open path walks the prolly chunk store's root pages and
blake3-hashes each one before any query can run. On a multi-GB raw
store that's a *lot* of tight inner-loop C code. We learned this the
hard way: a 3.5GB `slack/raw/entities.doltlite_db` took **~60 seconds** to open
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

### Rescue commits on every Rust-side open

`doltlite_raw::open` now checks `dolt_status` at every open and, if
non-empty, stamps a `rescue: pre-run snapshot of orphaned working tree`
commit before applying any DDL. The point isn't recovery — `dolt_log`
audit-trail hygiene is. Without it, a crashed run's uncommitted rows
silently fold into the next successful `commit_run`, mixing two runs'
work under one commit message. With it, each tool entry that finds a
dirty tree gets a dedicated commit it can be traced to.

`commit_run` is tolerant of "nothing to commit, working tree clean" so
the trailing orchestrator commit can legitimately find the rescue
already swept its work (which it will, on a successful single-process
run where rescue is a no-op).

If you see a stream of `rescue: ...` commits in `dolt_log()`, something
is crashing mid-batch. Look upstream of the rescue for the actual cause
(network timeout, panic, OOM, etc.).

## When not to use the CLI

- **During a live ETL run.** The Rust pool is at `max_connections = 1`
  to keep doltlite's per-connection HEAD coherent (see the long doc
  comment at the top of `datalib/backend/etl/src/doltlite_raw.rs`).
  Adding a second writer through the CLI defeats that.
- **For routine reads from app code.** Open the file via `sqlx` like
  everything else in the backend; the CLI is for ad-hoc inspection.
- **To "fix" a wedged DB.** Almost every wedge is recoverable by
  letting the next ETL run pick up the uncommitted state. Only reach
  for `dolt_reset` / `dolt_checkout` against a copy of the file, never
  against the live one.
