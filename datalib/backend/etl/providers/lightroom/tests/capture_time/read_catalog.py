"""Print what a test catalog holds for each case: `captureTime`,
`originalCaptureTime`, and every date in Lightroom's XMP for it.

Usage: python3 -I read_catalog.py <catalog.lrcat> [name-prefix]

Run it on a copy made while Lightroom is closed, or copy the `.lrcat` with
its `-wal` and `-shm` files and checkpoint the copy (`sqlite3 <copy>
'PRAGMA wal_checkpoint(TRUNCATE)'`): a catalog Lightroom has open keeps its
newest writes in the WAL. Only for a catalog of these test files; on a real
catalog it prints what the photos say.
"""

import re
import sqlite3
import sys
import zlib

DATE_ATTR = re.compile(r'([A-Za-z]+:[A-Za-z]*(?:Date|Time)[A-Za-z]*)="([^"]*)"')
SKIP = {"xmp:MetadataDate"}


# The catalog stores XMP as a 4-byte big-endian length, then zlib.
def xmp_dates(blob: bytes | None) -> str:
    if not blob:
        return ""
    text = zlib.decompress(blob[4:]).decode("utf-8", "replace")
    return "; ".join(f"{k}={v}" for k, v in DATE_ATTR.findall(text) if k not in SKIP)


def main():
    catalog = sys.argv[1]
    prefix = sys.argv[2] if len(sys.argv) > 2 else ""
    db = sqlite3.connect(f"file:{catalog}?mode=ro", uri=True)
    rows = db.execute(
        """SELECT f.baseName || '.' || f.extension, i.captureTime, i.originalCaptureTime, m.xmp
           FROM Adobe_images i
           JOIN AgLibraryFile f ON f.id_local = i.rootFile
           LEFT JOIN Adobe_AdditionalMetadata m ON m.image = i.id_local
           WHERE f.baseName LIKE ? || '%'
           ORDER BY f.baseName""",
        (prefix,),
    )
    for name, capture, original, blob in rows:
        edited = f" (was {original})" if original else ""
        print(f"{name:9} {capture}{edited}  {xmp_dates(blob)}")


if __name__ == "__main__":
    main()
