# What do time-series points cost in doltlite?

The question: a time-series index holds millions of points, and every
sync appends a few to each series. Can the index replace a source's
points wholesale, or must it write only what changed? And what does a
point cost on disk?

`scripts/doltlite_time_series_cost.py` loads 1,000 series of equal
length into `time_series_points` keyed `(time_series_id, t)`, then
applies one sync of 10 new points per series three ways. The design
that rests on these numbers is
[`docs/dev/plans/completed/time_series.md`](../../docs/dev/plans/completed/time_series.md).

```sh
bazelisk build //third-party/doltlite:doltlite
python3 scripts/doltlite_time_series_cost.py <scratch> 1000000 5000000 20000000
INT_KEYS=1 python3 scripts/doltlite_time_series_cost.py <scratch> 1000000 5000000 20000000
```

## Results — doltlite 0.50.14, Apple M4, macOS 27.0.1

The series key is a 36-character uuid string:

```
  1,000,000 points  load    0.9s  rss    295.3 MB  after gc    115.1 MB
  replace every series                 0.88s  rss    193.0 MB  written     14.2 MB  kept after gc     13.1 MB
  replace one series                   0.02s  rss      9.9 MB  written      1.0 MB  kept after gc      0.0 MB
  append new points only               0.07s  rss     58.1 MB  written     14.1 MB  kept after gc     13.1 MB
  range read, one series                  8 ms  (1001 rows)
  same, downsampled to 100 buckets        8 ms  (100 rows)

  5,000,000 points  load    4.7s  rss    695.4 MB  after gc    575.5 MB
  replace every series                 4.54s  rss    197.1 MB  written     21.2 MB  kept after gc     15.5 MB
  replace one series                   0.03s  rss     11.8 MB  written      4.9 MB  kept after gc      0.0 MB
  append new points only               0.10s  rss     75.9 MB  written     20.5 MB  kept after gc     15.5 MB
  range read, one series                 12 ms  (5001 rows)
  same, downsampled to 100 buckets       11 ms  (100 rows)

 20,000,000 points  load   20.6s  rss   1445.2 MB  after gc   2302.0 MB
  replace every series                21.46s  rss    237.7 MB  written     39.2 MB  kept after gc     16.7 MB
  replace one series                   0.23s  rss     20.8 MB  written     19.7 MB  kept after gc      0.0 MB
  append new points only               0.47s  rss    109.2 MB  written     36.5 MB  kept after gc     16.7 MB
  range read, one series                 64 ms  (20001 rows)
  same, downsampled to 100 buckets       26 ms  (100 rows)
```

The series key is an integer (`INT_KEYS=1`):

```
  1,000,000 points  load    0.5s  rss    176.0 MB  after gc     48.2 MB
  replace every series                 0.47s  rss    136.8 MB  written      8.8 MB  kept after gc      8.2 MB
  replace one series                   0.01s  rss      9.8 MB  written      0.4 MB  kept after gc      0.0 MB
  append new points only               0.05s  rss     41.3 MB  written      8.7 MB  kept after gc      8.2 MB
  range read, one series                  8 ms  (1001 rows)
  same, downsampled to 100 buckets        7 ms  (100 rows)

  5,000,000 points  load    2.6s  rss    369.0 MB  after gc    241.1 MB
  replace every series                 2.58s  rss    179.9 MB  written     13.7 MB  kept after gc     11.0 MB
  replace one series                   0.02s  rss     10.9 MB  written      2.1 MB  kept after gc      0.0 MB
  append new points only               0.07s  rss     53.3 MB  written     13.1 MB  kept after gc     11.0 MB
  range read, one series                 10 ms  (5001 rows)
  same, downsampled to 100 buckets        9 ms  (100 rows)

 20,000,000 points  load   11.2s  rss    545.3 MB  after gc    964.4 MB
  replace every series                10.77s  rss    197.0 MB  written     23.1 MB  kept after gc     12.4 MB
  replace one series                   0.07s  rss     16.2 MB  written      8.3 MB  kept after gc      0.0 MB
  append new points only               0.10s  rss     71.6 MB  written     20.8 MB  kept after gc     12.4 MB
  range read, one series                 16 ms  (20001 rows)
  same, downsampled to 100 buckets       15 ms  (100 rows)
```

Reading it:

- **Bytes: replacing is as cheap as appending.** Deleting every point
  and inserting them again in one transaction writes the same pages as
  inserting only the new ones (14.2 MB against 14.1 MB at 1M), and gc
  keeps the same. The prolly tree ends in the same shape, so only the
  leaves holding new points change.
- **Time: replacing costs the size of what is replaced.** Replacing
  every series takes about a second per million points (21 s at 20M
  with string keys); appending takes under half a second at any size
  here. So an index replaces only the series whose points changed,
  not every series a source has.
- **The key's size is most of a point's size.** A uuid string key
  costs about 115 bytes a point; an integer key about 48. It also
  halves the load and replace times.
- **Reads are not the problem.** A range read of one series of 20k
  points takes 16 ms; downsampled to 100 buckets in SQL, 15 ms.
- "Replace one series" keeps 0.0 MB after gc because 10 new points
  round to nothing at this precision.
