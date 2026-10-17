#!/usr/bin/env python3
"""Export every FIT `record` field of a Garmin source's activity files as
one time series per activity and field, one JSON object a line, for
`measure` to encode.

    python3 -I scripts/fit_time_series_export.py <garmin>/ingest/blobs.sqlite > series.jsonl

Each line: {"field": n, "t": [unix ms], "raw": [int], "scale": s,
"offset": o, "semicircles": bool}. A value in its unit is raw / scale -
offset, or raw * 180 / 2^31 degrees for a semicircle. The decoder reads
only what that takes: definition and data messages, compressed-timestamp
headers, developer fields skipped (the FIT protocol, garmin.com/fit)."""

import json
import sqlite3
import struct
import sys

FIT_EPOCH_S = 631_065_600
RECORD = 20
TIMESTAMP = 253

# base type -> (size, struct code, invalid value)
BASE = {
    0x00: (1, "B", 0xFF), 0x01: (1, "b", 0x7F), 0x02: (1, "B", 0xFF),
    0x83: (2, "h", 0x7FFF), 0x84: (2, "H", 0xFFFF), 0x85: (4, "i", 0x7FFFFFFF),
    0x86: (4, "I", 0xFFFFFFFF), 0x88: (4, "f", None), 0x89: (8, "d", None),
    0x0A: (1, "B", 0x00), 0x8B: (2, "H", 0x0000), 0x8C: (4, "I", 0x00000000),
    0x0D: (1, "B", 0xFF), 0x8E: (8, "q", 0x7FFFFFFFFFFFFFFF),
    0x8F: (8, "Q", 0xFFFFFFFFFFFFFFFF), 0x90: (8, "Q", 0),
}  # fmt: skip

# record field -> (scale, offset), from the FIT profile; others are 1, 0.
SCALES = {
    2: (5, 500), 5: (100, 0), 6: (1000, 0), 39: (10, 0), 41: (100, 0),
    53: (128, 0), 73: (1000, 0), 78: (5, 500), 83: (100, 0), 85: (10, 0),
}  # fmt: skip
SEMICIRCLES = {0, 1}


def records(buf):
    """Every record message of one FIT file, as {field: raw}."""
    pos, end = buf[0], buf[0] + struct.unpack_from("<I", buf, 4)[0]
    defs, out, last_ts = {}, [], None
    while pos < end:
        h = buf[pos]
        pos += 1
        if h & 0x80:  # compressed timestamp
            d = defs[(h >> 5) & 0x3]
            vals, pos = data(buf, pos, d)
            if last_ts is not None:
                ts = (last_ts & ~0x1F) + (h & 0x1F)
                if ts < last_ts:
                    ts += 0x20
                vals[TIMESTAMP] = last_ts = ts
        elif h & 0x40:  # definition
            endian = ">" if buf[pos + 1] else "<"
            glob = struct.unpack_from(endian + "H", buf, pos + 2)[0]
            n = buf[pos + 4]
            pos += 5
            fields = [tuple(buf[pos + 3 * i : pos + 3 * i + 3]) for i in range(n)]
            pos += 3 * n
            dev = 0
            if h & 0x20:
                nd = buf[pos]
                dev = sum(buf[pos + 2 + 3 * i] for i in range(nd))
                pos += 1 + 3 * nd
            defs[h & 0x0F] = {
                "global": glob,
                "endian": endian,
                "fields": fields,
                "dev": dev,
            }
            continue
        else:
            d = defs[h & 0x0F]
            vals, pos = data(buf, pos, d)
            if TIMESTAMP in vals:
                last_ts = vals[TIMESTAMP]
        if d["global"] == RECORD and TIMESTAMP in vals:
            out.append(vals)
    return out


def data(buf, pos, d):
    vals = {}
    for num, size, bt in d["fields"]:
        raw = buf[pos : pos + size]
        pos += size
        base = BASE.get(bt)
        if base is None or size != base[0]:  # a string or an array
            continue
        v = struct.unpack(d["endian"] + base[1], raw)[0]
        if base[2] is None or v != base[2]:
            vals[num] = v
    return vals, pos + d["dev"]


def main():
    db = sqlite3.connect(f"file:{sys.argv[1]}?mode=ro", uri=True)
    for (blob,) in db.execute("SELECT bytes FROM cas_objects"):
        recs = records(blob)
        fields = sorted({k for r in recs for k in r if k != TIMESTAMP})
        for f in fields:
            pts = [
                (r[TIMESTAMP], r[f]) for r in recs if f in r and isinstance(r[f], int)
            ]
            if not pts:
                continue
            scale, offset = SCALES.get(f, (1, 0))
            line = {
                "field": f,
                "t": [(t + FIT_EPOCH_S) * 1000 for t, _ in pts],
                "raw": [v for _, v in pts],
                "scale": scale,
                "offset": offset,
                "semicircles": f in SEMICIRCLES,
            }
            print(json.dumps(line))


if __name__ == "__main__":
    main()
