# What does folding a store's history save?

The question: a raw store keeps every checkpoint's pages
(imbue-ai/datalib#1259). If its history is rebuilt as one commit per run,
or per recent run, or as one commit, how much does `dolt_gc()` then free,
how long does it take, and how much free disk does it need? And what does
a reader that is already open see while a peer folds and collects?

`scripts/doltlite_history_fold.py` builds a store shaped like the JMAP
email raw store, made-up data only, against the Bazel-built doltlite
CLI, and measures each strategy on a copy. The plan these numbers feed
is [`docs/dev/plans/history_folding.md`](../../docs/dev/plans/history_folding.md).

```sh
bazelisk build //third-party/doltlite:doltlite
python3 scripts/doltlite_history_fold.py [scratch_dir]
```

The store: emails keyed in write order, threads and an
`emails_by_thread` index keyed at random, a bookkeeping sidecar updated
at random. One backfill run seals 80 times, then 90 daily runs seal
twice each; 136,000 emails and 92,500 threads at the end. Needs about
3 GB free in the scratch directory; takes about a minute.

## Results — doltlite 0.50.16, 2026-10-20, an M-series mac

```
strategy                 commits  size after gc  fold    gc     most the directory held during gc
as stored                    262     1,035.8 MB
gc only                      262       988.2 MB  —       12.1 s  2,024.0 MB
fold to run ends              93       546.7 MB  1.3 s    7.3 s  1,582.8 MB
keep the last 30 run ends     33       258.3 MB  0.4 s    1.8 s  1,294.3 MB
keep the last 7 run ends      10       144.9 MB  0.2 s    0.8 s  1,180.8 MB
fold to one commit             3       110.0 MB  0.1 s    0.6 s  1,145.8 MB
```

Every strategy kept every row (136,000 emails, 92,500 threads), and each
rebuilt run end had the same `dolt_hashof_db` as the commit it replaced.
The directory peaked at the old file plus the compacted one: gc needs the
compacted size in free disk.

Readers, against the store folded to run ends and then collected:

```
detached reader at a mid-run seal, before: 135800|67900000 | 92375
held read transaction on main, before    : 136000|68000000 | 92500
detached reader, after the fold and gc   : 135800|67900000 | 92375
held transaction, still open             : 136000|68000000 | 92500
held reader's next transaction           : 7 | 136000|68000000 | 92501
a new open of the folded-away seal       : branch or revision "<hash>" not found
```

An open reader keeps reading what it opened (gc renames a new file over
the old one, and the reader still holds the old one); a held reader's
next transaction moves to the new file and sees a later write. These are
shell probes, not sqlx.
