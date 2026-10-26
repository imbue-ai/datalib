# A move-aware diff viewer for two `fsindex` directory scans

`fsindex` records one row per file (path, kind, size, blake3, in
`files`) and one per directory (path, size, entry count, tree-hash, in
`dirs`) into a doltlite store. This tool turns two such scans into
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

Both binaries ship in `//datalib/backend:dist`, so a release carries
them like every other binary. From a source checkout they are
`bazel-bin/datalib/backend/bin/datalib-fsindex` and
`…/datalib-dirtree-diff` after `bazelisk build //datalib/backend:bin`.
sqlx links the same doltlite amalgamation the rest of the tree does, so
there is no CLI to locate and no subprocess per query.

## A worked recipe on a Mac: is one folder a superset of another?

Say two backups of the same thing sit in a synced folder, and you want
to know whether the newer one holds every file the older one does. The
names have spaces and the folders are synced, which is the usual case
under `~/Library/CloudStorage`.

```zsh
old="$HOME/Library/CloudStorage/Dropbox/backups/Archive 2025"
new="$HOME/Library/CloudStorage/Dropbox/backups/Archive 2026"
db="$TMPDIR/compare.doltlite_db"     # anywhere outside both trees

datalib-fsindex --db "$db" --root "$old" --branch older --no-stamp
datalib-fsindex --db "$db" --root "$new" --branch newer --no-stamp

datalib-dirtree-diff --left "$db#older" --right "$db#newer" -o "$TMPDIR/compare.html"
open "$TMPDIR/compare.html"
```

Hashing a file that is online-only downloads it. To see how many a
folder holds before you scan it, run `find "$old" -flags +dataless | wc -l`.

`recipe_test.sh` beside this README runs the recipe's code blocks
as written, on a made-up tree, so a change that breaks them fails a test.

Rescanning a folder costs a `stat` per unchanged file. The fingerprint
cache (`~/Library/Caches/datalib/fingerprints.sqlite`) keeps each file's
hash under its absolute path, and reuses it while size, mtime, ctime and
inode all match. A second folder's first scan reads every byte, even
where the files are copies of the first folder's.

`--no-stamp` makes the scan read-only. Without it, a folder whose
`.fsindex.toml` opts into stamping gets a UUID written into that file,
which is a change a sync client will upload.

The line `datalib-dirtree-diff` prints answers the question. The newer
folder holds every byte the older one does when `deleted` is 0.
`deleted-with-copy-remaining` and moves are fine, since their bytes are
still on the right under another path. A `modified` entry is a path
whose content changed, and expect `.DS_Store` among them, because Finder
rewrites it.

The same question in SQL, with `datalib-doltlite` (from the same
release):

```zsh
datalib-doltlite -readonly -box "$db" "
  SELECT count(*) AS missing, sum(size) AS bytes
    FROM dolt_at_files('older') AS o
   WHERE o.kind = 'file' AND o.id NOT LIKE '%.DS_Store'
     AND o.blake3 NOT IN (SELECT blake3 FROM dolt_at_files('newer'));"
```

Swap the `SELECT` line for `SELECT o.id, o.size` to list the files
that are missing. Scans in one file are read by branch name, never as
`main`; see fsindex's
[INGEST.md](../etl/providers/fsindex/INGEST.md) §"Several roots in one
file: branches".

Three zsh traps. Quote every path: zsh does not split an unquoted
`$var` into words, so `set -- $pair` gives you one argument, not two.
Quote `"$db#older"` too, since `#` is a glob operator when
`EXTENDED_GLOB` is set. And keep `$db` outside both trees, or the
second scan indexes the first scan's store.

## Can doltlite's prolly diff work across two separate files?

**No, not directly.** Ordinary SQL crosses an `ATTACH` fine, but a
diff reads only the connection's main database and resolves commit
hashes only against its own chunk store: a hash from the attached file
is `ref not found`, and `other.dolt_diff_files(…)` is refused
([diffs](/docs/dev/doltlite.md#diffs)).

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
file, which is what `demo.sh` case 2 uses:

```sh
datalib-fsindex --db scans.doltlite_db --root ./before --branch before
datalib-fsindex --db scans.doltlite_db --root ./after  --branch after
```

Each scan's `scan_meta.id` defaults to its root's directory name, so
standalone runs need no identifier. Pass `--source-id` when you want to
choose one — the pipeline does, because there a source's identity comes
from its config entry and outlives any particular path.

The checkout is fsindex's `RawDb::checkout_branch`
(`etl/providers/fsindex/src/ingest/db.rs`), covered by fsindex's
`tests/fsindex_tests/branch_scan.rs`. It connects to the branch with
`dolt_connect_branch`, which writes nothing, and creates the branch
with `dolt_checkout('-b', …)` only when that connect fails, then reads
the active branch back. The branch
is per connection, so the pool keeps its one connection for good
([branches, HEAD and the working set](/docs/dev/doltlite.md#branches-head-and-the-working-set);
`etl/README.md` §"Every pool is size 1 and never recycled").

## How moves are detected

`fsindex` hashes a directory over a canonical encoding of its immediate
children (`etl/providers/fsindex/src/ingest/hash.rs`), so a directory's blake3 covers its whole subtree.
Move a directory and its digest is unchanged — it simply appears at a
different path.

The prolly diff reports that as a `removed` row and an `added` row
carrying the **same digest**. Pairing them is the whole trick. Pairing
is greedy and prefers a candidate that kept its basename, so
`docs/reports` pairs with `archive/reports` rather than with some
unrelated directory that happens to hold identical bytes.

### Directories first

The tool diffs `dirs` before `files`, and that order is what makes a
big move cheap. The `dirs` diff is a few percent of the rows, and a
moved directory's row already says everything the interior would:
same digest on both sides, plus `size` and `entries` for the subtree.
So the moves and copies found among the directory rows are settled
first, and the `files` diff is then read **leaving those subtrees
out** — every file under a moved directory is implied by the
directory, and the tree-hash guarantees the interiors match exactly.
The page comes out identical; the rows simply never cross into Rust.

Measured on two 500 000-file scans differing by one 50 000-file
top-level rename (warm):

| | rows read | wall |
|---|---|---|
| `dirs` diff | 23 | ~0 ms |
| `files` diff, interiors included | 100 000 | 1.2 s end to end |
| `files` diff, explained interiors skipped | 0 | **0.27 s** end to end |

The skip is a `WHERE` clause on the key ranges, and it saves only the
transfer: the engine walks the same chunks either way (0.22 s here;
[diffs](/docs/dev/doltlite.md#diffs)). Excluded
prefixes are bound parameters, capped at 100 moves plus 100 copies per
side so the statement stays under sqlite's expression-depth limit; past
the cap, the remaining interiors are fetched and rolled up the ordinary
way. A move is always skipped on both sides or neither — skipping one
side alone leaves thousands of rows with nothing to pair against, and
each of those then costs a corpus scan in copy detection (measured:
21 s for the same rename).

### Only the outermost move is reported

Moving `docs/` to `archive/` moves every descendant too. Whatever
interior rows do reach the tool (a nested directory's own row, or files
past the skip cap) are suppressed when an ancestor directory made
exactly the same journey — same relative suffix on both sides. The
surviving outermost row carries a count of what it stands for, read
off the directory row's `entries`:

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

This costs one full scan of both tables per side — the
`size >= threshold` filter cuts transfer and grouping work, not the
scan, since neither table has an index on `size`. It announces itself on
stderr like the copy check does.

## Finding where the diff actually is

Every node carries the weight of the changes beneath it, and children
are ordered by it, so the busiest directory is the first thing you see:

```
huge_churn   50 changed · 9.5M
archive      32 changed · 1.4M
medium        5 changed · 24K
quiet
```

Two numbers, because they answer different questions and only one of
them sums naively:

- **entries** — every finding in the subtree, plus what its rollups
  absorbed. A directory that moved with 10 000 files inside is one
  finding but 10 001 affected entries, and the second number is the one
  that says how much is going on.
- **bytes** — counted from *maximal* findings only, meaning a finding
  with no other finding beneath it. A directory's size is the recursive
  sum of its contents, so a brand-new tree would otherwise count those
  bytes twice: once for the directory and again for each file in it.
  Taking only the outermost avoids that, and still gives a rolled-up
  move — whose interior is absent from the page — its full weight.

Both are computed in `analyze`, not in the page, so `--json` carries
them and the arithmetic is asserted in `analyze_test.rs` rather than in
a browser.

A directory opens on load only while its subtree is small (25 affected
entries). Without a ceiling, sorting the busiest directory to the top
and then expanding it fills the screen with one directory — the
opposite of what the ordering is for. `sort → name` restores the
ordinary alphabetical tree.

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
whether a digest exists *anywhere* on the other side, and neither table
carries a secondary index on `blake3` (deliberately —
[`STORAGE_NOTES.md`](../etl/providers/fsindex/src/ingest/STORAGE_NOTES.md)
§2 measures what one costs). A directory digest is
looked up in `dirs`, which is small enough not to matter; a file digest
costs one whole-corpus scan of `files`, however many digests are
wanted, since they go in as one bound array. It runs only for
digests the move pairing could not already account for, and it says so
on stderr rather than hiding:

```
note: scanning the corpus at 9447a1f522 for 3 unmatched digest(s) —
      `files` has no blake3 index, so this is a full scan
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

### Fetching and reading share a connection

The connection that fetched both scans into the empty scratch store can
read `files` straight away, because doltlite registers the per-table
modules on first use
([what a read-only connection may do](/docs/dev/doltlite.md#what-a-read-only-connection-may-do)),
so `store::unify` hands it back. `store_test.rs` pins that.

## What a subtree move costs, and why

Both tables are keyed by **root-relative path**, which is what makes
any of this possible: two scans taken at different absolute locations
compare directly, with the absolute root kept separately in
`scan_meta`.

The trade-off shows up on exactly the operation this tool exists for.
Because the path *is* the key, renaming a directory rewrites the key of
every descendant, so in `files` a subtree move lands as two rows per
file. Measured against synthetic scans where one top-level directory
was renamed and nothing else changed:

| files under the moved dir | `dolt_diff_files` rows | `dolt_diff_dirs` rows |
|---|---|---|
| 5 000 | 10 000 | 23 |
| 50 000 | 100 000 | 23 |

Linear in the files, and constant in the directories — which is why
directories have their own table and why this tool reads that table
first (§"Directories first"). The alternative —
an index on `kind` — would not have helped: a diff pushes no predicate
down and `dolt_at_<t>` seeks only a primary-key equality
([query plans](/docs/dev/doltlite.md#query-plans-and-indexes)), so a
filter on `kind` saves transfer and never the walk, and a secondary
index on a TEXT-keyed table re-stores the path per row.

Keying on path components instead (parent id + name) would turn the
100 000-row diff into a single changed row. It would also cost the
property the prolly diff currently leans on — a full-path PK means the
clustered order *is* depth-first tree order, so a subtree is contiguous
— and it would make every path read a recursive walk. Worth knowing the
number before anyone reopens that debate.

## Limits

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
  would fix that.
- **Duplicate groups are whole-corpus.** The scan is one pass per
  side and groups in RAM, which is the workflow `STORAGE_NOTES.md`
  intends, but it is still O(corpus) and holds every above-threshold
  row in memory.
- **Scans are compared as-is.** `scan_meta.options_fingerprint` tells
  two scans taken under different `ignore` rules apart, but this tool
  does not read it, so comparing two differently-configured scans
  reports ignored files as added or deleted.
