"""Make black test photos and videos whose capture times and time zones are
written in many different ways, to see how Lightroom Classic reads, shows,
sorts and edits them.

Usage: python3 -I make_cases.py <empty out_dir>

Needs exiftool, ImageMagick (`magick`) and ffmpeg on PATH. Writes the media,
`manifest.csv` (one row per file: what is written, the true moment, what the
case asks) and `hashes.txt` (each file's sha256 as made, so a later run can
tell which files Lightroom rewrote). Refuses a non-empty out_dir.
"""

import csv
import hashlib
import os
import subprocess
import sys
from dataclasses import dataclass, field

BERLIN = ("52.5200", "N", "13.4050", "E")
SF = ("37.7749", "N", "122.4194", "W")
TOKYO = ("35.6762", "N", "139.6503", "E")
REYKJAVIK = ("64.1466", "N", "21.9426", "W")  # UTC all year

OLD_MTIME = "2001:01:01 00:00:00"


@dataclass
class Case:
    name: str
    group: str
    true_utc: str  # the moment it stands for, or why there is none
    asks: str
    tags: dict = field(default_factory=dict)
    gps: tuple | None = None
    video: str | None = None  # "mov" or "mp4"
    edit: str = ""  # what to do to it in Lightroom, for the edit group

    @property
    def file(self) -> str:
        return f"{self.name}.{self.video or 'jpg'}"


def exif_photo(dto=None, offset=None, sub=None):
    t = {}
    if dto:
        t["EXIF:DateTimeOriginal"] = dto
    if offset:
        t["EXIF:OffsetTimeOriginal"] = offset
    if sub:
        t["EXIF:SubSecTimeOriginal"] = sub
    return t


def cases() -> list[Case]:
    out: list[Case] = []

    # A: one moment, 2024-07-01 12:00:00 UTC, spelled many ways. What time
    # does Lightroom show for each?
    a = "2024-07-01T12:00:00Z"
    A = [
        ("A01", "UTC clock, no offset", exif_photo("2024:07:01 12:00:00"), None),
        (
            "A02",
            "Berlin clock +02:00",
            exif_photo("2024:07:01 14:00:00", "+02:00"),
            None,
        ),
        ("A03", "Berlin clock, no offset", exif_photo("2024:07:01 14:00:00"), None),
        (
            "A04",
            "San Francisco clock -07:00",
            exif_photo("2024:07:01 05:00:00", "-07:00"),
            None,
        ),
        (
            "A05",
            "San Francisco clock, no offset",
            exif_photo("2024:07:01 05:00:00"),
            None,
        ),
        (
            "A06",
            "Tokyo clock +09:00",
            exif_photo("2024:07:01 21:00:00", "+09:00"),
            None,
        ),
        (
            "A07",
            "Mumbai clock +05:30",
            exif_photo("2024:07:01 17:30:00", "+05:30"),
            None,
        ),
        (
            "A08",
            "Berlin clock, no offset, GPS Berlin",
            exif_photo("2024:07:01 14:00:00"),
            BERLIN,
        ),
        (
            "A09",
            "Berlin clock, no offset, GPS San Francisco",
            exif_photo("2024:07:01 14:00:00"),
            SF,
        ),
        (
            "A10",
            "Berlin clock +02:00, GPS San Francisco",
            exif_photo("2024:07:01 14:00:00", "+02:00"),
            SF,
        ),
        (
            "A11",
            "UTC clock, no offset, GPS Reykjavik",
            exif_photo("2024:07:01 12:00:00"),
            REYKJAVIK,
        ),
    ]
    for name, what, tags, gps in A:
        out.append(
            Case(name, "A same moment", a, f"{what}: what time is shown?", tags, gps)
        )

    # B: twelve moments a minute apart from 2024-07-02 12:00 UTC, in the
    # file names' order, each spelled one of six ways. Sorted by capture
    # time, do they come out B01..B12?
    spellings = [
        ("UTC clock, no offset", 0, False),
        ("Berlin clock +02:00", 2, True),
        ("San Francisco clock -07:00", -7, True),
        ("Tokyo clock +09:00", 9, True),
        ("Berlin clock, no offset", 2, False),
        ("San Francisco clock, no offset", -7, False),
    ]
    for i in range(12):
        what, hours, with_offset = spellings[i % 6]
        minute = i
        h = (12 + hours) % 24
        day = 2 + (12 + hours) // 24
        dto = f"2024:07:{day:02d} {h:02d}:{minute:02d}:00"
        off = (
            f"{'+' if hours >= 0 else '-'}{abs(hours):02d}:00" if with_offset else None
        )
        out.append(
            Case(
                f"B{i + 1:02d}",
                "B sort order",
                f"2024-07-02T12:{minute:02d}:00Z",
                f"{what}: is it in place B01..B12?",
                exif_photo(dto, off),
            )
        )

    # C: Berlin's daylight-saving edges, all with GPS Berlin.
    C = [
        (
            "C01",
            "2024-10-27T00:30:00Z",
            "02:30 +02:00, the first 02:30 of the night clocks go back",
            exif_photo("2024:10:27 02:30:00", "+02:00"),
        ),
        (
            "C02",
            "2024-10-27T01:30:00Z",
            "02:30 +01:00, the second 02:30 of that night",
            exif_photo("2024:10:27 02:30:00", "+01:00"),
        ),
        (
            "C03",
            "ambiguous",
            "02:15 on the night clocks go back, no offset",
            exif_photo("2024:10:27 02:15:00"),
        ),
        (
            "C04",
            "does not exist in Berlin",
            "02:30 on the night clocks go forward, no offset",
            exif_photo("2024:03:31 02:30:00"),
        ),
        (
            "C05",
            "2024-03-31T01:30:00Z",
            "02:30 +01:00 on the night clocks go forward (a clock not yet moved)",
            exif_photo("2024:03:31 02:30:00", "+01:00"),
        ),
    ]
    for name, true, what, tags in C:
        out.append(
            Case(
                name,
                "C daylight saving",
                true,
                f"{what}: shown and sorted how?",
                tags,
                BERLIN,
            )
        )

    # D: where Lightroom takes the time from when the usual field is
    # missing or disagrees. Every one stands for 2024-07-03 12:00 UTC,
    # except D10a/b and D06.
    d = "2024-07-03T12:00:00Z"
    D = [
        (
            "D01",
            d,
            "EXIF Berlin +02:00, XMP DateTimeOriginal San Francisco -07:00: which wins?",
            {
                **exif_photo("2024:07:03 14:00:00", "+02:00"),
                "XMP-exif:DateTimeOriginal": "2024:07:03 05:00:00-07:00",
            },
            None,
        ),
        (
            "D02",
            d,
            "EXIF Berlin clock no offset, XMP DateTimeOriginal with +02:00",
            {
                **exif_photo("2024:07:03 14:00:00"),
                "XMP-exif:DateTimeOriginal": "2024:07:03 14:00:00+02:00",
            },
            None,
        ),
        (
            "D03",
            d,
            "only XMP DateTimeOriginal (+02:00), no EXIF dates",
            {"XMP-exif:DateTimeOriginal": "2024:07:03 14:00:00+02:00"},
            None,
        ),
        (
            "D04",
            d,
            "only EXIF CreateDate (digitized) with OffsetTimeDigitized +02:00",
            {
                "EXIF:CreateDate": "2024:07:03 14:00:00",
                "EXIF:OffsetTimeDigitized": "+02:00",
            },
            None,
        ),
        (
            "D05",
            d,
            "only EXIF ModifyDate with OffsetTime +02:00",
            {"EXIF:ModifyDate": "2024:07:03 14:00:00", "EXIF:OffsetTime": "+02:00"},
            None,
        ),
        (
            "D06",
            "none (file modified 2001-01-01)",
            "no dates at all: does Lightroom use the file's date?",
            {},
            None,
        ),
        (
            "D07",
            d,
            "only GPS date and time stamp (12:00 UTC), no DateTimeOriginal",
            {"GPS:GPSDateStamp": "2024:07:03", "GPS:GPSTimeStamp": "12:00:00"},
            BERLIN,
        ),
        (
            "D08",
            d,
            "Berlin clock no offset, GPS San Francisco with GPS stamp 12:00 UTC (camera on home time abroad)",
            {
                **exif_photo("2024:07:03 14:00:00"),
                "GPS:GPSDateStamp": "2024:07:03",
                "GPS:GPSTimeStamp": "12:00:00",
            },
            SF,
        ),
        (
            "D09",
            d,
            "Berlin clock, offset only in OffsetTime (not OffsetTimeOriginal)",
            {**exif_photo("2024:07:03 14:00:00"), "EXIF:OffsetTime": "+02:00"},
            None,
        ),
        (
            "D10a",
            "2024-07-03T12:30:00.90Z",
            "same second as D10b, subseconds .90: does D10b sort first?",
            exif_photo("2024:07:03 14:30:00", "+02:00", "90"),
            None,
        ),
        (
            "D10b",
            "2024-07-03T12:30:00.10Z",
            "same second as D10a, subseconds .10",
            exif_photo("2024:07:03 14:30:00", "+02:00", "10"),
            None,
        ),
        (
            "D11",
            d,
            "only XMP photoshop:DateCreated with +02:00",
            {"XMP-photoshop:DateCreated": "2024:07:03 14:00:00+02:00"},
            None,
        ),
    ]
    for name, true, what, tags, gps in D:
        out.append(Case(name, "D fallbacks", true, what, tags, gps))

    # E: to edit with Edit Capture Time. Each EnnX twin carries the same
    # metadata and is left alone, so the two can be compared afterwards.
    e = "2024-07-04T12:00:00Z"
    E = [
        (
            "E01",
            "Berlin clock 14:00 +02:00",
            exif_photo("2024:07:04 14:00:00", "+02:00"),
            None,
            None,
            "Shift by set number of hours: +1",
        ),
        (
            "E02",
            "Berlin clock 14:00, no offset",
            exif_photo("2024:07:04 14:00:00"),
            None,
            None,
            "Shift by set number of hours: +1",
        ),
        (
            "E03",
            "San Francisco clock 05:00 -07:00",
            exif_photo("2024:07:04 05:00:00", "-07:00"),
            None,
            None,
            "Adjust to a specified date and time: 2024-07-04 14:00:00",
        ),
        (
            "E04",
            "UTC clock 12:00, no offset, GPS Berlin (camera left on UTC)",
            exif_photo("2024:07:04 12:00:00"),
            BERLIN,
            None,
            "Shift by set number of hours: +2",
        ),
        (
            "E05",
            "video, QuickTime CreateDate 12:00 (UTC, as the format defines it)",
            {
                "QuickTime:CreateDate": "2024:07:04 12:00:00",
                "QuickTime:ModifyDate": "2024:07:04 12:00:00",
                "QuickTime:TrackCreateDate": "2024:07:04 12:00:00",
                "QuickTime:MediaCreateDate": "2024:07:04 12:00:00",
            },
            None,
            "mov",
            "Shift by set number of hours: +2",
        ),
        (
            "E06",
            "no dates at all (like a scan); file modified 2001-01-01",
            {},
            None,
            None,
            "Adjust to a specified date and time: 1968-05-01 12:00:00",
        ),
    ]
    for name, what, tags, gps, video, edit in E:
        true = "none" if name == "E06" else e
        out.append(
            Case(
                name,
                "E edit capture time",
                true,
                f"{what}: edit it",
                tags,
                gps,
                video,
                edit,
            )
        )
        out.append(
            Case(
                f"{name}X",
                "E edit capture time",
                true,
                f"{what}: leave alone (twin of {name})",
                tags,
                gps,
                video,
                "none: twin of " + name,
            )
        )

    # V: videos, all for 2024-07-05 12:00 UTC, with two photos a minute
    # either side of them, so the sort shows where each video lands.
    v = "2024-07-05T12:00:00Z"
    qt = lambda s: {
        "QuickTime:CreateDate": s,
        "QuickTime:ModifyDate": s,
        "QuickTime:TrackCreateDate": s,
        "QuickTime:MediaCreateDate": s,
    }
    out.append(
        Case(
            "V00",
            "V video",
            "2024-07-05T11:59:00Z",
            "photo, Berlin clock 13:59 +02:00: just before the videos",
            exif_photo("2024:07:05 13:59:00", "+02:00"),
        )
    )
    V = [
        (
            "V01",
            "mov",
            "QuickTime CreateDate 12:00 (UTC, correct per the format)",
            qt("2024:07:05 12:00:00"),
        ),
        (
            "V02",
            "mov",
            "QuickTime CreateDate 14:00 (local clock written as if UTC, as many cameras do)",
            qt("2024:07:05 14:00:00"),
        ),
        (
            "V03",
            "mov",
            "iPhone style: Keys:CreationDate 14:00+02:00 plus QuickTime CreateDate 12:00 UTC",
            {
                **qt("2024:07:05 12:00:00"),
                "Keys:CreationDate": "2024:07:05 14:00:00+02:00",
            },
        ),
        (
            "V04",
            "mp4",
            "MP4, QuickTime CreateDate 12:00 (UTC)",
            qt("2024:07:05 12:00:00"),
        ),
        ("V05", "mov", "no dates at all; file modified 2001-01-01", {}),
        (
            "V06",
            "mov",
            "XMP DateTimeOriginal 14:00+02:00, QuickTime dates empty",
            {"XMP-exif:DateTimeOriginal": "2024:07:05 14:00:00+02:00"},
        ),
    ]
    for name, kind, what, tags in V:
        true = "none" if name == "V05" else v
        out.append(
            Case(
                name,
                "V video",
                true,
                f"{what}: shown and sorted how?",
                tags,
                None,
                kind,
            )
        )
    out.append(
        Case(
            "V99",
            "V video",
            "2024-07-05T12:01:00Z",
            "photo, Berlin clock 14:01 +02:00: just after the videos",
            exif_photo("2024:07:05 14:01:00", "+02:00"),
        )
    )
    return out


def run(*args):
    subprocess.run(args, check=True, capture_output=True)


def make_media(c: Case, path: str):
    if c.video:
        run(
            "ffmpeg",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=black:s=320x240:d=2:r=10",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-map_metadata",
            "-1",
            path,
        )
    else:
        run(
            "magick",
            "-size",
            "320x240",
            "xc:black",
            "-fill",
            "#555",
            "-pointsize",
            "64",
            "-gravity",
            "center",
            "-annotate",
            "0",
            c.name,
            path,
        )


def write_tags(c: Case, path: str):
    args = ["exiftool", "-q", "-overwrite_original", "-m"]
    if not c.video:
        args.append("-all=")
    for tag, value in c.tags.items():
        args.append(f"-{tag}={value}")
    if c.gps:
        lat, lat_ref, lng, lng_ref = c.gps
        args += [
            f"-GPSLatitude={lat}",
            f"-GPSLatitudeRef={lat_ref}",
            f"-GPSLongitude={lng}",
            f"-GPSLongitudeRef={lng_ref}",
        ]
    args += [f"-XMP-dc:Title={c.name}", f"-XMP-dc:Description={c.asks}"]
    args.append(path)
    run(*args)
    # Last, so nothing above moves it.
    run("exiftool", "-q", "-overwrite_original", f"-FileModifyDate={OLD_MTIME}", path)


def sha256(path: str) -> str:
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def main():
    out_dir = sys.argv[1]
    os.makedirs(out_dir, exist_ok=True)
    if os.listdir(out_dir):
        sys.exit(f"{out_dir} is not empty")
    all_cases = cases()
    rows = []
    for c in all_cases:
        path = os.path.join(out_dir, c.file)
        make_media(c, path)
        write_tags(c, path)
        rows.append(
            {
                "file": c.file,
                "group": c.group,
                "true_moment_utc": c.true_utc,
                "written": "; ".join(f"{k}={v}" for k, v in c.tags.items())
                + (f"; GPS={c.gps[0]}{c.gps[1]},{c.gps[2]}{c.gps[3]}" if c.gps else ""),
                "asks": c.asks,
                "edit_in_lightroom": c.edit,
            }
        )
    with open(os.path.join(out_dir, "manifest.csv"), "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(rows[0]))
        w.writeheader()
        w.writerows(rows)
    with open(os.path.join(out_dir, "hashes.txt"), "w") as f:
        for c in all_cases:
            f.write(f"{sha256(os.path.join(out_dir, c.file))}  {c.file}\n")
    print(f"{len(all_cases)} files in {out_dir}")


if __name__ == "__main__":
    main()
