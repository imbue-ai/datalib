# A move-aware diff viewer for two `fsindex` directory scans

`fsindex` records one row per file and directory — path, kind, size,
blake3 — into a doltlite store. This prototype turns two such scans into
a single self-contained HTML page: the two trees side by side, colour
coded, with **moves reported as moves** rather than as a delete plus an
unrelated create.

That last part is the point. Unison tells you `docs/reports` was deleted
and `archive/reports` was created, and leaves you to work out that they
are the same bytes. Here the two are one row.

```sh
bazelisk build //datalib/backend/dirtree_diff:dirtree_diff
./demo.sh            # builds two trees, scans them, writes two pages
```

## The short version: two trees, one database

Scan each tree into its own branch of a single file, then diff the two
branches. This is the arrangement the storage is designed for — the two
scans share every identical subtree as the same content-addressed
chunks, so the second one is nearly free.

```sh
# 1. scan the first tree into scans.doltlite_db, on branch `before`
datalib-fsindex --db scans.doltlite_db --root ./tree1 --branch before

# 2. scan the second tree into the SAME file, on branch `after`
datalib-fsindex --db scans.doltlite_db --root ./tree2 --branch after

# 3. diff the two branches — no unification step, they already share
#    a chunk store
datalib-dirtree-diff \
    --left  scans.doltlite_db#before \
    --right scans.doltlite_db#after \
    -o diff.html
```

```
wrote diff.html — 1 move(s) (+3 rolled up), 0 modified, 1 added,
0 deleted, 0 deleted-with-copy-remaining
```

`docs/reports` moving to `archive/reports` is the one move; the three
entries inside it are rolled up rather than repeated. Both scans live in
one 9 KB file.

Both binaries ship. From a source checkout they are
`bazel-bin/datalib/backend/bin/datalib-fsindex` and
`…/datalib-dirtree-diff` after `bazelisk build //datalib/backend:bin`.

Ships as `datalib-dirtree-diff` — it is in `//datalib/backend:dist`, so
a tagged release carries it like every other binary. sqlx links the same
doltlite amalgamation the rest of the tree does, so there is no CLI to
locate and no subprocess per query.

## Can doltlite's prolly diff work across two separate files?

**No, not directly — and it fails in two different ways.** Measured
against the Bazel-built shell on 2026-09-04:

| attempt | result |
|---|---|
| `ATTACH` the second file, `SELECT` and `JOIN` across it | **works** — ordinary SQL crosses files fine |
| `dolt_diff_files('<hash-from-B>', '<hash-from-A>')` after `ATTACH` | `ref not found: <hash-from-B>` |
| `other.dolt_diff_files(…)`, qualified to the attached db | `dolt_diff_files is only available in the main database` |
| `dolt_at_files('<hash-from-B>')` from A | `ref not found` |

So two separate limits stack. The diff table-valued function is bound to
the connection's *main* database, and commit hashes resolve only against
that database's own chunk store. `ATTACH` extends neither.

**But the files can be unified without rescanning anything.** A
`.doltlite_db` works as a `file://` remote for another one:

```sql
SELECT dolt_remote('add','left','file:///abs/path/before.doltlite_db');
SELECT dolt_remote('add','right','file:///abs/path/after.doltlite_db');
SELECT dolt_fetch('left');
SELECT dolt_fetch('right');
-- both commits now resolve here, and the prolly diff works
SELECT * FROM dolt_diff_files('<left-head>','<right-head>');
```

This is what the tool does, into a **throwaway scratch database** in a
temp dir. Neither input is opened for writing, copied, or modified; they
are read as remotes and the scratch file is deleted on exit (keep it
with `--keep-scratch`). The two histories share no ancestor, and the
diff does not need one — it is a tree diff, not a merge.

### It is cheap, because the chunks dedup

Two 20 000-row scans differing only by one moved top-level directory,
each `dolt_gc`'d first:

| | bytes |
|---|---|
| left scan alone | 1 834 175 |
| right scan alone | 1 836 025 |
| left + fetched right | 1 869 628 |

Fetching the entire second scan cost **35 453 bytes — 1.9% of its
standalone size**. The unified file holds both scans for 51% of what the
two separate files cost. Identical subtrees are literally the same
content-addressed chunks, so a scan of a mostly-unchanged tree is nearly
free to bring alongside another.

### Two commits in one file work directly

If both refs already live in one file — branches, or any two commit
hashes — there is nothing to unify and the tool skips the scratch
database entirely. The viewer accepts any mix:

```sh
# two independent scan files (unified via file:// fetch)
datalib-dirtree-diff --left before.doltlite_db --right after.doltlite_db -o d.html

# two branches of one file (diffed directly)
datalib-dirtree-diff --left scans.doltlite_db#main --right scans.doltlite_db#nightly -o d.html

# a branch in one file vs. a raw commit hash in another
datalib-dirtree-diff --left a.doltlite_db#main --right b.doltlite_db#9447a1f5… -o d.html
```

Each side is `PATH[#REF]`, where `REF` is a branch, `HEAD~2`, or a
commit hash, resolved with `dolt_hashof` inside its own file before
anything is unified. `#REF` defaults to `HEAD`.

### Writing several roots into one file

`fsindex --branch <name>` scans a root into its own branch of a shared
file, which is the arrangement `schema_raw.rs` describes and what
`demo.sh` case 2 uses:

```sh
datalib-fsindex --db scans.doltlite_db --root ./before --branch before
datalib-fsindex --db scans.doltlite_db --root ./after  --branch after
```

Each scan's `scan_meta.id` defaults to its root's directory name, so
standalone runs need no identifier. Pass `--source-id` when you want to
choose one — the pipeline does, because there a source's identity comes
from its config entry and outlives any particular path.

`--branch` was broken until recently and is worth knowing about if you
are reading older notes. `RawDb::checkout_branch` issued MySQL's
`CALL DOLT_CHECKOUT(?)`, which doltlite's parser rejects
(`near "CALL": syntax error`); the `-b` fallback used the same spelling,
so the flag failed outright instead of degrading. doltlite exposes the
dolt procedures as **functions** — `SELECT dolt_checkout(…)` — the same
distinction `app_store.rs` documents for `dolt_commit`. Fixed in
`download/db.rs`, with `tests/branch_scan.rs` covering it: every other
caller in the tree passes `target_doltlite_branch: None`, which is how
it stayed broken with a green suite.

Two things that fix depends on, both of them non-obvious:

- **Order matters in both directions.** A plain checkout of a missing
  branch errors `no such branch or table`; `-b` on an existing one
  errors `branch already exists`. Neither call is idempotent, so the
  try-then-create order is load-bearing.
- **The active branch is per-connection, not per-file.** A fresh
  connection starts on `main`. sqlx's stock pool would retire the one
  connection carrying the checkout after 30 minutes and silently
  continue on `main`, so `doltlite_raw::open` now disables
  `idle_timeout` and `max_lifetime` alongside its existing
  `max_connections(1)`.

## How moves are detected

`fsindex` hashes a directory over a canonical encoding of its immediate
children (`schema_raw.rs` §"Directory tree-hash canonicalization"), so a
directory's blake3 covers its whole subtree. Move a directory and its
digest is unchanged — it simply appears at a different path.

The prolly diff reports that as a `removed` row and an `added` row
carrying the **same digest**. Pairing them is the whole trick. Pairing
is greedy and prefers a candidate that kept its basename, so
`docs/reports` pairs with `archive/reports` rather than with some
unrelated directory that happens to hold identical bytes.

### Only the outermost move is reported

Moving `docs/` to `archive/` moves every descendant too, and each one
arrives from the diff as its own matched pair. Reporting all of them
buries the one fact worth reading, so a pair is suppressed when an
ancestor directory made exactly the same journey — same relative suffix
on both sides. The surviving outermost row carries a count of what it
absorbed:

```
docs/reports   moved →   +2 inside      moved to archive/reports
```

The same rollup runs over copies, so a directory copied wholesale is one
`copy +2 inside` row, not three. Rolled-up interiors are never findings:
under `--full-tree` they render as ordinary unchanged entries inside the
directory that moved (collapsed by default, since there is nothing to
see), and otherwise they are simply absent.

## Duplicates *within* one tree

Separately from the left/right comparison, the tool answers a
single-tree question: **is this tree storing the same bytes more than
once?** Every entry at or above `--dup-threshold` (default `1M`;
accepts `4096`, `64K`, `2G`; `0` turns it off) is grouped by digest, and
any digest with more than one path is reported on both panes as a purple
`dup ×N` badge, with same-pane links to the other copies and the bytes
you would get back.

Directories participate, which is the useful part: because a directory's
digest covers its subtree, a folder copied to a second place inside the
same tree is **one** finding rather than one per file in it. The same
rollup as moves and copies applies, so

```
themes/dark   dup ×2  +2 inside     same bytes in this tree at themes/dark_backup
```

is the whole report, not three rows. The pane header carries the
per-tree total:

```
after.doltlite_db @HEAD 05bca079b9cc · 2 duplicated within this tree, 35B reclaimable
```

This costs one full scan per side — the `size >= threshold` filter cuts
transfer and grouping work, not the scan, since `files` has no index on
`size` either. It announces itself on stderr like the copy check does.

## What the page tells you

The question this was built to answer — *did this actually go away, or
are these bytes still here somewhere else?* — is the difference between
the two red-ish states:

| badge | meaning |
|---|---|
| `moved →` / `← moved` | same bytes, different path. Click the path to jump to the counterpart in the other pane |
| `deleted` | gone, and these bytes are **nowhere** on the right |
| `gone (copy remains)` | gone from here, but identical bytes still live elsewhere on the right |
| `new` | new content, these bytes are **nowhere** on the left |
| `copy` | new at this path, but identical bytes already existed on the left |
| `changed` | same path, different content |
| `dup ×N` | this entry's bytes appear N times **within its own tree** |

Directories are grey structure: a directory's digest changes whenever
anything under it changes, so "modified" on a directory only ever means
"something below me moved", which the children already say.

## Cost, and the one expensive query

The diff itself is O(changes) — that is what the prolly tree buys — and
move detection is free, because both halves of a move are already in the
diff.

Distinguishing `deleted` from `gone (copy remains)` is not free: it asks
whether a digest exists *anywhere* on the other side, and `files`
carries no secondary index on `blake3` (deliberately —
`STORAGE_NOTES.md` §2 measures what one costs). Each lookup chunk is a
whole-corpus scan. It runs only for digests the move pairing could not
already account for, and it says so on stderr rather than hiding:

```
note: scanning the corpus at 9447a1f522 for 3 unmatched digest(s) —
      `files` has no blake3 index, so this is a full scan per chunk
```

Turn it off with `--no-copy-detection`, which downgrades those rows to a
plain `deleted` / `new` — never the other way round.

`--dup-threshold` costs one more full scan per side; `0` skips it.

`--full-tree` renders unchanged entries too, and costs a full scan of
both corpora. Without it the page holds only changed paths plus their
ancestor directories, which is derived from the diff alone.

## The shape of the code

There is a deliberate seam between "what we read" and "what we
concluded", because everything interesting lives on the second side and
should be testable without a database or a browser in the way:

```
     doltlite  ──▶  Inputs  ──▶  analyze()  ──▶  DiffResult  ──▶  HTML
                    (POD)        (pure)          (POD)        └─▶  JSON
```

- **`Inputs`** is every row that was read, plus the flags that shaped
  the reading. Nothing is interpreted yet.
- **`analyze(inputs) -> DiffResult`** is pure. Move pairing, subtree
  rollup, delete-vs-copy-remains, in-tree duplicate grouping — all of it
  happens here, over plain data.
- **`DiffResult`** is the representation. The page is a projection of
  it (`to_payload()`), and so is `--json`.

Every type derives `Serialize` + `Deserialize`, so the JSON *is* the
representation rather than a debug dump — a run captured with `--json`
deserializes straight back into a `DiffResult`, and a test asserts that
round-trip:

```sh
datalib-dirtree-diff --left before.doltlite_db --right after.doltlite_db \
    --json run.json -o run.html
```

`tests/analyze_test.rs` builds `Inputs` literals and asserts on
`DiffResult` — no `.doltlite_db`, no HTML, no browser. A moved subtree
is four `Entry` values and an assertion about which single row survives
the rollup. `tests/store_test.rs` covers the half only a real store can
prove: that two independent files unify and diff across each other.

### One doltlite trap worth knowing

`dolt_diff_<table>` and `dolt_at_<table>` are registered **when a
connection opens**, from the tables present at that moment. A scratch
database is empty when we open it to add the remotes, so *that*
connection never learns about `files` and every later query on it fails
with `no such table: dolt_diff_files` — while a fresh connection to the
same file works. Fetching and reading therefore cannot share a
connection, which is why `store::unify` hands back nothing and the
caller reopens. `store_test.rs` pins the behaviour, so if doltlite ever
starts refreshing the registry the test fails and the reopen can go.

## What a subtree move costs, and why

`files` is keyed by **root-relative path**, which is what makes any of
this possible: two scans taken at different absolute locations compare
directly, with the absolute root kept separately in `scan_meta`.

The trade-off shows up on exactly the operation this tool exists for.
Because the path *is* the key, renaming a directory rewrites the key of
every descendant, so a subtree move lands in the diff as two rows per
entry. Measured against synthetic scans where one top-level directory
was renamed and nothing else changed:

| files under the moved dir | diff rows | of those, directory rows |
|---|---|---|
| 5 000 | 10 100 | 100 |
| 50 000 | 100 502 | 502 |

Linear, and 100 000 rows to report *one* move. The rollup in this tool
is downstream cleanup for that.

The directory rows alone — half a percent of the diff — already carry
the whole story, because a directory's tree-hash covers its subtree. So
the obvious optimization is to diff directories first and never fetch
the interior. **That does not work today:** there is no index on `kind`,
so `WHERE from_kind = 'dir'` makes the query *slower* (0.56s vs 0.23s at
50k) — it adds a predicate without avoiding the walk. Getting the cheap
summary would mean either an index on `kind` or splitting directory rows
into their own table, in the same spirit as the existing
`files` / `scan_meta` split.

Splitting directory rows into their own table is filed as
[#276](https://github.com/imbue-ai/datalib/issues/276).

Keying on path components instead (parent id + name) would turn that
100 000-row diff into a single changed row. It would also cost the
property the prolly diff currently leans on — a full-path PK means the
clustered order *is* depth-first tree order, so a subtree is contiguous
— and it would make every path read a recursive walk. Worth knowing the
number before anyone reopens that debate.

## Status and limits

Young, and the gaps are known:

- **The page holds every node it renders.** Fine for the thousands;
  `--full-tree` on a multi-million-entry scan will produce an
  unreasonable HTML file. The tree is built client-side from a flat
  list, and nothing is virtualized.
- **Move pairing is greedy, not optimal.** When several identical files
  are deleted and several identical files appear, which pairs with which
  is a basename-then-depth heuristic. For identical content the
  distinction is cosmetic.
- **`identity_uuid` is unused.** fsindex can stamp directories with a
  breadcrumb UUID that survives a move *and* an edit; this tool matches
  on content only, so a directory that moved and changed in the same
  pass reads as a delete plus an add. Pairing on `identity_uuid` first
  would fix that, and is the obvious next step.
- **Duplicate groups are whole-corpus.** The scan is one pass per
  side and groups in RAM, which is the workflow `schema_raw.rs` intends
  ("stream the full table into RAM once and index it there"), but it is
  still O(corpus) and holds every above-threshold row in memory.
- **Scans are compared as-is.** `scan_meta.options_fingerprint` exists
  precisely so two scans taken under different `ignore` rules can be
  told apart; the tool does not read it yet, so comparing two
  differently-configured scans will report ignored files as added or
  deleted.
