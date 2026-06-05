# CodeLab: Time-Traveling SQLite with `doltlite`

**Duration:** ~7 minutes
**You'll need:** `doltlite` on your PATH and a terminal.

`doltlite` is a drop-in `sqlite3` shell, with one superpower: every change can be committed, every commit can be named with a tag, and every pair of commits can be diffed. In this lab you'll build a tiny inventory table, evolve it through three tagged commits, then use SQL alone to see exactly what changed.

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

```sql
CREATE TABLE fruits (
  id    INTEGER PRIMARY KEY,
  name  TEXT,
  color TEXT,
  qty   INTEGER
);

INSERT INTO fruits VALUES
  (1, 'apple',  'red',    10),
  (2, 'banana', 'yellow',  5),
  (3, 'grape',  'purple', 20);

SELECT dolt_commit('-A', '-m', 'initial fruits');
SELECT dolt_tag('v1-initial');
```

`dolt_commit` returns a hash — that's our first commit. `dolt_tag` pins the name `v1-initial` to it so we never have to type the hash again. Like git, `-A` stages everything.

> **Talking point:** A `doltlite` database file is *also* a versioned repository. No external `.git` directory, no daemon — just one file.

---

## Step 2 — Edit, delete, insert — then commit and tag again

```sql
UPDATE fruits SET qty = 15 WHERE id = 1;            -- edit a row
DELETE FROM fruits WHERE id = 2;                     -- delete a row
INSERT INTO fruits VALUES (4, 'kiwi', 'green', 7);   -- add a row

SELECT dolt_commit('-A', '-m', 'tweak fruits');
SELECT dolt_tag('v2-tweaked');
```

---

## Step 3 — Drop the table and rebuild it differently

This is the "I broke everything and started over" scenario. The table physically disappears, then comes back in a new shape.

```sql
DROP TABLE fruits;

CREATE TABLE fruits (
  id    INTEGER PRIMARY KEY,
  name  TEXT,
  color TEXT,
  qty   INTEGER
);

INSERT INTO fruits VALUES
  (10, 'mango',     'orange', 12),
  (11, 'blueberry', 'blue',   50),
  (12, 'apple',     'red',    99);   -- apple is back, different id and qty

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
SELECT diff_type, from_id, from_name, from_qty, to_id, to_name, to_qty
FROM   dolt_diff_fruits
WHERE  from_commit = (SELECT tag_hash FROM dolt_tags WHERE tag_name = 'v1-initial')
  AND  to_commit   = (SELECT tag_hash FROM dolt_tags WHERE tag_name = 'v2-tweaked');
```

```
╭───────────┬─────────┬───────────┬──────────┬───────┬─────────┬────────╮
│ diff_type │ from_id │ from_name │ from_qty │ to_id │ to_name │ to_qty │
├───────────┼─────────┼───────────┼──────────┼───────┼─────────┼────────┤
│ modified  │       1 │ apple     │       10 │     1 │ apple   │     15 │
│ removed   │       2 │ banana    │        5 │       │         │        │
│ added     │         │           │          │     4 │ kiwi    │      7 │
╰───────────┴─────────┴───────────┴──────────┴───────┴─────────┴────────╯
```

> **Talking point:** the diff is a *queryable relation*. You can `JOIN`, `WHERE`, `GROUP BY` it like any other table — try `WHERE diff_type = 'modified'` to audit every cell change in a release.

---

## Step 7 — Diff across the "rebuild": `v2-tweaked` → `v3-rebuilt`

This is the dramatic one — the whole table contents were thrown away.

```sql
SELECT diff_type, from_id, from_name, to_id, to_name, to_qty
FROM   dolt_diff_fruits
WHERE  from_commit = (SELECT tag_hash FROM dolt_tags WHERE tag_name = 'v2-tweaked')
  AND  to_commit   = (SELECT tag_hash FROM dolt_tags WHERE tag_name = 'v3-rebuilt')
ORDER  BY diff_type, COALESCE(from_id, to_id);
```

Every old row shows up as `removed` and every new row as `added`. Even though we dropped and recreated the table, the diff understands the table identity by name — no row identity is lost.

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
SELECT to_id AS id, to_name AS name, to_color AS color, to_qty AS qty
FROM   dolt_diff_fruits
WHERE  to_commit = (SELECT tag_hash FROM dolt_tags WHERE tag_name = 'v1-initial')
  AND  diff_type = 'added';
```

(For the general case you'd typically check out a branch at that tag, but for a quick peek the diff view is enough.)

---

## Wrap-up — what you just saw

In ~30 lines of SQL you:

1. **Committed and tagged** schema + data changes into a single `.db` file.
2. **Inspected history** with `dolt_log` and `dolt_tags`.
3. **Diffed** any two tags at three resolutions: summary, stat, and row-level — all as ordinary tables you can `SELECT` from, addressed by human-readable names.
4. **Survived a destructive `DROP TABLE`** without losing the ability to compare against earlier state.

The whole repository is one file. `cp fruits.db backup.db` is a clone. Email it to a colleague and they have the full history *and* the tags.

---

## Optional encore (~1 min)

Branching:

```sql
SELECT dolt_branch('experiment');
SELECT dolt_checkout('experiment');
INSERT INTO fruits VALUES (99, 'durian', 'yellow', 1);
SELECT dolt_commit('-A', '-m', 'controversial addition');
SELECT dolt_checkout('main');   -- durian is gone
SELECT * FROM fruits;
```

Then `SELECT dolt_merge('experiment');` to bring it back.
