# CodeLab: Time-Traveling SQLite with `doltlite`

**Duration:** ~7 minutes
**You'll need:** `doltlite` on your PATH and a terminal.

`doltlite` is a drop-in `sqlite3` shell, with one superpower: every change can be committed, every commit can be named with a tag, and every pair of commits can be diffed. In this lab you'll build a tiny inventory table, evolve it through three tagged commits, then use SQL alone to see exactly what changed — including across a `DROP TABLE` that rebuilds the table from scratch.

---

## Step 0 — Start fresh

```bash
rm -f fruits.db
doltlite fruits.db
```

You're now in the `doltlite` shell. Turn on a nicer output mode:

```sql
.mode box
.headers on
```

---

## Step 1 — Create a table, commit, and tag it

The primary key is `name` — that's how Dolt will track each row's identity through edits, deletes, and even table rebuilds.

```sql
CREATE TABLE fruits (
  name        TEXT PRIMARY KEY,
  color       TEXT,
  qty         INTEGER,
  description TEXT
);

INSERT INTO fruits VALUES
  ('apple',  'red',    10, 'crisp and sweet'),
  ('banana', 'yellow',  5, 'soft and mild'),
  ('grape',  'purple', 20, 'tiny and juicy');

SELECT dolt_commit('-A', '-m', 'initial fruits');
SELECT dolt_tag('v1-initial');
```

`dolt_commit` returns a hash — that's our first commit. `dolt_tag` pins the name `v1-initial` to it so we never have to type the hash again. Like git, `-A` stages everything.

> **Talking point:** A `doltlite` database file is *also* a versioned repository. No external `.git` directory, no daemon — just one file.

---

## Step 2 — Edit, delete, insert — then commit and tag again

```sql
UPDATE fruits SET qty = 15 WHERE name = 'apple';              -- edit a row
DELETE FROM fruits WHERE name = 'banana';                      -- delete a row
INSERT INTO fruits VALUES
  ('kiwi', 'green', 7, 'fuzzy and tart');                      -- add a row

SELECT dolt_commit('-A', '-m', 'tweak fruits');
SELECT dolt_tag('v2-tweaked');
```

---

## Step 3 — Drop the table and rebuild it differently

This is the "I broke everything and started over" scenario. The table physically disappears, then comes back populated by hand. Notice that `apple` is among the rebuilt rows — but with a different `qty` and `description`.

```sql
DROP TABLE fruits;

CREATE TABLE fruits (
  name        TEXT PRIMARY KEY,
  color       TEXT,
  qty         INTEGER,
  description TEXT
);

INSERT INTO fruits VALUES
  ('mango',     'orange', 12, 'tropical and sweet'),
  ('blueberry', 'blue',   50, 'small and tangy'),
  ('apple',     'red',    99, 'orchard fresh');     -- apple is back, changed

SELECT dolt_commit('-A', '-m', 'rebuild from scratch');
SELECT dolt_tag('v3-rebuilt');
```

Nothing about the workflow changed — drop + recreate is just more SQL.

---

## Step 4 — Browse the history and the tags

```sql
SELECT commit_hash, message FROM dolt_log;
SELECT tag_name, date       FROM dolt_tags;
```

```
╭──────────────────────────────────────────┬────────────────────────────╮
│               commit_hash                │          message           │
├──────────────────────────────────────────┼────────────────────────────┤
│ …                                        │ rebuild from scratch       │
│ …                                        │ tweak fruits               │
│ …                                        │ initial fruits             │
│ …                                        │ Initialize data repository │
╰──────────────────────────────────────────┴────────────────────────────╯
╭────────────┬─────────────────────╮
│  tag_name  │        date         │
├────────────┼─────────────────────┤
│ v1-initial │ …                   │
│ v2-tweaked │ …                   │
│ v3-rebuilt │ …                   │
╰────────────┴─────────────────────╯
```

From here on, no hashes.

---

## Step 5 — High-level diff: `v1-initial` → `v2-tweaked`

```sql
SELECT * FROM dolt_diff_summary('v1-initial', 'v2-tweaked');
SELECT * FROM dolt_diff_stat   ('v1-initial', 'v2-tweaked');
```

`dolt_diff_summary` tells you *which tables* changed. `dolt_diff_stat` breaks it down to rows-added / rows-modified / rows-deleted / cells-modified. For our edit+delete+insert, expect 1 added, 1 deleted, 1 modified.

---

## Step 6 — Row-level diff: `v1-initial` → `v2-tweaked`

Every versioned table `T` gets a virtual companion table `dolt_diff_T` with `to_*` and `from_*` columns for every column in `T`, plus `to_commit`, `from_commit`, and `diff_type`.

One wrinkle: `from_commit` / `to_commit` store the resolved *hash*, not the tag name, so we look the tag up inline with a scalar subquery against `dolt_tags`. (You can't wrap this in a `CREATE VIEW` — `doltlite`'s safe mode disallows joining two virtual tables in a view definition.)

```sql
SELECT diff_type, from_name, from_qty, from_description,
                  to_name,   to_qty,   to_description
FROM   dolt_diff_fruits
WHERE  from_commit = (SELECT tag_hash FROM dolt_tags WHERE tag_name = 'v1-initial')
  AND  to_commit   = (SELECT tag_hash FROM dolt_tags WHERE tag_name = 'v2-tweaked');
```

```
╭───────────┬───────────┬──────────┬──────────────────┬─────────┬────────┬──────────────────╮
│ diff_type │ from_name │ from_qty │ from_description │ to_name │ to_qty │  to_description  │
├───────────┼───────────┼──────────┼──────────────────┼─────────┼────────┼──────────────────┤
│ modified  │ apple     │       10 │ crisp and sweet  │ apple   │     15 │ crisp and sweet  │
│ removed   │ banana    │        5 │ soft and mild    │         │        │                  │
│ added     │           │          │                  │ kiwi    │      7 │ fuzzy and tart   │
╰───────────┴───────────┴──────────┴──────────────────┴─────────┴────────┴──────────────────╯
```

> **Talking point:** the diff is a *queryable relation*. You can `JOIN`, `WHERE`, `GROUP BY` it like any other table — try `WHERE diff_type = 'modified'` to audit every cell change in a release.

---

## Step 7 — Diff across the "rebuild": `v2-tweaked` → `v3-rebuilt`

This is the headline result. We dropped the whole table and re-inserted rows by hand — but because the primary key is `name`, Dolt still recognizes `apple` as the *same row*, and reports just the cells that changed.

```sql
SELECT diff_type, from_name, from_qty, from_description,
                  to_name,   to_qty,   to_description
FROM   dolt_diff_fruits
WHERE  from_commit = (SELECT tag_hash FROM dolt_tags WHERE tag_name = 'v2-tweaked')
  AND  to_commit   = (SELECT tag_hash FROM dolt_tags WHERE tag_name = 'v3-rebuilt')
ORDER  BY diff_type, COALESCE(from_name, to_name);
```

```
╭───────────┬───────────┬──────────┬──────────────────┬───────────┬────────┬────────────────────╮
│ diff_type │ from_name │ from_qty │ from_description │  to_name  │ to_qty │   to_description   │
├───────────┼───────────┼──────────┼──────────────────┼───────────┼────────┼────────────────────┤
│ added     │           │          │                  │ blueberry │     50 │ small and tangy    │
│ added     │           │          │                  │ mango     │     12 │ tropical and sweet │
│ modified  │ apple     │       15 │ crisp and sweet  │ apple     │     99 │ orchard fresh      │
│ removed   │ grape     │       20 │ tiny and juicy   │           │        │                    │
│ removed   │ kiwi      │        7 │ fuzzy and tart   │           │        │                    │
╰───────────┴───────────┴──────────┴──────────────────┴───────────┴────────┴────────────────────╯
```

> **Talking point:** identity follows the primary key, not the underlying storage. A `DROP TABLE` followed by a fresh `INSERT` of the same `name` is a `modified` row, not a `removed`+`added` pair. Schema design (which columns make up the PK) directly determines what "the same row" *means* to your diff history.

---

## Step 8 — Diff across the entire history: `v1-initial` → `v3-rebuilt`

```sql
SELECT * FROM dolt_diff_stat('v1-initial', 'v3-rebuilt');
```

One query, two commits apart, full accounting of net change.

---

## Step 9 — Time travel by reading old state

You're not stuck looking at diffs — you can read a table *as it was*:

```sql
-- snapshot from v1-initial
SELECT to_name AS name, to_color AS color, to_qty AS qty, to_description AS description
FROM   dolt_diff_fruits
WHERE  to_commit = (SELECT tag_hash FROM dolt_tags WHERE tag_name = 'v1-initial')
  AND  diff_type = 'added';
```

(For the general case you'd typically check out a branch at that tag, but for a quick peek the diff table is enough.)

---

## Wrap-up — what you just saw

In ~30 lines of SQL you:

1. **Committed and tagged** schema + data changes into a single `.db` file.
2. **Inspected history** with `dolt_log` and `dolt_tags`.
3. **Diffed** any two tags at three resolutions: summary, stat, and row-level — all as ordinary tables you can `SELECT` from, addressed by human-readable names.
4. **Preserved row identity across a destructive `DROP TABLE`**, because the primary key — not the physical storage — defines what "the same row" means.

The whole repository is one file. `cp fruits.db backup.db` is a clone. Email it to a colleague and they have the full history *and* the tags.

---

## Optional encore (~1 min)

Branching:

```sql
SELECT dolt_branch('experiment');
SELECT dolt_checkout('experiment');
INSERT INTO fruits VALUES ('durian', 'yellow', 1, 'pungent and divisive');
SELECT dolt_commit('-A', '-m', 'controversial addition');
SELECT dolt_checkout('main');   -- durian is gone
SELECT * FROM fruits;
```

Then `SELECT dolt_merge('experiment');` to bring it back.
