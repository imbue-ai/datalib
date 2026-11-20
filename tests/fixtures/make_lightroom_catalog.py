#!/usr/bin/env python3
"""Generate a small, Lightroom-shaped `.lrcat` fixture.

Run as a Bazel genrule
(`//datalib/backend/etl/sqlite_mirror:tng_catalog`, which reaches
this file across the package boundary); the output is a **plain SQLite
file**.

This is Python rather than Rust so that the fixture is minted
independently of the engine under test. Rust *could* do it: every Rust
binary here statically links doltlite as its `sqlite3` (see
`MODULE.bazel`), and while a file doltlite creates is in its own
prolly-tree format by default, the `doltlite_engine=sqlite` URI
parameter selects the stock engine for a new empty file — see
`datalib/backend/runs/src/store.rs`. Generating the input with
stdlib `sqlite3` keeps that choice out of the fixture entirely.

The table definitions are copied verbatim from a real Lightroom Classic
catalog (`sqlite_master.sql`), trimmed to the tables the mirror tests and
`lightroom_render` read. Keeping them verbatim is the point: the shapes
that make this ingester interesting are Lightroom's, not ours —

  * `id_local INTEGER PRIMARY KEY` (a rowid alias Lightroom may renumber)
    alongside `id_global UNIQUE NOT NULL` (a stable UUID),
  * tables with `id_local` but no `id_global` (`AgLibraryKeywordImage`),
  * tables with no key at all (`AgOzSpaceIds`),
  * mostly *untyped* columns, which in SQLite hold whatever type the
    value has — `Adobe_AdditionalMetadata.xmp` is a blob here and a
    string elsewhere in the same column,
  * `NOT NULL DEFAULT` clauses in all three flavours (`''`, `0`, and the
    Lightroom epoch sentinel `-63113817600`).

What the render reads, so a sync of this catalog files each photo under
every search key a photo has (`//tests/fixtures:ingested_lightroom`):

  * a keyword tree under Lightroom's hidden, nameless root keyword, so a
    keyword answers to the keywords above it too,
  * people: a face confirmed with a person keyword, a person keyword with
    no face (Guinan), and a face only suggested, never confirmed, which
    is no one,
  * collections: one inside a set, the quick collection, a smart
    collection (a saved search, so no member rows), and a publish
    service's collection,
  * a caption, a creator, a city and country, and GPS on one photo only,
  * ratings 5, 4 and 3, and one photo unrated (NULL).

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
        colorLabels NOT NULL DEFAULT '',
        copyName,
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
    CREATE TABLE AgLibraryRootFolder (
        id_local INTEGER PRIMARY KEY,
        id_global UNIQUE NOT NULL,
        absolutePath UNIQUE NOT NULL DEFAULT '',
        name NOT NULL DEFAULT '',
        relativePathFromCatalog
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
        keywordType,
        lc_name,
        name,
        parent INTEGER
    )
    """,
    """
    CREATE TABLE AgLibraryKeywordImage (
        id_local INTEGER PRIMARY KEY,
        image INTEGER NOT NULL DEFAULT 0,
        tag INTEGER NOT NULL DEFAULT 0
    )
    """,
    """
    CREATE TABLE AgLibraryFace (
        id_local INTEGER PRIMARY KEY,
        br_x,
        br_y,
        image INTEGER NOT NULL DEFAULT 0,
        tl_x NOT NULL DEFAULT '',
        tl_y NOT NULL DEFAULT ''
    )
    """,
    """
    CREATE TABLE AgLibraryKeywordFace (
        id_local INTEGER PRIMARY KEY,
        face INTEGER NOT NULL DEFAULT 0,
        tag INTEGER NOT NULL DEFAULT 0,
        userPick INTEGER,
        userReject INTEGER
    )
    """,
    """
    CREATE TABLE AgLibraryCollection (
        id_local INTEGER PRIMARY KEY,
        creationId NOT NULL DEFAULT '',
        name NOT NULL DEFAULT '',
        parent INTEGER,
        systemOnly NOT NULL DEFAULT ''
    )
    """,
    """
    CREATE TABLE AgLibraryCollectionImage (
        id_local INTEGER PRIMARY KEY,
        collection INTEGER NOT NULL DEFAULT 0,
        image INTEGER NOT NULL DEFAULT 0
    )
    """,
    """
    CREATE TABLE AgLibraryPublishedCollection (
        id_local INTEGER PRIMARY KEY,
        creationId NOT NULL DEFAULT '',
        name NOT NULL DEFAULT '',
        parent INTEGER,
        systemOnly NOT NULL DEFAULT ''
    )
    """,
    """
    CREATE TABLE AgLibraryPublishedCollectionImage (
        id_local INTEGER PRIMARY KEY,
        collection INTEGER NOT NULL DEFAULT 0,
        image INTEGER NOT NULL DEFAULT 0
    )
    """,
    """
    CREATE TABLE AgLibraryIPTC (
        id_local INTEGER PRIMARY KEY,
        caption,
        copyright,
        image INTEGER NOT NULL DEFAULT 0
    )
    """,
    """
    CREATE TABLE AgHarvestedExifMetadata (
        id_local INTEGER PRIMARY KEY,
        image INTEGER,
        aperture,
        cameraModelRef INTEGER,
        focalLength,
        gpsLatitude,
        gpsLongitude,
        hasGPS INTEGER,
        isoSpeedRating,
        lensRef INTEGER,
        shutterSpeed
    )
    """,
    """
    CREATE TABLE AgHarvestedIptcMetadata (
        id_local INTEGER PRIMARY KEY,
        image INTEGER,
        cityRef INTEGER,
        countryRef INTEGER,
        creatorRef INTEGER,
        locationRef INTEGER,
        stateRef INTEGER
    )
    """,
    # Lightroom interns each camera, lens and place name once, in a table of
    # its own with this one shape.
    *[
        f"CREATE TABLE {table} (id_local INTEGER PRIMARY KEY, searchIndex, value)"
        for table in (
            "AgInternedExifCameraModel",
            "AgInternedExifLens",
            "AgInternedIptcCity",
            "AgInternedIptcCountry",
            "AgInternedIptcCreator",
            "AgInternedIptcLocation",
            "AgInternedIptcState",
        )
    ],
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

# (id_local, id_global, absolutePath, name, relativePathFromCatalog)
ROOT_FOLDERS = [
    (1, "ROOT-0001-PICTURES", "/Users/picard/Pictures/", "Pictures", "../Pictures/"),
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

# (id_local, id_global, keywordType, lc_name, name, parent). Lightroom roots
# every keyword tree at one keyword with no name.
KEYWORDS = [
    (200, "KW-0200-ROOT", None, None, None, None),
    (201, "KW-0201-CREW", None, "crew", "Crew", 200),
    (202, "KW-0202-BRIDGE", None, "bridge", "Bridge", 204),
    (203, "KW-0203-ANDROID", None, "android", "Android", 206),
    (204, "KW-0204-PLACES", None, "places", "Places", 200),
    (206, "KW-0206-SPECIES", None, "species", "Species", 200),
    (207, "KW-0207-PICARD", "person", "jean-luc picard", "Jean-Luc Picard", 200),
    (208, "KW-0208-DATA", "person", "data", "Data", 200),
    (209, "KW-0209-TROI", "person", "deanna troi", "Deanna Troi", 200),
    (210, "KW-0210-WORF", "person", "worf", "Worf", 200),
    (211, "KW-0211-GUINAN", "person", "guinan", "Guinan", 200),
]

# (id_local, image, tag). A confirmed face's person keyword is applied to
# its image too, as Lightroom does; Guinan's is applied with no face.
KEYWORD_IMAGES = [
    (301, 101, 201),
    (302, 101, 202),
    (303, 102, 201),
    (304, 102, 203),
    (305, 103, 201),
    (306, 101, 207),
    (307, 102, 208),
    (308, 103, 209),
    (309, 104, 210),
    (310, 103, 211),
]

# (id_local, image, tl_x, tl_y, br_x, br_y), each corner a fraction of the
# picture.
FACES = [
    (401, 101, 0.40, 0.20, 0.60, 0.50),
    (402, 102, 0.35, 0.15, 0.55, 0.45),
    (403, 103, 0.30, 0.25, 0.50, 0.55),
    (404, 104, 0.45, 0.10, 0.65, 0.40),
    (405, 104, 0.10, 0.30, 0.25, 0.55),
]

# (id_local, face, tag, userPick, userReject). The last is Lightroom's
# suggestion that Worf's shipmate is Data, never confirmed: no one.
KEYWORD_FACES = [
    (501, 401, 207, 1, None),
    (502, 402, 208, 1, None),
    (503, 403, 209, 1, None),
    (504, 404, 210, 1, None),
    (505, 405, 208, None, None),
]

# (id_local, creationId, name, parent, systemOnly)
COLLECTIONS = [
    (601, "com.adobe.ag.library.group", "Away missions", None, 0),
    (602, "com.adobe.ag.library.collection", "Risa", 601, 0),
    (603, "com.adobe.ag.library.collection", "quick collection", None, 1),
    (604, "com.adobe.ag.library.smart_collection", "Five stars", None, 0),
    (605, "com.adobe.ag.library.collection", "Crew portraits", None, 0),
]

# (id_local, collection, image). The smart collection has none.
COLLECTION_IMAGES = [
    (651, 602, 103),
    (652, 603, 101),
    (653, 605, 101),
    (654, 605, 102),
    (655, 605, 104),
]

PUBLISHED_COLLECTIONS = [
    (701, "com.adobe.ag.export.file", "Hard Drive", None, 0),
    (702, "com.adobe.ag.export.file", "Ship's newsletter", 701, 0),
]

PUBLISHED_COLLECTION_IMAGES = [
    (751, 702, 101),
]

# (id_local, caption, copyright, image)
IPTC = [
    (801, "The captain in his ready room, tea in hand.", "Starfleet", 101),
    (803, "Counselor Troi on shore leave.", None, 103),
]

# Each interned table's (id_local, value).
INTERNED = {
    "AgInternedExifCameraModel": [(901, "Starfleet Holoimager Mk IV")],
    "AgInternedExifLens": [(911, "Holoimager 50mm")],
    "AgInternedIptcCity": [(921, "Suraya Bay")],
    "AgInternedIptcCountry": [(931, "Risa")],
    "AgInternedIptcCreator": [(941, "Data")],
    "AgInternedIptcLocation": [(951, "Temtibi Lagoon")],
    "AgInternedIptcState": [],
}

# (id_local, image, aperture, cameraModelRef, focalLength, gpsLatitude,
#  gpsLongitude, hasGPS, isoSpeedRating, lensRef, shutterSpeed). Only the
# captain's photo was taken where there was a GPS fix: Starfleet
# Headquarters, San Francisco.
EXIF = [
    (1001, 101, 4.0, 901, 50.0, 37.8080, -122.4750, 1, 400.0, 911, 6.0),
    (1002, 102, 4.0, 901, 50.0, None, None, 0, 800.0, 911, 5.0),
]

# (id_local, image, cityRef, countryRef, creatorRef, locationRef, stateRef)
IPTC_HARVESTED = [
    (1101, 101, None, None, 941, None, None),
    (1103, 103, 921, 931, None, 951, None),
]


# Each fixture XMP packet is padded past this. A real catalog's packets
# run to tens of KB, and size is load-bearing for the tests: doltlite
# used to silently corrupt values over 4057 bytes on the write path the
# mirror uses (dolthub/doltlite#2327, fixed in v0.11.53), so a fixture
# with only short packets would let a regression through. See
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
            "INSERT INTO AgLibraryRootFolder "
            "(id_local, id_global, absolutePath, name, relativePathFromCatalog) VALUES (?,?,?,?,?)",
            ROOT_FOLDERS,
        )
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
            "INSERT INTO AgLibraryKeyword "
            "(id_local, id_global, keywordType, lc_name, name, parent) VALUES (?,?,?,?,?,?)",
            KEYWORDS,
        )
        con.executemany(
            "INSERT INTO AgLibraryKeywordImage (id_local, image, tag) VALUES (?,?,?)",
            KEYWORD_IMAGES,
        )
        con.executemany(
            "INSERT INTO AgLibraryFace (id_local, image, tl_x, tl_y, br_x, br_y) "
            "VALUES (?,?,?,?,?,?)",
            FACES,
        )
        con.executemany(
            "INSERT INTO AgLibraryKeywordFace (id_local, face, tag, userPick, userReject) "
            "VALUES (?,?,?,?,?)",
            KEYWORD_FACES,
        )
        for table, rows in (
            ("AgLibraryCollection", COLLECTIONS),
            ("AgLibraryPublishedCollection", PUBLISHED_COLLECTIONS),
        ):
            con.executemany(
                f"INSERT INTO {table} (id_local, creationId, name, parent, systemOnly) "
                "VALUES (?,?,?,?,?)",
                rows,
            )
        for table, rows in (
            ("AgLibraryCollectionImage", COLLECTION_IMAGES),
            ("AgLibraryPublishedCollectionImage", PUBLISHED_COLLECTION_IMAGES),
        ):
            con.executemany(
                f"INSERT INTO {table} (id_local, collection, image) VALUES (?,?,?)",
                rows,
            )
        con.executemany(
            "INSERT INTO AgLibraryIPTC (id_local, caption, copyright, image) VALUES (?,?,?,?)",
            IPTC,
        )
        for table, rows in INTERNED.items():
            con.executemany(
                f"INSERT INTO {table} (id_local, searchIndex, value) VALUES (?,?,?)",
                [(i, f"/T{value.lower()}/", value) for i, value in rows],
            )
        con.executemany(
            "INSERT INTO AgHarvestedExifMetadata "
            "(id_local, image, aperture, cameraModelRef, focalLength, gpsLatitude, "
            " gpsLongitude, hasGPS, isoSpeedRating, lensRef, shutterSpeed) "
            "VALUES (?,?,?,?,?,?,?,?,?,?,?)",
            EXIF,
        )
        con.executemany(
            "INSERT INTO AgHarvestedIptcMetadata "
            "(id_local, image, cityRef, countryRef, creatorRef, locationRef, stateRef) "
            "VALUES (?,?,?,?,?,?,?)",
            IPTC_HARVESTED,
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
