#!/usr/bin/env python3
"""Generate a small, Lightroom-shaped `.lrcat` fixture.

Run as a Bazel genrule (`:tng_catalog`); the output is a **plain SQLite
file**, which is the whole reason this is Python and not Rust. Every Rust
binary in this tree statically links doltlite's amalgamation as its
`sqlite3` (see `MODULE.bazel`), and while that library reads *and writes*
ordinary SQLite files transparently, a file it *creates* is always in
doltlite's own prolly-tree format. So a Rust test can mutate a catalog
fixture but cannot mint one; Python's stdlib `sqlite3` can.

The table definitions are copied verbatim from a real Lightroom Classic
catalog (`sqlite_master.sql`), trimmed to the tables the mirror tests
actually exercise. Keeping them verbatim is the point: the shapes that
make this ingester interesting are Lightroom's, not ours —

  * `id_local INTEGER PRIMARY KEY` (a rowid alias Lightroom may renumber)
    alongside `id_global UNIQUE NOT NULL` (a stable UUID),
  * tables with `id_local` but no `id_global` (`AgLibraryKeywordImage`),
  * tables with no key at all (`AgOzSpaceIds`),
  * mostly *untyped* columns, which in SQLite hold whatever type the
    value has — `Adobe_AdditionalMetadata.xmp` is a blob here and a
    string elsewhere in the same column,
  * `NOT NULL DEFAULT` clauses in all three flavours (`''`, `0`, and the
    Lightroom epoch sentinel `-63113817600`).

Content is TNG-themed, per this repo's fixture convention.
"""

import hashlib
import sqlite3
import sys

# Verbatim from a real catalog, minus columns no test touches. Indexes and
# triggers are created below too — the mirror is expected to ignore them.
SCHEMA = [
    """
    CREATE TABLE Adobe_images (
        id_local INTEGER PRIMARY KEY,
        id_global UNIQUE NOT NULL,
        captureTime,
        fileFormat NOT NULL DEFAULT 'unset',
        fileHeight,
        fileWidth,
        pick NOT NULL DEFAULT 0,
        rating,
        rootFile INTEGER NOT NULL DEFAULT 0,
        touchTime NOT NULL DEFAULT 0
    )
    """,
    """
    CREATE TABLE AgLibraryFile (
        id_local INTEGER PRIMARY KEY,
        id_global UNIQUE NOT NULL,
        baseName NOT NULL DEFAULT '',
        extension NOT NULL DEFAULT '',
        folder INTEGER NOT NULL DEFAULT 0,
        originalFilename NOT NULL DEFAULT ''
    )
    """,
    """
    CREATE TABLE AgLibraryFolder (
        id_local INTEGER PRIMARY KEY,
        id_global UNIQUE NOT NULL,
        parentId INTEGER,
        pathFromRoot NOT NULL DEFAULT '',
        rootFolder INTEGER NOT NULL DEFAULT 0
    )
    """,
    # The XMP carrier. `xmp` is untyped and holds a blob, so it doubles as
    # the round-trip check that SQLite's dynamic typing survives the copy.
    """
    CREATE TABLE Adobe_AdditionalMetadata (
        id_local INTEGER PRIMARY KEY,
        id_global UNIQUE NOT NULL,
        image INTEGER,
        internalXmpDigest,
        lastSynchronizedTimestamp NOT NULL DEFAULT -63113817600,
        xmp NOT NULL DEFAULT ''
    )
    """,
    # No `id_global`: keyed on the renumberable `id_local` and nothing else.
    """
    CREATE TABLE AgMetadataSearchIndex (
        id_local INTEGER PRIMARY KEY,
        exifSearchIndex NOT NULL DEFAULT '',
        image INTEGER,
        searchIndex NOT NULL DEFAULT ''
    )
    """,
    """
    CREATE TABLE AgLibraryKeyword (
        id_local INTEGER PRIMARY KEY,
        id_global UNIQUE NOT NULL,
        lc_name,
        name
    )
    """,
    """
    CREATE TABLE AgLibraryKeywordImage (
        id_local INTEGER PRIMARY KEY,
        image INTEGER NOT NULL DEFAULT 0,
        tag INTEGER NOT NULL DEFAULT 0
    )
    """,
    # Primary keys that are NOT `INTEGER PRIMARY KEY`, so not rowid
    # aliases. SQLite reports these columns as nullable; dolt stores them
    # NOT NULL. Ten of a real catalog's 133 tables are shaped this way,
    # and the asymmetry made every one of them recreate on every run
    # until `build_spec` normalised it — see
    # `unchanged_source_produces_no_commit`.
    "CREATE TABLE MigrationSchemaVersion(version TEXT PRIMARY KEY)",
    """
    CREATE TABLE AgLibraryImageChangeCounter(
        image PRIMARY KEY,
        changeCounter DEFAULT 0,
        changedAtTime DEFAULT ''
    )
    """,
    # Genuinely keyless — no PRIMARY KEY, no UNIQUE.
    """
    CREATE TABLE AgOzSpaceIds(
        ozCatalogId NOT NULL,
        ozSpaceId NOT NULL,
        isPublic DEFAULT 1
    )
    """,
    # Indexes and a trigger the mirror must skip rather than choke on.
    "CREATE INDEX index_Adobe_images_rootFile ON Adobe_images( rootFile )",
    "CREATE INDEX index_AgLibraryFile_folder ON AgLibraryFile( folder )",
    "CREATE UNIQUE INDEX index_AgLibraryKeyword_lc_name ON AgLibraryKeyword( lc_name )",
    """
    CREATE TRIGGER Adobe_images_touch AFTER UPDATE OF rating ON Adobe_images
    BEGIN
        UPDATE Adobe_images SET touchTime = touchTime + 1 WHERE id_local = OLD.id_local;
    END
    """,
]

FOLDERS = [
    (1, "FOLDER-0001-ENTERPRISE", None, "Enterprise/Bridge/", 1),
    (2, "FOLDER-0002-TENFORWARD", 1, "Enterprise/TenForward/", 1),
]

# (id_local, id_global, baseName, extension, folder, originalFilename)
FILES = [
    (11, "FILE-0011-PICARD", "picard_ready_room", "dng", 1, "IMG_0011.dng"),
    (12, "FILE-0012-DATA", "data_at_ops", "dng", 1, "IMG_0012.dng"),
    (13, "FILE-0013-TROI", "troi_ten_forward", "jpg", 2, "IMG_0013.jpg"),
    (14, "FILE-0014-WORF", "worf_tactical", "dng", 1, "IMG_0014.dng"),
]

# (id_local, id_global, captureTime, fileFormat, w, h, rating, rootFile)
IMAGES = [
    (101, "IMAGE-0101-PICARD", "2364-03-12T09:15:00-07:00", "RAW", 6000, 4000, 5, 11),
    (102, "IMAGE-0102-DATA", "2364-03-12T09:41:30-07:00", "RAW", 6000, 4000, 4, 12),
    (103, "IMAGE-0103-TROI", "2364-03-13T20:02:11-07:00", "JPG", 4032, 3024, 3, 13),
    (104, "IMAGE-0104-WORF", "2364-03-14T06:00:00-07:00", "RAW", 6000, 4000, None, 14),
]

KEYWORDS = [
    (201, "KW-0201-CREW", "crew", "Crew"),
    (202, "KW-0202-BRIDGE", "bridge", "Bridge"),
    (203, "KW-0203-ANDROID", "android", "Android"),
]

KEYWORD_IMAGES = [
    (301, 101, 201),
    (302, 101, 202),
    (303, 102, 201),
    (304, 102, 203),
    (305, 103, 201),
]


# Each fixture XMP packet is padded past this. A real catalog's packets
# run to tens of KB, and size is load-bearing for the tests: doltlite
# v0.11.50 silently corrupts values over 4054 bytes on the write path the
# mirror uses, so a fixture with only short packets would let that
# through. See `mirror::rebuild_table` §"The staging hop" and
# `mirror_roundtrip.rs::large_values_round_trip_byte_for_byte`.
MIN_XMP_BYTES = 20_000


def xmp_packet(subject: str) -> bytes:
    """A stand-in for the per-image XMP packet, as raw bytes.

    Stored into an untyped column, so it lands as a SQLite BLOB — which is
    what a real catalog does and what the mirror must preserve. Padded to
    at least `MIN_XMP_BYTES` with content derived from `subject`, so every
    photo's packet is distinct, deterministic, and large enough to matter.
    """
    head = (
        '<?xpacket begin="﻿"?>'
        '<x:xmpmeta xmlns:x="adobe:ns:meta/">'
        f'<rdf:Description dc:subject="{subject}"/>'
    )
    tail = '</x:xmpmeta><?xpacket end="w"?>'
    # Distinct per subject and incompressible enough to be a real
    # comparison, without needing a PRNG the fixture would have to seed.
    filler = hashlib.sha256(subject.encode()).hexdigest()
    pad = (filler * (MIN_XMP_BYTES // len(filler) + 1))[:MIN_XMP_BYTES]
    return (head + f"<!--{pad}-->" + tail).encode("utf-8")


def main(out_path: str) -> None:
    con = sqlite3.connect(out_path)
    try:
        for stmt in SCHEMA:
            con.execute(stmt)

        con.executemany(
            "INSERT INTO AgLibraryFolder "
            "(id_local, id_global, parentId, pathFromRoot, rootFolder) VALUES (?,?,?,?,?)",
            FOLDERS,
        )
        con.executemany(
            "INSERT INTO AgLibraryFile "
            "(id_local, id_global, baseName, extension, folder, originalFilename) "
            "VALUES (?,?,?,?,?,?)",
            FILES,
        )
        con.executemany(
            "INSERT INTO Adobe_images "
            "(id_local, id_global, captureTime, fileFormat, fileWidth, fileHeight, "
            " rating, rootFile) VALUES (?,?,?,?,?,?,?,?)",
            IMAGES,
        )
        con.executemany(
            "INSERT INTO AgLibraryKeyword (id_local, id_global, lc_name, name) VALUES (?,?,?,?)",
            KEYWORDS,
        )
        con.executemany(
            "INSERT INTO AgLibraryKeywordImage (id_local, image, tag) VALUES (?,?,?)",
            KEYWORD_IMAGES,
        )
        for img in IMAGES:
            con.execute(
                "INSERT INTO Adobe_AdditionalMetadata "
                "(id_local, id_global, image, internalXmpDigest, xmp) VALUES (?,?,?,?,?)",
                (
                    img[0] + 1000,
                    f"META-{img[1]}",
                    img[0],
                    f"digest-{img[0]}",
                    xmp_packet(img[1]),
                ),
            )
            con.execute(
                "INSERT INTO AgMetadataSearchIndex "
                "(id_local, exifSearchIndex, image, searchIndex) VALUES (?,?,?,?)",
                (img[0] + 2000, f"|raw|{img[3]}|", img[0], f"|{img[1].lower()}|"),
            )
        con.execute("INSERT INTO MigrationSchemaVersion (version) VALUES ('11.0')")
        con.executemany(
            "INSERT INTO AgLibraryImageChangeCounter (image, changeCounter, changedAtTime) "
            "VALUES (?,?,?)",
            [(img[0], 1, "2364-03-12T09:15:00-07:00") for img in IMAGES],
        )
        con.executemany(
            "INSERT INTO AgOzSpaceIds (ozCatalogId, ozSpaceId, isPublic) VALUES (?,?,?)",
            [
                ("catalog-ncc-1701-d", "space-alpha", 1),
                ("catalog-ncc-1701-d", "space-beta", 0),
            ],
        )
        con.commit()
    finally:
        con.close()


if __name__ == "__main__":
    main(sys.argv[1])
