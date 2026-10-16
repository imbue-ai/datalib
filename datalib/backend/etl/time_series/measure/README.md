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

Each series is encoded as:

- **Integers:** the raw values the FIT file holds (speed in mm/s,
  latitude in semicircles).
- **Floats:** the values in their unit, as a render stores them (speed
  in m/s, latitude in degrees), with the floats that have no short
  decimal form laid out four ways: raw 8-byte floats, XOR floats,
  binary-scaled integers, and whichever of the three makes the smallest
  blob, which is what the store writes.

"On disk" is a time-series store holding only those chunks, after
`dolt_commit` and `dolt_gc()`, divided by the number of values.

## Results

Measured 2026-10-16 on one person's 13 activities: 167 series,
257,694 values at one a second, 21,228 of them latitude or longitude.
The FIT files themselves take 5.32 bytes a value.

| points a chunk | as | deflate | blob B/value | lat/long B/value | rest B/value | on disk B/value | decode ns/value |
|---|---|---|---|---|---|---|---|
| 256 | integers | no | 0.98 | 1.39 | 0.94 |  | 4.0 |
| 256 | integers | when smaller | 0.73 | 1.39 | 0.67 | 1.09 | 12.4 |
| 256 | floats, raw | no | 1.48 | 8.05 | 0.89 |  | 2.6 |
| 256 | floats, raw | when smaller | 0.90 | 3.68 | 0.65 | 1.27 | 13.7 |
| 256 | floats, XOR | no | 1.06 | 2.95 | 0.89 |  | 4.0 |
| 256 | floats, XOR | when smaller | 0.84 | 2.94 | 0.65 | 1.21 | 12.0 |
| 256 | floats, binary | no | 0.94 | 1.40 | 0.89 |  | 3.0 |
| 256 | floats, binary | when smaller | 0.71 | 1.40 | 0.65 | 1.08 | 11.0 |
| 256 | floats, smaller | no | 0.94 | 1.40 | 0.89 |  | 3.2 |
| 256 | floats, smaller | when smaller | 0.71 | 1.40 | 0.65 | 1.08 | 11.8 |
| 1024 | integers | no | 1.19 | 1.36 | 1.18 |  | 2.9 |
| 1024 | integers | when smaller | 0.63 | 1.36 | 0.57 | 0.77 | 7.7 |
| 1024 | floats, raw | no | 1.70 | 8.02 | 1.13 |  | 2.9 |
| 1024 | floats, raw | when smaller | 0.79 | 3.44 | 0.55 | 0.93 | 10.2 |
| 1024 | floats, XOR | no | 1.29 | 3.06 | 1.13 |  | 3.5 |
| 1024 | floats, XOR | when smaller | 0.74 | 2.90 | 0.55 | 0.88 | 9.8 |
| 1024 | floats, binary | no | 1.15 | 1.36 | 1.13 |  | 2.8 |
| 1024 | floats, binary | when smaller | 0.62 | 1.36 | 0.55 | 0.75 | 8.3 |
| 1024 | floats, smaller | no | 1.15 | 1.36 | 1.13 |  | 3.0 |
| 1024 | floats, smaller | when smaller | 0.62 | 1.36 | 0.55 | 0.75 | 8.4 |
| 4096 | integers | no | 1.35 | 1.37 | 1.34 |  | 2.7 |
| 4096 | integers | when smaller | 0.61 | 1.37 | 0.54 | 0.70 | 7.7 |
| 4096 | floats, raw | no | 1.84 | 8.01 | 1.29 |  | 2.7 |
| 4096 | floats, raw | when smaller | 0.76 | 3.41 | 0.52 | 0.85 | 7.8 |
| 4096 | floats, XOR | no | 1.44 | 3.08 | 1.29 |  | 4.2 |
| 4096 | floats, XOR | when smaller | 0.72 | 2.87 | 0.52 | 0.81 | 8.3 |
| 4096 | floats, binary | no | 1.30 | 1.37 | 1.29 |  | 2.9 |
| 4096 | floats, binary | when smaller | 0.59 | 1.37 | 0.52 | 0.68 | 6.8 |
| 4096 | floats, smaller | no | 1.30 | 1.37 | 1.29 |  | 3.2 |
| 4096 | floats, smaller | when smaller | 0.59 | 1.37 | 0.52 | 0.68 | 6.8 |

What it says:

1. **1,024 points a chunk.** With the smallest float layout, going
   from 1,024 to 4,096 saves 9% on disk and going down to 256 costs
   44%. At 1,024, an append rewrites at
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
4. **Latitude and longitude need the binary layout.** In degrees they
   have no short decimal form. As raw 8-byte floats, deflated, they
   cost 3.44 bytes a value at 1,024 points a chunk, 36% of all the
   bytes for 8% of the values. XOR floats (Gorilla's layout) bring that
   to 2.90. But a FIT latitude is semicircles times 180/2^31, which is
   45/2^29, so every value is exactly an integer over 2^29 and they all
   share the factor 45. Stored as those integers divided by 45, which
   is the semicircles again, they cost 1.36, what the raw integers
   cost. Taking the smallest layout per chunk, the store comes to 0.75
   bytes a value on disk, less than storing the raw integers (0.77).
   XOR never made the smallest blob on this data; it is kept for floats
   with neither a short decimal nor a small binary form.
