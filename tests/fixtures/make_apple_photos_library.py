#!/usr/bin/env python3
"""Generate a small, Apple-Photos-shaped `Photos.sqlite` fixture.

Run as a Bazel genrule
(`//datalib/backend/etl/providers/apple_photos:tng_library`, which
reaches this file across the package boundary); the output is a **plain
SQLite file** at `<bundle>.photoslibrary/database/Photos.sqlite`, the
one path inside a library the mirror opens.

Python rather than Rust for the reason `make_lightroom_catalog.py`
gives: every Rust binary in this tree links doltlite as its `sqlite3`,
and generating the input with stdlib `sqlite3` keeps the engine under
test out of the fixture.

The shapes are Core Data's, trimmed from a real library
(`LibrarySchemaVersion` 5001) to what the tests exercise:

  * `Z_PK INTEGER PRIMARY KEY` on every entity table — a rowid Photos
    renumbers on a library repair — beside `ZUUID`, which is indexed
    (`Z_Asset_byUuidIndex`) but **never declared UNIQUE**,
  * `Z_OPT`, Core Data's optimistic-locking counter, on every row,
  * a many-to-many join table (`Z_33ASSETS`) keyed on a pair of rowids,
  * Core Data's persistent history (`ACHANGE`, `ATRANSACTION`,
    `ATRANSACTIONSTRING`) and per-entity counters (`Z_PRIMARYKEY`),
    which change on every save,
  * an R-tree virtual table (`Z_RT_Asset_boundedByRect`) with its three
    shadow tables,
  * a table whose `ZUUID` is NULL in one row (`ZDETECTEDFACE`), so the
    stable-key check has something to refuse,
  * Core Data timestamps: seconds since 2001-01-01 as REAL.

Content is TNG-themed, per this repo's fixture convention.
"""

import os
import sqlite3
import sys

# Trimmed from a real library's `sqlite_master`. Column order and the
# odd spellings (`Z_33ASSETS`, `Z_FOK_3ASSETS`) are Core Data's.
SCHEMA = [
    """
    CREATE TABLE ZASSET (
        Z_PK INTEGER PRIMARY KEY,
        Z_ENT INTEGER,
        Z_OPT INTEGER,
        ZFAVORITE INTEGER,
        ZHIDDEN INTEGER,
        ZKIND INTEGER,
        ZTRASHEDSTATE INTEGER,
        ZWIDTH INTEGER,
        ZHEIGHT INTEGER,
        ZADDITIONALATTRIBUTES INTEGER,
        ZDATECREATED TIMESTAMP,
        ZMODIFICATIONDATE TIMESTAMP,
        ZLATITUDE FLOAT,
        ZLONGITUDE FLOAT,
        ZDIRECTORY VARCHAR,
        ZFILENAME VARCHAR,
        ZUNIFORMTYPEIDENTIFIER VARCHAR,
        ZUUID VARCHAR
    )
    """,
    "CREATE INDEX Z_Asset_byUuidIndex ON ZASSET (ZUUID COLLATE BINARY ASC)",
    "CREATE INDEX ZASSET_ZADDITIONALATTRIBUTES_INDEX ON ZASSET (ZADDITIONALATTRIBUTES)",
    """
    CREATE TABLE ZADDITIONALASSETATTRIBUTES (
        Z_PK INTEGER PRIMARY KEY,
        Z_ENT INTEGER,
        Z_OPT INTEGER,
        ZASSET INTEGER,
        ZORIGINALFILENAME VARCHAR,
        ZTITLE VARCHAR,
        ZTIMEZONENAME VARCHAR,
        ZTIMEZONEOFFSET INTEGER
    )
    """,
    """
    CREATE TABLE ZGENERICALBUM (
        Z_PK INTEGER PRIMARY KEY,
        Z_ENT INTEGER,
        Z_OPT INTEGER,
        ZKIND INTEGER,
        ZTRASHEDSTATE INTEGER,
        ZCREATIONDATE TIMESTAMP,
        ZTITLE VARCHAR,
        ZUUID VARCHAR
    )
    """,
    "CREATE INDEX Z_GenericAlbum_byUuidIndex ON ZGENERICALBUM (ZUUID COLLATE BINARY ASC)",
    """
    CREATE TABLE Z_33ASSETS (
        Z_33ALBUMS INTEGER,
        Z_3ASSETS INTEGER,
        Z_FOK_3ASSETS INTEGER,
        PRIMARY KEY (Z_33ALBUMS, Z_3ASSETS)
    )
    """,
    # `ZUUID` here is nullable in practice: a face Photos has detected
    # but not yet assigned gets one later.
    """
    CREATE TABLE ZDETECTEDFACE (
        Z_PK INTEGER PRIMARY KEY,
        Z_ENT INTEGER,
        Z_OPT INTEGER,
        ZASSETFORFACE INTEGER,
        ZCENTERX FLOAT,
        ZCENTERY FLOAT,
        ZUUID VARCHAR
    )
    """,
    # Core Data's persistent history and per-entity bookkeeping.
    """
    CREATE TABLE ACHANGE (
        Z_PK INTEGER PRIMARY KEY,
        Z_ENT INTEGER,
        Z_OPT INTEGER,
        ZCHANGETYPE INTEGER,
        ZENTITY INTEGER,
        ZENTITYPK INTEGER,
        ZTRANSACTIONID INTEGER,
        ZCOLUMNS BLOB
    )
    """,
    """
    CREATE TABLE ATRANSACTION (
        Z_PK INTEGER PRIMARY KEY,
        Z_ENT INTEGER,
        Z_OPT INTEGER,
        ZAUTHORTS INTEGER,
        ZTIMESTAMP FLOAT,
        ZAUTHOR VARCHAR,
        ZQUERYGEN BLOB
    )
    """,
    """
    CREATE TABLE ATRANSACTIONSTRING (
        Z_PK INTEGER PRIMARY KEY,
        Z_ENT INTEGER,
        Z_OPT INTEGER,
        ZNAME VARCHAR
    )
    """,
    "CREATE UNIQUE INDEX Z_TRANSACTIONSTRING_UNIQUE_NAME ON ATRANSACTIONSTRING (ZNAME)",
    "CREATE TABLE Z_PRIMARYKEY (Z_ENT INTEGER PRIMARY KEY, Z_NAME VARCHAR, Z_SUPER INTEGER, Z_MAX INTEGER)",
    "CREATE TABLE Z_METADATA (Z_VERSION INTEGER PRIMARY KEY, Z_UUID VARCHAR(255), Z_PLIST BLOB)",
    "CREATE TABLE Z_MODELCACHE (Z_CONTENT BLOB)",
    # The geo index over ZASSET's lat/lon: an R-tree, whose rows live in
    # three shadow tables the mirror must not copy.
    """
    CREATE VIRTUAL TABLE Z_RT_Asset_boundedByRect USING RTREE (
        Z_PK INTEGER PRIMARY KEY,
        ZLATITUDE_MIN FLOAT, ZLATITUDE_MAX FLOAT,
        ZLONGITUDE_MIN FLOAT, ZLONGITUDE_MAX FLOAT
    )
    """,
    # A counter-maintaining trigger of the kind Photos keeps, which the
    # mirror must skip rather than replay.
    """
    CREATE TRIGGER ZT_ZASSET_TOUCH AFTER UPDATE OF ZFAVORITE ON ZASSET
    BEGIN
        UPDATE ZASSET SET Z_OPT = Z_OPT + 1 WHERE Z_PK = OLD.Z_PK;
    END
    """,
]

# 2364-03-12T09:15:00Z as seconds since the Core Data epoch, 2001-01-01.
CORE_DATA_EPOCH = 978307200
T0 = 12_439_270_500 - CORE_DATA_EPOCH

# (Z_PK, ZUUID, ZFAVORITE, w, h, created, lat, lon, filename, title)
ASSETS = [
    (
        1,
        "1A2B3C4D-0001-4000-8000-000000000001",
        1,
        6000,
        4000,
        T0,
        37.8,
        -122.4,
        "IMG_0011.jpeg",
        "Picard, ready room",
    ),
    (
        2,
        "1A2B3C4D-0002-4000-8000-000000000002",
        0,
        6000,
        4000,
        T0 + 1590,
        37.8,
        -122.4,
        "IMG_0012.jpeg",
        "Data at ops",
    ),
    (
        3,
        "1A2B3C4D-0003-4000-8000-000000000003",
        0,
        4032,
        3024,
        T0 + 125_231,
        -180.0,
        -180.0,
        "IMG_0013.heic",
        "Troi, Ten Forward",
    ),
    (
        4,
        "1A2B3C4D-0004-4000-8000-000000000004",
        1,
        6000,
        4000,
        T0 + 161_100,
        -180.0,
        -180.0,
        "IMG_0014.jpeg",
        "Worf, tactical",
    ),
]

# (Z_PK, ZUUID, ZKIND, ZTITLE); kind 2 is a user album.
ALBUMS = [
    (51, "A1B2C3D4-0051-4000-8000-000000000051", 2, "Bridge crew"),
    (52, "A1B2C3D4-0052-4000-8000-000000000052", 2, "Ten Forward"),
]

# (album Z_PK, asset Z_PK, sort key)
ALBUM_ASSETS = [(51, 1, 1024), (51, 2, 2048), (51, 4, 3072), (52, 3, 1024)]

# (Z_PK, asset Z_PK, cx, cy, ZUUID) — the last one Photos has not named.
FACES = [
    (71, 1, 0.41, 0.33, "F1E2D3C4-0071-4000-8000-000000000071"),
    (72, 2, 0.52, 0.30, "F1E2D3C4-0072-4000-8000-000000000072"),
    (73, 3, 0.48, 0.29, None),
]


def main(out_path: str) -> None:
    os.makedirs(os.path.dirname(out_path), exist_ok=True)
    con = sqlite3.connect(out_path)
    try:
        for stmt in SCHEMA:
            con.execute(stmt)

        for pk, uuid, fav, w, h, created, lat, lon, filename, title in ASSETS:
            con.execute(
                "INSERT INTO ZASSET (Z_PK, Z_ENT, Z_OPT, ZFAVORITE, ZHIDDEN, ZKIND, "
                " ZTRASHEDSTATE, ZWIDTH, ZHEIGHT, ZADDITIONALATTRIBUTES, ZDATECREATED, "
                " ZMODIFICATIONDATE, ZLATITUDE, ZLONGITUDE, ZDIRECTORY, ZFILENAME, "
                " ZUNIFORMTYPEIDENTIFIER, ZUUID) "
                "VALUES (?,3,1,?,0,0,0,?,?,?,?,?,?,?,?,?,?,?)",
                (
                    pk,
                    fav,
                    w,
                    h,
                    pk + 100,
                    float(created),
                    float(created),
                    lat,
                    lon,
                    uuid[0],
                    f"{uuid}.{filename.rsplit('.', 1)[1]}",
                    "public.heic" if filename.endswith(".heic") else "public.jpeg",
                    uuid,
                ),
            )
            con.execute(
                "INSERT INTO ZADDITIONALASSETATTRIBUTES (Z_PK, Z_ENT, Z_OPT, ZASSET, "
                " ZORIGINALFILENAME, ZTITLE, ZTIMEZONENAME, ZTIMEZONEOFFSET) "
                "VALUES (?,1,1,?,?,?,'America/Los_Angeles',-25200)",
                (pk + 100, pk, filename, title),
            )
            if lat > -180.0:
                con.execute(
                    "INSERT INTO Z_RT_Asset_boundedByRect VALUES (?,?,?,?,?)",
                    (pk, lat, lat, lon, lon),
                )
        con.executemany(
            "INSERT INTO ZGENERICALBUM (Z_PK, Z_ENT, Z_OPT, ZKIND, ZTRASHEDSTATE, "
            " ZCREATIONDATE, ZTITLE, ZUUID) VALUES (?,32,1,?,0,?,?,?)",
            [(pk, kind, float(T0), title, uuid) for pk, uuid, kind, title in ALBUMS],
        )
        con.executemany(
            "INSERT INTO Z_33ASSETS (Z_33ALBUMS, Z_3ASSETS, Z_FOK_3ASSETS) VALUES (?,?,?)",
            ALBUM_ASSETS,
        )
        con.executemany(
            "INSERT INTO ZDETECTEDFACE (Z_PK, Z_ENT, Z_OPT, ZASSETFORFACE, ZCENTERX, "
            " ZCENTERY, ZUUID) VALUES (?,23,1,?,?,?,?)",
            FACES,
        )

        # The bookkeeping a real library never stops writing.
        con.executemany(
            "INSERT INTO Z_PRIMARYKEY (Z_ENT, Z_NAME, Z_SUPER, Z_MAX) VALUES (?,?,0,?)",
            [
                (1, "AdditionalAssetAttributes", 104),
                (3, "Asset", 4),
                (23, "DetectedFace", 73),
                (32, "GenericAlbum", 52),
            ],
        )
        con.execute(
            "INSERT INTO Z_METADATA (Z_VERSION, Z_UUID, Z_PLIST) VALUES (1, ?, ?)",
            ("9E8D7C6B-5A49-4000-8000-00000000BEEF", b"bplist00"),
        )
        con.execute("INSERT INTO Z_MODELCACHE (Z_CONTENT) VALUES (?)", (b"\x00model",))
        con.executemany(
            "INSERT INTO ATRANSACTIONSTRING (Z_PK, Z_ENT, Z_OPT, ZNAME) VALUES (?,1,1,?)",
            [(1, "com.apple.Photos"), (2, "com.apple.photoanalysisd")],
        )
        con.executemany(
            "INSERT INTO ATRANSACTION (Z_PK, Z_ENT, Z_OPT, ZAUTHORTS, ZTIMESTAMP, ZAUTHOR, "
            " ZQUERYGEN) VALUES (?,2,1,?,?,?,NULL)",
            [(i, 1 + i % 2, float(T0 + i), "import") for i in range(1, 6)],
        )
        con.executemany(
            "INSERT INTO ACHANGE (Z_PK, Z_ENT, Z_OPT, ZCHANGETYPE, ZENTITY, ZENTITYPK, "
            " ZTRANSACTIONID, ZCOLUMNS) VALUES (?,3,1,0,3,?,?,NULL)",
            [(i, (i % 4) + 1, (i % 5) + 1) for i in range(1, 21)],
        )
        con.commit()
    finally:
        con.close()


if __name__ == "__main__":
    main(sys.argv[1])
