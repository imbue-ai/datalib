# Lightroom backups you can keep often, store small, and compare

Adobe Lightroom Classic backs up its catalog by writing a **full copy**
of it each time. Keep them often and they fill your disk. Keep them
rarely and you can never say what changed between two of them.

This is the story of keeping every backup in a database that stores
each version, saves only what changed, and lets you **compare any two
backups with one query**. It covers what that looks like in practice,
what it cost, what the comparisons showed on a real library, and the
mistakes along the way.

It is written for people who use these tools: Lightroom users who want
their backups to be cheap and answerable, and doltlite users who want to
see how a real versioned database behaves. If you have never heard of
either one, start at the top; the first part assumes nothing.

Every number below is a count, a size or a timing from a real library.
No catalog content, no file names and no paths appear here.

## The result, up front

Ten Lightroom catalog backups, from 0.9 to 6.0 GB each (about 32 GiB in
all), kept in one file:

| | |
| --- | --- |
| The ten catalogs, side by side | about 32 GiB |
| The versioned store, as written | 12.38 GiB |
| The same store after a cleanup (`dolt_gc`) | **7.86 GiB**, about a quarter of the originals |
| Comparing any two backups | one SQL query |
| What changed over four weekly backups | under 1% of any sizeable table |

You get every backup, each one still queryable as it was, for a quarter
of the space. The saving and the comparisons come from the same fact:
between two backups almost nothing changes, and the database stores
each piece of data once.

## Part 1. Starting from nothing

### What Lightroom keeps, and why a backup is large

Lightroom Classic is a photo manager. Your pictures stay where they are.
Lightroom keeps a **catalog**, one file ending in `.lrcat`, that records
everything it knows about them: where each file is, its ratings,
keywords and collections, the faces it detected, and every edit you have
made. The catalog holds no pixels, yet a library of tens of thousands of
photos produces a catalog of gigabytes. The ones here run from about
1 GB to 6 GB.

Lightroom can back the catalog up on a schedule. Each backup is a folder
named for when it was taken (`2026-09-27 1650`) holding a zipped copy of
the whole catalog. Nothing in it says what changed since the last one. A
backup that differs from the one before by twenty edited photos costs
about as much as one that differs by twenty thousand.

### SQLite, in one paragraph

A `.lrcat` is an ordinary **SQLite** database. SQLite is a small
database library that keeps a whole database, with its tables and rows,
in one file and needs no server. A great deal of desktop software uses
it (Apple Photos, Quicken for Mac, Things, and others). Two things
matter here. You can open a catalog with any SQLite tool and look at its
tables, with no Adobe software involved. And SQLite is *dynamically
typed*: a column's declared type is a hint, and one column may hold a
number in one row and text in the next.

### doltlite, from first principles

[Doltlite](https://github.com/dolthub/doltlite) is SQLite with the
storage and the version control of [Dolt](https://github.com/dolthub/dolt)
built in. You write ordinary SQL against what looks like an ordinary
database file, and the file also has a **history**, shaped like git's:
commits, and comparisons between any two of them, all available from SQL.
Three ideas make it work, and the rest of this document leans on them.

**1. A table is stored as chunks named by their content.** The rows of a
table are kept in a *prolly tree*, a kind of B-tree whose pages
(**chunks**) are stored under the hash of their own contents. In a prolly
tree the boundaries between chunks come from the data itself, so
inserting a row in the middle changes the chunk that holds it and leaves
its neighbours alone. A chunk is never edited in place: a write produces
a new chunk for the changed rows, plus a new copy of each page on the
path up to the root.

**2. A commit names one root.** A commit is a tiny record pointing at the
root of every table at that moment. Two commits that share most of their
data share most of their chunks, because identical content has an
identical hash and is stored once. This is why ten backups of a catalog
that barely changes cost far less than ten copies.

**3. A comparison costs what changed.** To compare two commits, the
engine can walk the two trees together and skip every part whose hash is
the same, which is what a content-addressed tree allows. Comparing two
versions of a million-row table in which one row changed takes under
10 milliseconds by doltlite's own measurement.

The price is that nothing is ever updated in place, so a store that has
been written to carries chunks that no commit reaches any more. They stay
until you run a cleanup, `dolt_gc()`. Part 4 is about how much that is.

### How the pieces fit

```text
 Lightroom Classic
   writes a full backup each time
        |
        v
 Backups/2026-09-20 1650/Catalog.zip    <- one zipped catalog per backup
 Backups/2026-09-27 1650/Catalog.zip
        |
        |  for each backup, oldest first (read-only)
        v
 one doltlite file, one commit per backup
 (chunks shared between commits)
        |
        v
 any two backups:  which tables changed, by how much     -> dolt_diff_stat
                   which rows, which columns             -> dolt_diff_<table>
```

The tool that does this is [datalib](https://github.com/imbue-ai/datalib),
an open-source (MIT) program that mirrors a person's own data from many
services and files into local, queryable stores. Its Lightroom source is
what this case study is about.

## Part 2. What happens when you point it at your backups

You point a Lightroom source at your `Backups` folder. Each sync:

- finds every backup the store does not hold yet and mirrors them
  **oldest first, one commit each**, dated when the backup was taken, so
  the history reads in the order your catalog really changed;
- ends on the newest state, so the latest data is always what you see
  when you query the store;
- **only reads**. It never writes to your catalog, your backups or your
  photos. A live catalog is read from a consistent copy, so it is safe
  to run while Lightroom is open;
- knows a backup by its contents, not its name, so renaming a backup's
  folder to add a note changes nothing;
- mirrors the **catalog only**: not your photos, and not Lightroom's
  previews.

Each table of the catalog is copied whole into the store on every run. It
sounds wasteful, and it is not: rewriting a table with the same rows is
no change to doltlite, so an unchanged catalog produces no commit at all.
Whatever the catalog says today becomes the newest state, and the history
builds up behind it.

### Why the comparisons needed one more idea: keys

This is the part that took the longest to get right, and the part most
worth reading if you build anything on a versioned database.

A comparison pairs the rows of one backup with the rows of another, and
it pairs them by **primary key**, the column (or columns) that says which
row is which. If a table has one, "row 41 changed its rating" shows as
one changed cell. If it does not, doltlite falls back on a hidden row
number, and the comparison pairs row *n* with row *n*. Insert a photo
near the start and every later row has a new number, so the comparison
reads as a long run of changes. The data is identical; the answer is not.

Lightroom's tables fall into three groups:

- **Most have a stable ID** beside the ordinary one: a UUID (`id_global`)
  that never changes, next to an integer (`id_local`) that Lightroom is
  free to renumber, for example when it upgrades a catalog. Keying on the
  stable one means a renumbering would show as one changed column, not as
  every row deleted and re-added. In the backups we examined, the integer
  never changed for existing rows, so this was insurance, not rescue. We
  checked one large table.
- **Some have no primary key at all**, mostly the cloud-sync tables. They
  compare by position, and they compare badly. One table of about 227,000
  small rows showed 121,114 of 224,500 rows "modified" between two weekly
  backups. We first believed its contents changed on every backup. They
  did not: fewer than 20,000 differed, and the rest were rows that had
  merely moved when others were added.
- **But Lightroom does record the key** for those tables, as a UNIQUE
  index named `index_<Table>_primaryKey`, for example on `(image,
  payloadKey)`. It was in the catalog all along; we had only been
  reading declared primary keys. A UNIQUE index is not quite a primary
  key. It allows empty (NULL) values to repeat, and a table may have
  several. So the rule is cautious: use a table's **only** UNIQUE index;
  check that no row has a NULL in it; otherwise (two or more, or a NULL)
  leave the table without a key and say so, because with several none is
  more the key than another. It never stops a sync, and the same rule
  now applies to the other SQLite-backed sources in datalib.

With the key in place, the same table showed **128 modified rows across
four weekly backups**.

## Part 3. What the comparisons show

This is the payoff. Between any two backups, one query lists the tables
that changed:

```sql
SELECT table_name, rows_added, rows_deleted, rows_modified, cells_modified
FROM dolt_diff_stat('<old commit>', '<new commit>')
ORDER BY rows_added + rows_deleted + rows_modified DESC LIMIT 10;
```

`cells_modified / rows_modified` is the quickest signal: near 1.0 means
one column changed in every modified row. A second query,
`dolt_diff_<table>('<old>', '<new>')`, shows the rows themselves, and
`pragma_table_info('<table>')` lists a table's columns. Everything below
was found with those, without reading a single value.

### Weekly backups, before and after keys

Four weekly backups from 2026, one pair at a time. "Modified" means a row
present in both backups whose contents differ.

| | Before keys | After keys |
| --- | --- | --- |
| Rows modified, all tables, per weekly pair | 126,000 to 224,000 | about 23,000 across all four pairs together |
| The table above, one pair | 121,114 of 224,500 rows | 128 across four pairs |
| Largest churn in a sizeable table | one table at 54% in one pair | none above 1% over all four pairs (a few tiny counter tables change every time) |
| Images added across the four pairs | counts inflated by shifted rows | 1,472 added, 28 deleted |

The two sets of four pairs are comparable but not identical: they differ
by one backup. And note what did *not* change: the store was
13,293,076,212 bytes before and 13,293,589,085 after, half a megabyte
apart (the second store was built from scratch, so they are not the same
file, but the catalogs and their order were the same). The tables whose
comparisons we cleaned up were never what filled the file. What improved
is that a comparison now says what actually happened.

### What real changes look like

With the noise gone, the changes are legible and match what a person did:

- **New photos are rows added across many tables.** Each new image adds a
  row to each per-image table. The counts reconcile exactly: old rows
  plus added minus deleted equals new rows.
- **A new Lightroom version can store a column differently.** Between two
  catalogs from different Lightroom generations, one develop-history
  column went from text to a shorter, opaque binary form in all 132,342
  rows, while the columns beside it did not change. The average length
  fell from 941 to 385, which fits Lightroom compressing the value in a
  newer version. We did not identify the format and did not decompress
  it. That column is stored again once, and the next catalog in the same
  format stores no rewrite. A comparison where every row's storage type
  changed is the signature, and it is a one-time cost of the upgrade.
- **Reprocessing rewrites rows.** After a face-detection model change,
  the face tables showed nearly every row modified in about one column
  each (76,439 of 76,509 rows in one table). That is real change in the
  catalog.
- **Small tables look dramatic and are not.** A counter table with a few
  hundred rows changes on every backup, and a one-row sync counter always
  reads as 100% modified. Read the counts, not the percentages.

## Part 4. Where the space goes

### The expectation, and what happened

The pitch for storing backups this way is that each one costs only what
changed. That held: about 32 GiB of catalogs became a 12.38 GiB store.
But the first explanation we reached for, that the weekly changes were
filling the file, was wrong, as the half-megabyte figure above shows. The
size was something else.

### Garbage, and how to get it back

Because a chunk is never edited in place, a store accumulates chunks that
no commit reaches. `dolt_gc()` finds and deletes them. We ran it on a
copy of the 12.38 GiB store:

| | |
| --- | --- |
| Before | 13,293,589,085 bytes (12.38 GiB) |
| After | 8,443,844,496 bytes (7.86 GiB) |
| Reclaimed | 4,849,744,589 bytes, about 36% |
| Chunks | 783,443 removed, 973,001 kept (45% of all chunks unreachable) |
| Time | 2 min 41 s on an Apple Silicon laptop |

That takes the store from about 0.4 of the originals' size to about 0.25.
Collecting a copy is a safe way to measure it: the original is never
touched.

We have not traced exactly which writes leave the garbage. What we know
about the cleanup comes mostly from doltlite's own documentation, and the
measurement is consistent with it:

- **A commit keeps whatever its transaction wrote.** Commit once at the
  end of a batch and the cleanup reclaims every intermediate page. Commit
  after each of many small transactions and it reclaims nothing, because
  each in-between state is now history. This tool rebuilds each table in
  its own transaction and makes one commit when a backup is done.
- **The cleanup can run before or after a commit.** After one, it
  reclaims that run's own garbage.
- **It writes a compacted copy before dropping the original**, so it
  needs about the store's size in free disk for a moment.
- **It rewrites the whole file and takes minutes at this size**, so it is
  a deliberate step, not something to do after every row.
- **Deleting a row reclaims nothing.** The row stays reachable from the
  earlier commits. Space comes back only from chunks nothing reaches.

In datalib this is the **"Collect unreachable chunks each sync"** option
of a source, and it is off by default.

## Part 5. Try it on your own backups

Everything above can be measured from a store's metadata, without reading
any of your data. Open the store read-only (`datalib-doltlite -readonly
<store>`); a writable session lands on the branch every reader reads.

```sql
-- the backups, oldest first
SELECT commit_hash, substr(message, 1, 120), date FROM dolt_log ORDER BY date;

-- which tables changed between two of them
SELECT table_name, rows_added, rows_deleted, rows_modified
FROM dolt_diff_stat('<old>', '<new>')
ORDER BY rows_added + rows_deleted + rows_modified DESC LIMIT 20;

-- which tables have no key (they will compare by position)
SELECT m.name FROM sqlite_master m
WHERE m.type = 'table'
GROUP BY m.name
HAVING sum((SELECT count(*) FROM pragma_table_info(m.name) WHERE pk > 0)) = 0;

-- one table's key, and whether a column changed storage type between two backups
SELECT name FROM pragma_table_info('<table>') WHERE pk > 0;
SELECT count(*), sum(typeof(from_c) IS NOT typeof(to_c))
FROM dolt_diff_<table>('<old>', '<new>') WHERE diff_type = 'modified';
```

To measure garbage: copy the store file, run `SELECT dolt_gc();` on the
copy, and compare sizes.

## Things to know if you use Lightroom

- **Your first comparison after an upgrade will be large, and that is
  normal.** A new Lightroom version can store some columns differently,
  and a new face-detection model rewrites the face tables. Both show up
  once as a big change between two backups, then settle.
- **Keep your `Backups` folder.** The store is built from it. Lightroom's
  own backup settings are untouched, and zipped backups stay as they are.
- **Plan some free disk.** The store is a quarter of your backups after a
  cleanup, but the cleanup itself needs about the store's size free for a
  moment.
- **Only the catalog is kept**: its settings, keywords, ratings, edits and
  face data, not your photos and not the previews.
- **Old backups become a record.** Because each backup is one commit, you
  can ask when something first appeared or last changed, not only what is
  there now. `INGEST.md` beside this file has worked queries.

## Things to know if you use doltlite

- **Give your tables a primary key if you want to compare them.** A table
  without one is compared by row position, so an insert or a delete in
  the middle looks like a rewrite of everything after it. This was the
  largest source of noise here.
- **A key should be a stable identity, not just something unique.** A
  UNIQUE index allows repeated NULLs and a table can have several, so
  check that there is one, and that it really names a row.
- **Rewriting a table with the same rows is no change.** That makes
  "drop and refill everything" a cheap way to mirror a source whose
  changes you cannot track.
- **Commit once per batch.** Many small commits keep every intermediate
  state alive and leave the cleanup nothing to reclaim.
- **Plan for garbage.** An uncollected store grew to about one and a half
  times its real size here. Run `dolt_gc()` periodically, with no writer
  running, and keep free disk about the size of the store.
- **Chunks are not compressed today** (an open upstream issue, as far as
  this repository's notes go), so a store is roughly as large as the
  distinct data it holds.
- **Open a store read-only to look at it.** Close and reopen a long-lived
  connection after a cleanup; we have not verified what an already-open
  connection sees.

## What we got wrong on the way

Each wrong turn was cheap to make, and the data corrected it.

- **We blamed renumbering.** A table that changed in every row looked like
  Lightroom renumbering its IDs. A per-column count showed no ID had
  changed; a different column had.
- **Then we blamed the data.** The next guess was that the contents really
  changed on every backup. The counts said fewer than a fifth of the
  "modified" rows had changed contents; the rest had only moved.
- **We believed the weekly changes were the size.** They were not
  (Part 4).

## What we have not verified

- The numbers come from one set of ten catalogs on one machine. Another
  library will differ in size and shape.
- We measured the cleanup once, on a copy.
- The free-disk need of the cleanup comes from doltlite's documentation,
  not from our own measurement.
- The same key rule now applies to datalib's other SQLite-backed sources
  (Apple Photos, Apple Messages, WhatsApp). We have not measured their
  comparisons the way we did Lightroom's.

## Where to read more

- [`INGEST.md`](INGEST.md) beside this file: every option of the Lightroom
  source, the backups-folder rules, the key rules and the cleanup.
- [`docs/dev/doltlite.md`](/docs/dev/doltlite.md): what doltlite does,
  fact by fact, each backed by a test.
