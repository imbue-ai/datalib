# What the chunk encoding costs

`measure_chunks` encodes real activity data with the chunk encoding in
`../src/chunk.rs` and prints what it costs at three chunk sizes. The
encoding itself is described in `docs/dev/time_series.md` §"The chunk
encoding".

## Running it

The input is a Garmin source's FIT activity files, exported one time
series per activity and record field:

```sh
python3 -I scripts/fit_time_series_export.py \
    <root>/<garmin source>/ingest/blobs.sqlite > /tmp/series.jsonl
bazelisk build -c opt //datalib/backend/etl/time_series:measure_chunks
bazel-bin/datalib/backend/etl/time_series/measure_chunks /tmp/series.jsonl /tmp/measure
```

Each series is encoded twice:

- **As integers:** the raw values the FIT file holds (speed in mm/s,
  latitude in semicircles).
- **As floats:** the values in their unit, as a render stores them
  (speed in m/s, latitude in degrees).

"On disk" is a time-series store holding only those chunks, after
`dolt_commit` and `dolt_gc()`, divided by the number of values.

## Results

Measured 2026-10-16 on one person's 13 activities: 167 series,
257,694 values at one a second, 21,228 of them latitude or longitude.
The FIT files themselves take 5.32 bytes a value.

| points a chunk | as | deflate | blob B/value | lat/long B/value | rest B/value | on disk B/value | decode ns/value |
|---|---|---|---|---|---|---|---|
| 256 | integers | no | 0.98 | 1.39 | 0.94 |  | 3.9 |
| 256 | integers | when smaller | 0.73 | 1.39 | 0.67 | 1.09 | 12.3 |
| 256 | floats | no | 1.48 | 8.05 | 0.89 |  | 2.8 |
| 256 | floats | when smaller | 0.90 | 3.68 | 0.65 | 1.27 | 16.2 |
| 1024 | integers | no | 1.19 | 1.36 | 1.18 |  | 2.9 |
| 1024 | integers | when smaller | 0.63 | 1.36 | 0.57 | 0.77 | 8.6 |
| 1024 | floats | no | 1.70 | 8.02 | 1.13 |  | 2.8 |
| 1024 | floats | when smaller | 0.79 | 3.44 | 0.55 | 0.93 | 9.9 |
| 4096 | integers | no | 1.35 | 1.37 | 1.34 |  | 3.0 |
| 4096 | integers | when smaller | 0.61 | 1.37 | 0.54 | 0.70 | 7.4 |
| 4096 | floats | no | 2.15 | 8.01 | 1.63 |  | 3.0 |
| 4096 | floats | when smaller | 0.76 | 3.41 | 0.52 | 0.85 | 8.7 |

What it says:

1. **1,024 points a chunk.** Going from 1,024 to 4,096 saves 8% on
   disk. Going down to 256 costs 37%. At 1,024, an append rewrites at
   most a kilobyte or so, and a range read decodes at most two chunks
   it does not need whole.
2. **Deflate pays.** Without it, bit-packing alone grows with the chunk
   size, because one large change sets the width for the whole chunk.
   Deflate takes out most of that, at about 10 ns a value to decode:
   10 ms for a million points.
3. **Floats with few decimals cost what integers cost** (0.55 against
   0.57 bytes a value, latitude and longitude aside), because they are
   stored as integers over a power of ten. That only works when the
   render computes a value without rounding error. For an altitude
   stored as 2501 (scale 5, offset 500), `(2501 - 2500) / 5` is 0.2,
   while `2501 / 5 - 500` is 0.19999999999998863, which has no
   short decimal form and is stored as 8 raw bytes. Take an offset off
   before dividing.
4. **Latitude and longitude are the costly part.** In degrees they have
   no short decimal form, so they are stored as raw 8-byte floats and
   deflated: 3.44 bytes a value, against 1.36 as semicircles. They are
   8% of the values and 36% of the bytes. An XOR float encoding
   (Gorilla, Chimp) is the next thing to try for them, as a new
   encoding number.
