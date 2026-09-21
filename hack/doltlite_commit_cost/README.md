# What does a doltlite write cost, and does committing often make it worse?

The question this started from: a step that checkpoints every 15 s
(`datalib_etl::checkpointer`) makes many commits where it used to make
one. Same rows at the end — what does the extra history cost?

`scripts/doltlite_commit_cost.py` builds the same 100k-row table six ways against the
Bazel-built doltlite CLI and reports the file size before and after
`dolt_gc()`. The write-up is `datalib/backend/etl/README.md` § "What a
write costs"; this directory is the reproduction.

```sh
bazelisk build //third-party/doltlite:doltlite
python3 scripts/doltlite_commit_cost.py
```

## Results — doltlite 0.50.3, 2026-09-20

```
random keys, 200 txns, commit once                   commits=   3  before gc  430.1 MB  after gc   14.8 MB
random keys, ONE txn, commit once                    commits=   3  before gc   24.4 MB  after gc   14.8 MB
random keys, 200 txns, commit every 10 txns          commits=  22  before gc  430.1 MB  after gc  141.3 MB
random keys, 200 txns, commit every txn              commits= 202  before gc  430.3 MB  after gc  419.3 MB
  … squashed to one commit, gc again                 commits=   3                         after gc   14.8 MB
time-prefixed keys, 200 txns, commit once            commits=   3  before gc   16.9 MB  after gc   15.6 MB
time-prefixed keys, 200 txns, commit every txn       commits= 202  before gc   17.0 MB  after gc   16.8 MB
```

Reading it:

- Row 1 vs row 2: the **transaction** is what rewrites pages. The same
  statements in one transaction write a tree once.
- Rows 1, 3, 4: before gc the size is the same whatever the cadence —
  nothing reclaims pages until gc runs. After gc, each commit keeps the
  pages its transaction rewrote, so 200 commits keep nearly everything.
- Row 4's squash: `dolt_reset('--soft', base)` + one commit gives the
  same table hash and lets gc reclaim it all.
- Rows 5, 6: with keys that sort in write order, a transaction touches
  one or two leaves, and 200 commits cost ~1 MB over one.
