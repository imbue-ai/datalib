# Doltlite: looking inside a store, and what the engine does

This page is the one place for what doltlite does. Other docs and code
comments state our rules — one writer per file, readers pinned — and
link here for the engine facts under them. Each fact says which doltlite
version it was checked on and, where one exists, the test or script
that holds it. The pinned version is in `MODULE.bazel`;
`doltlite --version` names the one a binary was built from.

**The facts are tests.** `//datalib/backend/doltlite_facts:doltlite_facts_test`
checks every single-process fact below in well under a second, one
test each, in a module named for its section here;
`//datalib/backend/etl:doltlite_two_process_test` checks the ones about
a writer and a reader in two processes. After a doltlite bump, run
both. A fact that moved fails by name: fix it here, then the test,
then whatever in the tree leaned on it.

> **GUI option:** For a macOS sqlite-browser build patched to load
> doltlite, grab a release from
> <https://github.com/thadd3us/sqlitebrowser/releases>. The CLI recipes
> below all still apply; the GUI is just nicer for exploring schema and
> running ad-hoc SELECTs. In the desktop app, Browse on a download step
> opens its `entities.doltlite_db` read-only: in that build (`-R`) when
> it is what opens `.doltlite_db` files, otherwise in the bundled shell
> (`-readonly`) in Terminal — `datalib/tauri/src/raw_store.rs`.

Every store the pipeline writes — the raw stores under
`<data_root>/<group>/ingest/`, each source's render store, and the grid
index (`<data_root>/unified_index/grid_index/db.doltlite_db`) — is a
[doltlite](https://github.com/dolthub/doltlite) database: SQLite with
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
> file's default branch, `main`, which is the one every reader reads.
> Its commits land on `main`, and the sync's next seal moves `main`
> over them, so they vanish without an error. While it holds a
> transaction open, the sync's writes wait on it and can fail with
> `database is locked` ([Locks and writers](#locks-and-writers)). The
> writer lock in `datalib_etl::doltlite_raw` keeps a second *Rust*
> writer out; nothing keeps the CLI out but you.

## Getting the data out: export to plain SQLite

A `.doltlite_db` is not a SQLite *file*. It is a prolly-tree store, and
stock `sqlite3` opens one with `Error: file is not a database`. That is
a fact about the on-disk format, not about lock-in — the shell above
dumps any store to ordinary SQL, which stock SQLite loads:

```sh
datalib-doltlite -readonly unified_index/grid_index/db.doltlite_db .dump \
  | sqlite3 grid.sqlite
```

That is the whole export. A 16 MB grid store comes out as about 15 MB
of SQL, well under a second each way, and `grid_rows` / `markdowns` /
`edges` arrive with their schemas, primary keys and indexes intact. It
works for the raw stores too — BLOB columns come through as hex
literals. (A source's `blobs.sqlite` is plain SQLite already; stock
`sqlite3` opens it.)

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
- **No pipe, one session:** an `ATTACH` of a `file:` URI with
  `doltlite_engine=sqlite` writes a real SQLite file from inside the
  shell ([Plain SQLite files](#plain-sqlite-files-and-sqlite-compatibility)):

  ```sh
  datalib-doltlite copy.doltlite_db \
    "ATTACH 'file:grid.sqlite?doltlite_engine=sqlite' AS out;
     CREATE TABLE out.grid_rows AS SELECT * FROM main.grid_rows;"
  ```

  The result is a file `sqlite3` opens directly. Two caveats.
  `-readonly` is missing from that command on purpose — under it the
  `ATTACH` cannot create the output and the next line fails with
  `unknown database out` — so run this against a **copy** of the store
  rather than adding a second writer to a live one. And
  `CREATE TABLE … AS SELECT` copies rows and column types but not
  primary keys or indexes, which leaves `.dump` the higher-fidelity
  route.

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
(use `active_branch()` to check which one that is); `dolt_log('<branch>')`
walks another branch's commits without switching to it.

### Which branch is checked out / what branches exist

```sh
doltlite -readonly slack/ingest/entities.doltlite_db "SELECT active_branch();"
doltlite -readonly slack/ingest/entities.doltlite_db "SELECT * FROM dolt_branches;"
```

Every connection lands on the file's stored default branch, `main`
([Branches, HEAD and the working set](#branches-head-and-the-working-set)).
So `dolt_log()` and a plain `SELECT` show you `main`: sealed state, not
whatever a run has in flight on `datalib_writer` (`etl/README.md` §"A
writer works on its own branch" for why that branch exists).

**To look at another branch or an old commit, open it by path**, still
read-only ([Opening a revision by path](#opening-a-revision-by-path)):

```sh
doltlite -readonly 'slack/ingest/entities.doltlite_db/datalib_writer'   # a branch, uncommitted rows included
doltlite -readonly 'slack/ingest/entities.doltlite_db/<40-hex hash>'   # one commit, pinned
```

Use `/` in the shell, not `@`. For committed rows you can also stay on
`main` and name the ref: `dolt_at_<table>('<branch or hash>')`.

### Uncommitted changes (`git status`)

```sh
doltlite -readonly slack/ingest/entities.doltlite_db "SELECT * FROM dolt_status;"
```

Columns are `(table_name, staged, status)`. A read-only connection
lands on `main`, whose working set a writer never touches (it works on
`datalib_writer`), so what you see there is rows someone wrote on `main`
without committing — a writable CLI session, say. What a writer has in
flight on its own branch, or left there when it died, the next writer's
`doltlite_raw::open` discards — see
[A writer's open discards the working set](#a-writers-open-discards-the-working-set).

With `-readonly` it is safe against a store a sync is writing right
now (doltlite 0.50.10 and later;
[What a read-only connection may do](#what-a-read-only-connection-may-do)).
Check `doltlite --version` before pointing an older shell at a live
store.

### What changed between two commits

**Per-table summary** — which tables differ, and is it a data or schema change:

```sh
doltlite -readonly slack/ingest/entities.doltlite_db \
  "SELECT from_table_name, to_table_name, diff_type, data_change, schema_change
     FROM dolt_diff_summary
    WHERE from_ref = 'HEAD^1' AND to_ref = 'HEAD';"
```

`HEAD`, `HEAD^1`, `HEAD~N`, branch names, tags and full commit hashes
all work as refs; a short hash prefix does not. The cheapest "git
status between commits" view.

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
     FROM dolt_diff_messages('HEAD^1', 'HEAD')
    LIMIT 20;"
```

The two refs can also go in as `WHERE from_ref = … AND to_ref = …`.
Either way the result's `from_commit` / `to_commit` columns hold
resolved hashes ([Diffs](#diffs)).

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
     FROM dolt_diff_conversations('HEAD^1', 'HEAD')
    WHERE diff_type = 'removed';"
```

To read back what a deleted row actually said, ask the commit where it
still existed — the table is in the vtab's name and the ref is its one
argument:

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

`dolt_history_<table>` answers it by key, and it is fast on our text
keys too:

```sh
doltlite -readonly slack/ingest/entities.doltlite_db \
  "SELECT commit_hash, commit_date FROM dolt_history_messages WHERE id = '<the id>';"
```

It lists the row at every commit it *existed* in, changed or not.
`dolt_blame_<table>` names the last commit that changed each row; its
`commit` column has to be quoted because it is a keyword:

```sh
doltlite -readonly slack/ingest/entities.doltlite_db \
  "SELECT \"commit\", commit_date, message
     FROM dolt_blame_messages
    WHERE id = '<uuid>';"
```

To see *how* the row changed at each step, leave the refs off
`dolt_diff_<table>` and it walks every adjacent commit pair on the
branch, one row per row that changed between them:

```sh
doltlite -readonly slack/ingest/entities.doltlite_db \
  "SELECT to_commit, to_commit_date, diff_type
     FROM dolt_diff_messages
    WHERE coalesce(to_id, from_id) = '<the id>';"
```

That walk costs the table's whole churn over its life, and the `WHERE`
is applied after it ([Diffs](#diffs)). To bound it, name the commits:
`AND to_commit IN (SELECT commit_hash FROM dolt_log('<old>..HEAD'))`.
Not `from_ref = '<old>..HEAD'`: in a diff a range is one comparison, so
the commits in between disappear.

Which commits touched which tables at all is the commit-level
`dolt_diff` vtab, and costs nothing:

```sh
doltlite -readonly slack/ingest/entities.doltlite_db \
  "SELECT commit_hash, date, table_name FROM dolt_diff WHERE table_name = 'messages';"
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
| `active_branch()` | scalar fn | current branch name; NULL on a [detached open](#opening-a-revision-by-path). |
| `dolt_commit('-Am', msg)` | scalar fn | stage + commit; returns hash. **Don't** run by hand against a live DB. |
| `dolt_log()` | table-valued fn | `(commit_hash, committer, email, date, message)`; takes a branch or a range. |
| `dolt_branches` | vtab | all branches with their head commit. |
| `dolt_status` | vtab | uncommitted-changes summary. |
| `dolt_schemas` | vtab | the views and triggers in the database (`type`, `name`, `fragment`). A schema diff between two commits is `dolt_schema_diff(from, to)`. |
| `dolt_diff_stat` | vtab + table-valued fn | per-table row/cell counts. As a vtab, filter with `from_ref` / `to_ref` and get every changed table; as a 3-arg call, one named table. |
| `dolt_diff` | vtab | which tables each commit on the branch changed. |
| `dolt_diff_summary` | vtab | which tables differ, data vs schema. Filter with `from_ref` / `to_ref`. |
| `dolt_diff_<table>` | vtab | row-level diff for one table. Pass two refs, or leave them off for every adjacent pair on the branch. |
| `dolt_merge(branch)` | scalar fn | merge a branch into the active one; returns the commit, or `Already up to date`. `'--squash'` first makes it one commit with one parent. See [Merging a branch](#merging-a-branch). |
| `dolt_merge_base(a, b)` | scalar fn | the commit two branches split at. |
| `dolt_conflicts_resolve('--ours' \| '--theirs', table)` | scalar fn | settles a merge's conflicts in one table, inside the merge's transaction. |
| `dolt_revert(hash)` | scalar fn | a new commit undoing one; see [Reverting a commit](#reverting-a-commit). |
| `dolt_at_<table>(ref)` | table-valued fn | one table as it was at a commit. |
| `dolt_history_<table>` | vtab | every committed version of every row in one table; a primary-key equality seeks per commit. |
| `dolt_blame_<table>` | vtab | per-row `git blame`: the last commit that changed each row. |
| `dolt_conflicts_<table>` | vtab | a merge's conflicts, visible only inside the transaction that made them; a conflicted merge never commits. |
| `dolt_commit_ancestors` | vtab | the commit DAG. |

## How doltlite behaves

The facts our rules rest on, checked on the pinned doltlite (0.50.14)
by the tests below; a number measured on an older release names it. A fact in one process is a test in
`doltlite_facts_test`, in the module named for its section. Anything
about a writer and a reader in two processes is measured with
`doltlite_two_process_test`, whose writer seals through the real
`commit_run`: the shell's writer is not `commit_run`, and a shell probe
has been wrong about contention before. "Shell probe" below marks a
fact seen only in the `doltlite` shell on a scratch store. Upstream's
own contract is `doc/doltlite/concurrency.md` and `doc/doltlite/refs.md`
in the doltlite repo, at the pinned tag.

### Branches, HEAD and the working set

- **A connection has its own active branch and HEAD. The working set —
  the rows written and not yet committed — belongs to the branch and
  lives in the file**, shared by every connection on that branch in any
  process. So two writers on one branch sweep each other's rows into
  their `dolt_commit('-Am', …)`, and a connection on another branch
  sees none of them. (Shell probes, two processes.)
- **A fresh connection lands on the file's stored default branch**,
  `zDefaultBranch` in the persisted refs block. `dolt_default_branch(x)`
  moves it for every later connection; nothing in the tree calls it.
  A connection's own branch is never written into the file.
  `a_fresh_connection_starts_on_main` guards the default.
- **`dolt_connect_branch(x)` switches a session without writing
  anything; `dolt_checkout(x)` persists a working set for the branch it
  leaves and the one it enters**, about 500 bytes a call. `dolt_checkout
  ('-b', x)` is what creates a branch. `reopening_an_untouched_store_does_not_grow_it`
  holds the first.
- **A missing branch fails loudly**: `branch not found` from
  `dolt_connect_branch`, `no such branch or table: x` from
  `dolt_checkout`. (Doltlite 0.50.3 let a failed checkout through
  silently, #691; the writer's pool still reads its branch back.)
- **The version-control procedures are functions**: `SELECT
  dolt_checkout(…)`. `CALL DOLT_CHECKOUT(…)` is a syntax error.
- **`dolt_checkout` refuses a commit hash or a tag** ("dolt does not
  support a detached head state"). To read one commit, open it by path.
- **Re-pointing a branch at the commit it already names still writes a
  ref chunk**, a few hundred bytes. `publish_to_main` skips the move
  when nothing changed for that reason.
- **A fast-forward `dolt_merge` costs under a millisecond; a real
  merge costs the tables' size.** On 74k synthetic rows, a branch that
  added five indexes of its own merged `main` in about half a second.

### Locks and writers

- **Doltlite's own lock is a sidecar file, `.<name>-lock`** (for
  `entities.doltlite_db`, `.entities.doltlite_db-lock`), taken through
  SQLite's file-locking layer for the length of each write transaction.
  The store file itself is never locked, and a connection that has not
  written holds nothing. The fork-inherited `flock` of doltlite 0.11.x,
  where a forked child's copy of the lock made the parent's writes fail,
  is gone: its reproducer saw 0 `SQLITE_BUSY` on 0.50.13 against
  ~200,000 on 0.11.5.
- **One write at a time per file.** A connection that tries to write
  while another holds a write transaction waits in the busy handler
  (sqlx sets a 5 s busy timeout by default) and then fails with
  `database is locked`. `dolt_commit`, `dolt_merge` and `dolt_branch`
  wait the same way. A commit that loses a race for its branch's HEAD
  fails with `cannot commit: database is busy or branch HEAD changed.
  Please retry your transaction.`
- **Doltlite does not refuse a second writer**; it only takes turns.
  Refusing one is our job, done by the per-file lock in `doltlite_raw`
  (`etl/README.md` § "One writer per file, by construction").
- **A process that moves a ref is a writer.** A reader that kept its
  own branch and fast-forwarded it with `dolt_merge('main')` every 1 to
  100 ms, beside the writer sealing through `commit_run`, cost the
  writer nothing: 0 of 1,500 seals refused on 0.50.12 and 0.50.13, where
  0.50.3 refused up to 100 of 500. But each refresh waits behind any
  transaction the writer holds open, grows the file 1–2 KB, and a reader
  refreshing with no pause at all starved the writer (no seal in 60 s).
  The `branch-read` scenarios in `doltlite_two_process_test` measure it.
- **Two writers on branches of their own** take turns under the same
  lock. On 0.50.3 about three quarters of their operations failed;
  on 0.50.13 a shell probe with a 5 s busy timeout saw none fail, but
  that has not been measured through `commit_run`
  ([Not yet measured](#not-yet-measured)). Our reason for one writer
  per file does not rest on it: `main` moves only by force-move, which
  is right only while one process moves it.
- **A pool of more than one connection** lands statements on
  connections that disagree about the tree: a `dolt_commit` whose hash
  never appears in `dolt_log`, or a commit refused as conflicting.
  Observed on 0.50.3 and in Dolt itself; not re-measured since.

### What a read-only connection may do

- **It refuses every write** with `attempt to write a readonly
  database` — including `dolt_checkout` and `dolt_connect_branch`, so it
  cannot switch branches in SQL (0.50.12 and later). It opens a branch
  or a commit by path instead.
- **It never blocks a writer.** `dolt_status` included, since doltlite
  0.50.10 (before that, a read-only `dolt_status` failed the writer's
  commit and lost its rows: dolthub/doltlite#2832, our #400).
  `a_reader_asking_dolt_status_never_makes_the_writers_commit_fail`
  holds it. The statements a reader here may issue are the allowlist in
  `etl/README.md` § "A reader opens read-only and pinned".
- **On a branch, a plain `SELECT` reads that branch's working set**:
  committed rows plus anything uncommitted on the branch. Our readers
  land on `main`, where our writers never leave anything uncommitted,
  so they see sealed state; a writable CLI session on `main` is the
  exception. On a [detached open](#opening-a-revision-by-path) a plain
  `SELECT` reads the commit and nothing else.
- **A scalar function answers from the session's last view of the
  store.** A bare `dolt_hashof('HEAD')` keeps naming the HEAD the
  connection last loaded, until a table read — any `SELECT` from a
  table, `sqlite_master` included — reloads it. `datalib_pin::head`
  reads `sqlite_master` first for that reason; `datalib_pin`'s tests
  hold it.
- **The per-table modules are registered the first time a statement
  names them** (0.50.12 and later). So `pragma_module_list` is no census
  of what a commit holds, and a table another process commits after this
  connection opened reads through `dolt_at_<table>` without a reopen.
- **A per-table module is named as its table is**: case-insensitively,
  and as a quoted identifier when the table's name needs quoting
  (`"dolt_at_odd ""name"""('HEAD')`). A missing one is `no such table:
  dolt_at_<table>` either way. `datalib_pin::Pin::table` always quotes,
  so upstream names a mirror keeps, such as Lightroom's
  `Adobe_AdditionalMetadata`, read like any other.
- **A `BEGIN` on a read-only connection holds one commit** for the whole
  transaction: plain tables read that commit, use their indexes, and the
  writer seals underneath untouched.
  `a_held_read_transaction_is_a_snapshot_while_the_writer_seals`; at
  1.3 GB on 0.50.3, `docs/dev/plans/paged_grids.md` § "Pinned and indexed".

### Three ways to read one commit

| | `dolt_at_<table>('<hash>')` | a held read transaction | a detached open, `<file>@<hash>` |
|---|---|---|---|
| which commit | any, named in each query | `main` as of `BEGIN` | any, named at open |
| indexes | primary-key equality only; no secondary index, no `ORDER BY` | all | all |
| writes to the file | none | none | none |
| a table committed later | readable by name | at the next transaction | open the newer commit |
| what holds it | `datalib_pin` tests | `a_held_read_transaction_is_a_snapshot_while_the_writer_seals` | `detached_readers_are_snapshots_while_the_writer_seals` |

Every reader in the tree uses the third (`doltlite_raw::open_reader`),
except the search applet, which holds a transaction; `dolt_at_` is for
reading one table at another commit on a connection already open (the
history reader). The detached open works on our `.doltlite_db` names
from 0.50.13 and runs every query from 0.50.14: three detached readers beside 500
flat-out seals saw 0 errors, each open read one sealed commit, opens
took ~1–10 ms even while the writer held a transaction, and the file
came out byte-identical to the writer's alone.

### Opening a revision by path

`sqlite3_open` — and so sqlx — takes a revision after the file name:
`<file>@<rev>` or `<file>/<rev>`.

- **A branch** opens that branch, working set and all; opened read-only,
  it refuses writes.
- **Anything else** — a 40-hex commit hash, a tag, `main~2` — opens
  **detached**: read-only whatever the flags, `active_branch()` NULL,
  pinned even if a peer moves or deletes the ref. `dolt_diff_<table>`,
  `dolt_log()` and `dolt_hashof('HEAD')` work there.
- **A missing revision fails the open**: `branch or revision "x" not found`.
- **Every query runs on a detached open**, including those that need a
  temporary table (`IN (…)`, `DISTINCT`); through 0.50.13 those were
  refused as writes (dolthub/doltlite#3392).
  `revision_by_path::a_detached_open_runs_queries_that_need_an_ephemeral_table`.

Doltlite decides where the file name ends by looking for the longest
prefix that is a doltlite store. Before 0.50.13 it looked only at
prefixes whose name held `.db` or `.sqlite`, so a `.doltlite_db` store
failed with `unable to open database file` (dolthub/doltlite#3231).

**In the shell, use `/`.** The shell splits `@` off a `.doltlite_db`
name itself and then switches branches in SQL, so `doltlite -readonly
'<file>@<rev>'` fails with `attempt to write a readonly database`;
`doltlite -readonly '<file>/<rev>'` works.
`revision_by_path::the_shell_opens_a_revision_with_a_slash_not_an_at`.

### Diffs

- **`dolt_diff_<table>` takes its two refs as arguments,
  `dolt_diff_t('<from>', '<to>')`, or as filters** on `from_ref` /
  `to_ref` or on `from_commit` / `to_commit`; all three give the same
  rows, and every ref spelling works. (On 0.50.3, filtering the bare
  vtab on `from_commit` / `to_commit` across a non-adjacent range
  returned 0 rows.)
- **With no refs it walks every adjacent commit pair on the branch.**
- **A range as a ref, `'<a>..<b>'`, is one comparison of `a` with
  `b`**, not a walk: every row has `to_commit` = `b`.
- **No column predicate is pushed into `dolt_diff_<table>`**, not even
  a primary-key equality: a filter saves transfer, never the walk.
- **`diff_type` is `added`, `modified` or `removed`**; there is no
  `unchanged`.
- **`dolt_history_<table>` and `dolt_blame_<table>` seek a primary-key
  equality in each commit, text keys included** (0.50.12 and later): on
  a 200k-row table with 42 commits, under 10 ms against ~0.8 s for the
  `dolt_diff_<table>` walk.
- **A keyless table's rows are keyed by a hidden rowid, and the diff
  pairs rows by rowid, not by content.** Drop and refill it in a
  different order, or with one row gone, and the diff reads as a run of
  `modified` rows; delete and re-insert and every row reads as removed
  and added. Identical rows in the same order are no change.
- **An unchanged row is no change.** Re-writing a table with the same
  rows — upsert, delete and re-insert, or drop and recreate with the
  same schema — leaves `dolt_status` clean and the next `dolt_commit`
  fails with `nothing to commit, working tree clean`.
- **The diff goes by table name.** Rename a table to a new name and
  that name's diff shows every row as added; swap a staging table in
  (`DROP TABLE t` and `ALTER TABLE t_staging RENAME TO t` in one
  transaction) and `t`'s diff shows only the rows that differ.
- **A diff reads only the main database.** A commit hash from an
  `ATTACH`ed store is `ref not found`; `datalib/backend/dirtree_diff/README.md`
  has the fetch-into-scratch way to compare two files.

### Merging a branch

How a draft works: edits go to a branch of their own, uncommitted, and
saving merges that branch into the one readers see.

- **A branch's uncommitted rows outlive the connection that wrote
  them**, and `dolt_reset('--hard')` / `dolt_clean()` on another branch
  leave them alone. `dolt_diff_<table>('<commit>', 'WORKING')` on that
  branch reads them as a diff.
- **A merge takes a branch's commits, not its uncommitted rows**: a
  branch that only has those merges as `Already up to date`. A merge
  *into* a branch with uncommitted rows is refused (`uncommitted
  changes`).
- **Merges are cell by cell.** Two branches that change different
  columns of one row merge cleanly, into a commit with two parents.
- **A conflict outside a transaction changes nothing** (`conflicts
  detected`, rolled back). Inside `BEGIN`, `dolt_merge` still returns
  an error (`Merge has 1 conflict(s)`), but the transaction stays open:
  `dolt_conflicts_<table>` holds each row's `base_`, `our_` and
  `their_` columns, `dolt_conflicts_resolve('--theirs', '<table>')`
  takes the merged branch's side, and `dolt_commit` commits the merge
  and ends the transaction.
- **`dolt_merge('--squash', b)` commits at once**, one commit whose one
  parent is the old head; the branch's own commits never reach the log.
- **A merge commit and a squash both revert** with `dolt_revert`.
- **`dolt_branch('-d', b)` refuses a branch with unmerged commits**
  (`branch is not fully merged`) and drops one whose only change is
  uncommitted; `-D` drops either, uncommitted rows included, so a
  branch made again under the same name starts clean.

### Reverting a commit

- **`dolt_revert('<hash>')` makes a new commit that undoes the named
  one, on the active branch, and returns its hash.** The commit need
  not be HEAD; the ones after it stay. The message is
  `Revert "<original message>"`, and the working set is clean after it.
- **It is a merge, and is refused with `conflicts detected` when a
  later commit changed a row the commit touched.** Nothing is
  committed or changed then. A row still exactly as the commit left it
  reverts cleanly, so a revert of a commit that deleted a row puts it
  back even after later commits elsewhere.
- **Reverting the same commit twice is `nothing to commit`**; reverting
  the revert restores what the original did.
- **An uncommitted change refuses it** (`Your local changes would be
  overwritten by revert`), through the library we link; a probe through
  the shell went ahead and left the change uncommitted.

### Query plans and indexes

- **Plain tables plan as in SQLite**: on a read-only connection, inside
  a read transaction and on a detached open alike.
- **`dolt_at_<table>` seeks only a primary-key equality** (text keys
  included, 0.50.12 and later). It never uses a secondary index, a
  primary-key range on a text key, or an `ORDER BY`: those scan the
  table and sort.
- **A filter on a column with no index, ordered by an indexed one,
  scans the table and sorts what matches** (0.50.13). 0.50.12 walked the
  order's index and tested each row. For the grid's listing, which has
  no `LIMIT`, the new plan is faster: 3–6 ms against 32–35 ms over 74k
  synthetic rows. `every_filter_key_is_served_by_an_index` holds which
  keys have an index.
- **A `VIRTUAL` generated column with an index plans as an ordinary
  index search, not a covering one**; an expression index is covering.
- **`rowid` on a table keyed by text is a hash of the key** and carries
  no order; a keyless table's rowids count up from 1.
- **`dbstat` is not supported**: the chunk store has no page layout.

### What a write costs

A doltlite file is a bag of content-addressed chunks. A table is a
prolly tree — a B-tree whose pages are chunks named by their hash —
and a chunk is never edited in place: a write produces a new leaf page
holding the changed rows *and every unchanged row that shared the
page*, plus a new copy of each page on the path to the root. The old
pages stay in the file until `dolt_gc()` finds nothing that reaches
them. A commit is a small chunk naming one root; it makes that root's
pages reachable, forever, and does nothing else.

Three consequences, each measured with `scripts/doltlite_commit_cost.py`
(100k rows of ~100 bytes; the table is in `hack/doltlite_commit_cost/`;
0.50.3 and 0.50.13 agree after gc):

- **A SQL transaction rewrites each page it touched once, at
  `COMMIT`.** 200 statements in 200 transactions wrote 439 MB of pages
  for 15 MB of rows; the same 200 statements in one transaction wrote
  25 MB.
- **The pages a transaction touches are the pages its keys fall in.**
  The tree is sorted by primary key. Rows whose keys are adjacent land
  in one or two leaves; the same number of rows with random keys land
  in one leaf each (400 rows: 39 KB against 1.75 MB written). Random
  keys are uuidv4s, uuidv5s and content hashes; adjacent keys are
  `(device_id, ts_ms)`, `"{metric}#{date}"`, and the time-prefixed ids
  `datalib_id` mints.
- **A commit pins whatever its transaction wrote.** Commit once at the
  end and `dolt_gc()` reclaims every intermediate page (439 MB → 15 MB).
  Commit after each of 200 transactions and gc reclaims nothing
  (→ 419 MB), because each intermediate tree is now history.

The time is small either way: a statement outside a transaction costs
well under a millisecond, and a `dolt_commit` after the transaction has
written its pages takes about a fifth of a millisecond, whatever it
seals. Sealing on a branch and then moving `main` costs about 0.8 KB
more per seal than committing on `main`. A transaction holds its writes
in memory at about twice their size, and splitting it buys little: 150
MB of rows peaked at 323 MB RSS in one transaction and 239 MB in ten,
and 456 MB of rows at about 570 MB in one or thirty.

A diff costs what changed, not the table's size: one changed row in a
million-row table diffs in under 10 ms. And branches in one file share
every chunk they have in common: a second branch holding 999k of the
same 1M rows added 3.5 MB to a 133 MB store after gc.

Creating and dropping a table in one open still appends chunks, though
`dolt_status` ends clean.

### Disk space and `dolt_gc`

- **Doltlite does not compress chunks** (dolthub/doltlite#655, open).
- **A deleted row stays reachable from the commits before it**, so a
  `DELETE` and a commit reclaim nothing, even after `dolt_gc()`.
  Rewriting history does: `dolt_reset('--soft', <base>)` then
  `dolt_commit` folds the commits after `<base>` into one, and the next
  gc reclaims what only they reached (419 MB → 15 MB). It deletes commit
  hashes, so a consumer whose cursor named one falls back to a full
  pass, and a later read at one of them has nothing to read. (An
  already-open connection kept reading its commit in a shell probe;
  see [Not yet measured](#not-yet-measured).)
- **`dolt_gc()` may run before or after `dolt_commit`**; at a million
  uncommitted rows both orders work.
- **A pinned commit survives gc**: a `dolt_at_` read of an old commit
  still answers after a peer's gc (`hack/doltlite_concurrent_reader/`).
- **gc writes a compacted copy before it drops the original**, so it
  needs about the store's size again in free disk (upstream
  `doc/doltlite/dolt_gc.md`; not measured here).
- Only `sqlite_mirror` and `fsindex` run gc today.

### Plain SQLite files and SQLite compatibility

- **Stock `sqlite3` cannot open a `.doltlite_db`**: `file is not a
  database`. Doltlite reads and writes plain SQLite files, though, with
  its default engine.
- **`doltlite_engine=sqlite` makes a new file plain SQLite** — only in
  a `file:` URI, only for a file that is new or empty; an existing
  store keeps its format. Without `file:` the `?…` is part of a literal
  file name. `VACUUM INTO` takes no URI and writes the source's own
  format: a doltlite store's copy is a doltlite file, a plain SQLite
  file's copy is plain.
- **`VACUUM INTO` of a live SQLite file in WAL mode includes the WAL's
  rows**, which is how the SQLite mirrors snapshot a source in use.
- **`journal_mode` is inert**: any mode is accepted and reads back
  `wal`, and doltlite makes no `-wal` or `-shm` sidecar. `synchronous`
  at anything above `OFF` syncs every commit (upstream
  `doc/doltlite/pragmas.md`).
- **`:memory:` works**, commits and diffs included, which is what unit
  tests use.
- **A primary key that is not `INTEGER` is `NOT NULL`**
  (`pragma_table_info` says so, where stock SQLite says nullable), and
  its index is `sqlite_autoindex_<table>_1`.
- **A quoted type name keeps its affinity**, and a quoted `"INTEGER"`
  primary key is still a rowid alias.
- **FTS3/4/5 and R-tree are compiled in**; foreign keys are off unless
  a connection turns them on.
- **Every new file starts with an "Initialize data repository" commit**
  stamped with the wall clock, so two fresh stores differ;
  `dolt_commit('--date', …)` pins a commit's date. `dolt_log().date`
  has one-second resolution.
- **`dolt_reset('--hard')` restores tracked tables and keeps an untracked
  one; `dolt_clean()` removes it.** At the initialization commit, a hard
  reset drops `sqlite_sequence`, after which `dolt_clean()` fails with
  `no such table: main.sqlite_sequence`, so `doltlite_raw` skips the
  reset there.

## Versions: the storage format, and what each pin brought

Upstream freezes chunk-store format 12 for the DoltLite beta: every
version-12 file stays readable and writable by later version-12 builds,
so a bump that stays on 12 needs no store migration. A file whose
format differs is refused at open (`SQLITE_NOTADB`, "written by an
incompatible doltlite version"). `third-party/doltlite/README.md`
§ "Upgrading doltlite" has the procedure for a bump.

What each pin was taken for, newest first:

| version | what it brought us |
|---|---|
| 0.50.14 | a detached open runs `IN` and `DISTINCT` (dolthub/doltlite#3392), so readers open `<store>@<commit>` directly |
| 0.50.13 | a revision opened by path works on any file name (dolthub/doltlite#3231); an unindexed filter plans as scan + sort |
| 0.50.12 | per-table modules registered on first use; `dolt_at_` and `dolt_history_` seek a text primary key; a ref-moving reader no longer refuses the writer's seals |
| 0.50.10 | a read-only `dolt_status` no longer fails a writer's commit (dolthub/doltlite#2832, our #400) |
| 0.50.0 | the beta, a renumbering of 0.11.57 with no code change; its point is the frozen format |
| 0.11.54–0.11.57 | index corruption fixes in merges and reverts, and `VACUUM INTO` |
| 0.11.53 | large values read from a plain SQLite file no longer collapse onto the first row's bytes (dolthub/doltlite#2327; `hack/doltlite_blob_bug/run.sh` checks it) |
| 0.11.13 | chunk-store format 12 |
| 0.11.4 | the amalgamation builds doltlite, not stock SQLite, so the build is one file |

## Operational notes

### Build doltlite at `-O2`

`third-party/doltlite/BUILD.bazel` compiles doltlite at `-O2` whatever
`--compilation_mode` says. Bazel's `fastbuild` default is `-O0`, and on
doltlite 0.11.x a 3.5 GB raw store built that way took ~60 s to open
from Rust — past sqlx's 30 s acquire timeout — where the upstream CLI
took 2.4 s. We never step-debug doltlite's C, so optimizing it costs
nothing. `//hack/slack_open_debug/` times a raw `sqlite3_open_v2`
against the same open through sqlx, to tell the two layers apart if
opens slow down again.

Don't add `-DSQLITE_DEFAULT_FOREIGN_KEYS=1`: the upstream CLI builds
without it, and a caller that wants foreign keys enforced turns them on
after connect (sqlx does).

### A writer's open discards the working set

`doltlite_raw::open` checks `dolt_status` at every open and, if
non-empty, runs `dolt_reset --hard` and then `dolt_clean()`, before
applying any DDL. Both halves are needed, and they split the way git's
do: reset restores the tracked tables and leaves an untracked one
alone, clean takes the untracked one. A writer that died after a
`CREATE TABLE` and before its first commit leaves exactly that. Every
commit is `-Am`, so anything left dirty here would ride into the schema
commit a moment later; that is why the untracked tables go too.

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

- **During a live ETL run, writably.** Every writable open on the Rust
  side takes the store's writer lock and pins its pool at
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

## Not yet measured

Claims that were true on an older doltlite, or seen only in a shell
probe, and that nothing in the tree yet checks on 0.50.13:

- **Two writers, each on a branch of its own, through `commit_run`.**
  Needs a harness role that seals on a named branch.
- **An open reader after a peer's `dolt_gc()`.** A shell probe saw an
  open read-only connection stay on the pre-gc state for good, even
  across transactions. Nothing runs gc on a store a reader holds open,
  but confirm through sqlx before relying on either answer.
- **Full-size numbers.** The 1.3 GB read-transaction measurement and
  the multi-GB open time were taken on 0.50.3 and earlier against real
  stores, and re-checked only on synthetic ones since. A 197 MB store
  opens in 0.01 s on the shipped build; whether open time still grows
  with the store is not known.
