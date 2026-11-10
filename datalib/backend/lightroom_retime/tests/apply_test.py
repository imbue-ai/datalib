"""datalib-lightroom-retime against a catalog stock SQLite made, as
Lightroom's is: in WAL mode, with Lightroom's column shapes for the tables
the tool touches and the three XMP shapes round 5 met
(lightroom/tests/capture_time/RESULTS.md). The binary writes through
doltlite; stock SQLite reads the result back, as Lightroom will."""

import hashlib
import json
import sqlite3
import struct
import subprocess
import sys
import tempfile
import unittest
import zlib
from pathlib import Path

BINARY = Path(sys.argv[1]).resolve()

SCHEMA = """
CREATE TABLE Adobe_images (
    id_local INTEGER PRIMARY KEY, id_global UNIQUE NOT NULL,
    captureTime, originalCaptureTime, rootFile INTEGER NOT NULL DEFAULT 0);
CREATE TABLE AgLibraryFile (
    id_local INTEGER PRIMARY KEY, id_global UNIQUE NOT NULL,
    baseName NOT NULL DEFAULT '', extension NOT NULL DEFAULT '');
CREATE TABLE Adobe_AdditionalMetadata (
    id_local INTEGER PRIMARY KEY, id_global UNIQUE NOT NULL,
    image INTEGER, xmp NOT NULL DEFAULT '');
CREATE TABLE AgHarvestedExifMetadata (
    id_local INTEGER PRIMARY KEY, image INTEGER, dateDay, dateMonth, dateYear);
"""

RDF = 'xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"'


def wrap(description: str) -> str:
    return f'<x:xmpmeta xmlns:x="adobe:ns:meta/">\n <rdf:RDF {RDF}>\n  {description}\n </rdf:RDF>\n</x:xmpmeta>\n'


PHOTOS = [
    (
        "A01.jpg",
        "ID-A01",
        "2024-07-01T12:00:00.000",
        wrap(
            '<rdf:Description rdf:about=""\n'
            '    xmlns:exif="http://ns.adobe.com/exif/1.0/"\n'
            '    xmlns:photoshop="http://ns.adobe.com/photoshop/1.0/"\n'
            '   exif:DateTimeOriginal="2024-07-01T12:00:00"\n'
            '   photoshop:DateCreated="2024-07-01T12:00:00"/>'
        ),
    ),
    (
        "F10.jpg",
        "ID-F10",
        "2024-08-01T21:00:00.000",
        wrap(
            '<rdf:Description rdf:about=""\n'
            '    xmlns:exif="http://ns.adobe.com/exif/1.0/"\n'
            '   exif:PixelXDimension="320"/>'
        ),
    ),
    (
        "E05X.mov",
        "ID-E05X",
        "2024-07-04T12:00:00.000",
        wrap(
            '<rdf:Description rdf:about=""\n'
            '    xmlns:xmp="http://ns.adobe.com/xap/1.0/"\n'
            '   xmp:CreateDate="2024-07-04T12:00:00Z"/>'
        ),
    ),
    (
        "V01.mov",
        "ID-V01",
        "2024-07-05T14:00:00.000",
        wrap(
            '<rdf:Description rdf:about=""\n'
            '    xmlns:xmp="http://ns.adobe.com/xap/1.0/"\n'
            '   xmp:CreateDate="2024-07-05T12:00:00Z"/>'
        ),
    ),
]

EDITS = [
    {
        "id_global": "ID-A01",
        "expected": "2024-07-01T12:00:00.000",
        "capture_time": "2024-07-01T14:00:00.000+02:00",
        "why": "camera on UTC",
    },
    {
        "id_global": "ID-F10",
        "expected": "2024-08-01T21:00:00.000",
        "capture_time": "2024-08-02T06:00:00.000+09:00",
    },
    {
        "id_global": "ID-E05X",
        "expected": "2024-07-04T12:00:00.000",
        "capture_time": "2024-07-04T14:00:00.000+02:00",
    },
    # The clock is already right; only the offset is missing.
    {
        "id_global": "ID-V01",
        "expected": "2024-07-05T14:00:00.000",
        "capture_time": "2024-07-05T14:00:00.000+02:00",
    },
]


def pack(text: str) -> bytes:
    raw = text.encode()
    return struct.pack(">I", len(raw)) + zlib.compress(raw)


def unpack(blob: bytes) -> str:
    return zlib.decompress(blob[4:]).decode()


class Catalog:
    def __init__(self) -> None:
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)
        self.path = self.dir / "test.lrcat"
        self.cache = self.dir / "test Helper.lrdata"
        db = sqlite3.connect(self.path)
        db.execute("PRAGMA journal_mode = WAL")
        db.executescript(SCHEMA)
        for i, (name, id_global, capture, xmp) in enumerate(PHOTOS):
            n = i * 10
            base, ext = name.split(".")
            y, m, d = (float(p) for p in capture[:10].split("-"))
            db.execute(
                "INSERT INTO AgLibraryFile VALUES (?,?,?,?)",
                (n + 1, f"F-{id_global}", base, ext),
            )
            db.execute(
                "INSERT INTO Adobe_images (id_local, id_global, captureTime, rootFile) VALUES (?,?,?,?)",
                (n + 2, id_global, capture, n + 1),
            )
            db.execute(
                "INSERT INTO Adobe_AdditionalMetadata VALUES (?,?,?,?)",
                (n + 3, f"M-{id_global}", n + 2, pack(xmp)),
            )
            db.execute(
                "INSERT INTO AgHarvestedExifMetadata VALUES (?,?,?,?,?)",
                (n + 4, n + 2, d, m, y),
            )
        db.commit()
        db.close()
        self.cache.mkdir()
        (self.cache / "metadatahelper.db").write_bytes(b"cache")

    def retime(
        self, edits: list[dict[str, str]], *flags: str
    ) -> subprocess.CompletedProcess[str]:
        edits_file = self.dir / "edits.jsonl"
        edits_file.write_text("\n".join(json.dumps(e) for e in edits) + "\n")
        return subprocess.run(
            [str(BINARY), str(self.path), str(edits_file), *flags],
            check=False,
            capture_output=True,
            text=True,
        )

    def digest(self) -> str:
        return hashlib.sha256(self.path.read_bytes()).hexdigest()

    def files(self) -> list[str]:
        return sorted(p.name for p in self.dir.iterdir())

    def photo(self, id_global: str) -> tuple:
        db = sqlite3.connect(self.path)
        try:
            return db.execute(
                """SELECT i.captureTime, i.originalCaptureTime, m.xmp,
                          e.dateYear, e.dateMonth, e.dateDay
                   FROM Adobe_images i
                   JOIN Adobe_AdditionalMetadata m ON m.image = i.id_local
                   JOIN AgHarvestedExifMetadata e ON e.image = i.id_local
                   WHERE i.id_global = ?""",
                (id_global,),
            ).fetchone()
        finally:
            db.close()


class RetimeTest(unittest.TestCase):
    def catalog(self) -> Catalog:
        c = Catalog()
        self.addCleanup(c.tmp.cleanup)
        return c

    def test_python_reads_with_stock_sqlite(self) -> None:
        db = sqlite3.connect(":memory:")
        with self.assertRaises(sqlite3.OperationalError):
            db.execute("SELECT * FROM dolt_log")

    def test_a_dry_run_writes_nothing(self) -> None:
        c = self.catalog()
        before, files = c.digest(), c.files()
        done = c.retime(EDITS)
        self.assertEqual(done.returncode, 0, done.stderr)
        self.assertIn("4 to write, 0 already done, 0 refused.", done.stdout)
        self.assertEqual(c.digest(), before)
        self.assertEqual(c.files(), sorted(files + ["edits.jsonl"]))

    def test_writes_every_place_and_keeps_the_catalog_and_cache_aside(self) -> None:
        c = self.catalog()
        original = c.path.read_bytes()
        done = c.retime(EDITS, "--write")
        self.assertEqual(done.returncode, 0, done.stderr)
        kept = [n for n in c.files() if ".bak_timestamps_" in n]
        self.assertEqual(len(kept), 2, c.files())
        backup, cache = sorted(
            kept, key=lambda n: n.startswith("test.lrcat"), reverse=True
        )
        self.assertEqual((c.dir / backup).read_bytes(), original)
        self.assertTrue(backup.startswith("test.lrcat.bak_timestamps_"), backup)
        self.assertTrue(cache.startswith("test Helper.lrdata.bak_timestamps_"), cache)
        self.assertEqual((c.dir / cache / "metadatahelper.db").read_bytes(), b"cache")
        self.assertFalse(c.cache.exists())
        self.assertNotIn("test.lrcat-wal", c.files())

        db = sqlite3.connect(c.path)
        self.assertEqual(db.execute("PRAGMA integrity_check").fetchall(), [("ok",)])
        self.assertEqual(db.execute("PRAGMA journal_mode").fetchone(), ("wal",))
        db.close()

        capture, original_time, xmp, *_ = c.photo("ID-A01")
        self.assertEqual(capture, "2024-07-01T14:00:00.000")
        self.assertEqual(original_time, "2024-07-01T12:00:00.000")
        self.assertIn('exif:DateTimeOriginal="2024-07-01T14:00:00+02:00"', unpack(xmp))
        self.assertIn('photoshop:DateCreated="2024-07-01T14:00:00+02:00"', unpack(xmp))

        # Aug 2 in Tokyo is Aug 1 in UTC; the Library filter's day is the local one.
        capture, _, xmp, *day = c.photo("ID-F10")
        self.assertEqual(capture, "2024-08-02T06:00:00.000")
        self.assertEqual(day, [2024.0, 8.0, 2.0])
        self.assertIn(
            'xmlns:photoshop="http://ns.adobe.com/photoshop/1.0/"', unpack(xmp)
        )

        # A first time is kept only when the clock moves.
        capture, original_time, _, *_ = c.photo("ID-V01")
        self.assertEqual((capture, original_time), ("2024-07-05T14:00:00.000", None))

        _, _, xmp, *_ = c.photo("ID-E05X")
        text = unpack(xmp)
        self.assertIn('xmlns:exif="http://ns.adobe.com/exif/1.0/"', text)
        self.assertIn('exif:DateTimeOriginal="2024-07-04T14:00:00+02:00"', text)
        self.assertIn('xmp:CreateDate="2024-07-04T12:00:00Z"', text)

    def test_running_again_finds_it_done(self) -> None:
        c = self.catalog()
        c.retime(EDITS, "--write")
        after = c.digest()
        again = c.retime(EDITS, "--write")
        self.assertEqual(again.returncode, 0, again.stderr)
        self.assertIn("0 to write, 4 already done, 0 refused.", again.stdout)
        self.assertEqual(c.digest(), after)

    def test_one_stale_edit_and_nothing_is_written(self) -> None:
        """A file made from a mirror older than the catalog must not
        overwrite an edit made since, nor apply the rest of itself."""
        c = self.catalog()
        before, files = c.digest(), c.files()
        stale = [dict(EDITS[0]), dict(EDITS[1], expected="2024-08-01T20:00:00.000")]
        done = c.retime(stale, "--write")
        self.assertEqual(done.returncode, 1)
        self.assertIn("not the expected", done.stdout)
        self.assertEqual(c.digest(), before)
        self.assertEqual(c.files(), sorted(files + ["edits.jsonl"]))

    def test_refuses_a_catalog_lightroom_has_open(self) -> None:
        c = self.catalog()
        (c.dir / "test.lrcat.lock").write_text("")
        done = c.retime(EDITS, "--write")
        self.assertNotEqual(done.returncode, 0)
        self.assertIn("quit Lightroom", done.stderr)


if __name__ == "__main__":
    unittest.main(argv=sys.argv[:1])
