"""Write capture times straight into a copy of a test catalog, each edit
touching a chosen set of the places Lightroom keeps one, to learn which
of them Lightroom needs, keeps and shows (the README's round 5).

Usage: python3 -I edit_catalog.py <catalog.lrcat> <edits.csv>

`edits.csv` has a header and the columns `name` (a case's file name
without extension), `capture_time` (the target, local clock time with
its offset: `2024-07-01T14:00:00.000+02:00`), `touch` (`+`-joined, from
the list below) and `asks`. Refuses a catalog Lightroom has open (its
`.lock` file is there), copies the catalog to `<catalog>.before-edit`
first (`.before-edit-2` and on when that is taken), and refuses an XMP
edit that would not parse. Only for a catalog of these test files.

  capture         `Adobe_images.captureTime` = the clock time, no offset
  capture_offset  `Adobe_images.captureTime` = the clock time and offset
  xmp             `exif:DateTimeOriginal` and `photoshop:DateCreated` in
                  the catalog's XMP = the target with its offset
  days            `AgHarvestedExifMetadata.dateYear/Month/Day` = its date
  original        `Adobe_images.originalCaptureTime` = the old time, when
                  it is empty, as Edit Capture Time does
"""

import csv
import os
import re
import shutil
import sqlite3
import struct
import sys
import xml.etree.ElementTree as ET
import zlib
from datetime import datetime

TOUCHES = {"capture", "capture_offset", "xmp", "days", "original"}
XMP_DATES = ("exif:DateTimeOriginal", "photoshop:DateCreated")
# A photo whose XMP had no such date may not declare the prefix either;
# Lightroom then reads none of that photo's XMP.
NAMESPACES = {
    "exif": "http://ns.adobe.com/exif/1.0/",
    "photoshop": "http://ns.adobe.com/photoshop/1.0/",
}


def unpack_xmp(blob: bytes) -> str:
    return zlib.decompress(blob[4:]).decode("utf-8")


# The catalog stores XMP as its uncompressed length (4 bytes, big-endian),
# then zlib.
def pack_xmp(text: str) -> bytes:
    raw = text.encode("utf-8")
    return struct.pack(">I", len(raw)) + zlib.compress(raw)


def set_attr(xmp: str, name: str, value: str) -> str:
    attr = re.compile(rf'(\s){re.escape(name)}="[^"]*"')
    if attr.search(xmp):
        return attr.sub(lambda m: f'{m.group(1)}{name}="{value}"', xmp, count=1)
    # Add it beside the first attribute of the description.
    return re.sub(
        r'(<rdf:Description rdf:about=""\s)', rf'\1   {name}="{value}"\n', xmp, count=1
    )


def declare(xmp: str, prefix: str) -> str:
    if f"xmlns:{prefix}=" in xmp:
        return xmp
    return re.sub(
        r'(<rdf:Description rdf:about="")',
        rf'\1\n    xmlns:{prefix}="{NAMESPACES[prefix]}"',
        xmp,
        count=1,
    )


def clock(target: datetime) -> str:
    return target.strftime("%Y-%m-%dT%H:%M:%S.") + f"{target.microsecond // 1000:03d}"


def with_offset(target: datetime) -> str:
    return target.isoformat(timespec="seconds")


def apply(db: sqlite3.Connection, name: str, target: datetime, touch: set[str]) -> None:
    row = db.execute(
        """SELECT i.id_local, i.captureTime, i.originalCaptureTime FROM Adobe_images i
           JOIN AgLibraryFile f ON f.id_local = i.rootFile WHERE f.baseName = ?""",
        (name,),
    ).fetchall()
    if len(row) != 1:
        sys.exit(f"{name}: {len(row)} images, expected 1")
    image, old, original = row[0]
    if "capture" in touch:
        db.execute(
            "UPDATE Adobe_images SET captureTime = ? WHERE id_local = ?",
            (clock(target), image),
        )
    if "capture_offset" in touch:
        db.execute(
            "UPDATE Adobe_images SET captureTime = ? WHERE id_local = ?",
            (with_offset(target), image),
        )
    if "original" in touch and original is None:
        db.execute(
            "UPDATE Adobe_images SET originalCaptureTime = ? WHERE id_local = ?",
            (old, image),
        )
    if "days" in touch:
        db.execute(
            "UPDATE AgHarvestedExifMetadata SET dateYear = ?, dateMonth = ?, dateDay = ? WHERE image = ?",
            # Lightroom stores these as reals (2024.0).
            (float(target.year), float(target.month), float(target.day), image),
        )
    if "xmp" in touch:
        (blob,) = db.execute(
            "SELECT xmp FROM Adobe_AdditionalMetadata WHERE image = ?", (image,)
        ).fetchone()
        xmp = unpack_xmp(blob)
        for attr in XMP_DATES:
            xmp = set_attr(declare(xmp, attr.split(":")[0]), attr, with_offset(target))
        ET.fromstring(xmp)
        db.execute(
            "UPDATE Adobe_AdditionalMetadata SET xmp = ? WHERE image = ?",
            (pack_xmp(xmp), image),
        )
    print(f"{name}: {old} -> {target.isoformat()} ({'+'.join(sorted(touch))})")


def main():
    catalog, edits = sys.argv[1], sys.argv[2]
    if os.path.exists(catalog + ".lock"):
        sys.exit(f"{catalog} is open in Lightroom (its .lock file is there)")
    backup, n = catalog + ".before-edit", 1
    while os.path.exists(backup):
        n += 1
        backup = f"{catalog}.before-edit-{n}"
    shutil.copyfile(catalog, backup)
    db = sqlite3.connect(catalog)
    with open(edits, newline="") as f:
        for row in csv.DictReader(f):
            touch = set(row["touch"].split("+"))
            if not touch <= TOUCHES:
                sys.exit(f"{row['name']}: unknown touch {touch - TOUCHES}")
            apply(db, row["name"], datetime.fromisoformat(row["capture_time"]), touch)
    db.commit()
    db.close()


if __name__ == "__main__":
    main()
