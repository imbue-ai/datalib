"""The F cases: which files make Lightroom store a time with an offset in
`captureTime`, or a time with no seconds.

A real catalog had both: an offset (one region's summer and winter offsets)
copied from `xmp:ModifyDate` on AVI, MPG, VOB and MOV videos and some JPEGs,
an offset on JPEGs whose XMP held no date at all, and `…T14:00` copied from
an `xmp:CreateDate` with no seconds on videos. These cases try each.

Usage: python3 -I make_more_cases.py <empty out_dir>

Every file that carries a date has its modified date set to 2001-01-01, as
in `make_cases.py`, so a time taken from the file's date stands out. The
files with no date at all get a summer or a winter modified date instead,
to see whether Lightroom stores that in UTC (as D06 did) or in this Mac's
zone with an offset, and whether daylight saving is applied.
"""

import csv
import hashlib
import os
import subprocess
import sys
from dataclasses import dataclass, field

OLD_MTIME = "2001:01:01 00:00:00"
SUMMER_MTIME = "2024:08:01 14:00:00-07:00"  # 2024-08-01 21:00 UTC
WINTER_MTIME = "2024:01:15 14:00:00-08:00"  # 2024-01-15 22:00 UTC
MOMENT = "2024-08-01T21:00:00Z"  # 14:00 at -07:00


@dataclass
class Case:
    name: str
    kind: str  # jpg, png, tif, mov, mp4, avi, mpg, vob
    asks: str
    tags: dict = field(default_factory=dict)
    mtime: str = OLD_MTIME
    true_utc: str = MOMENT

    @property
    def file(self) -> str:
        return f"{self.name}.{self.kind}"

    @property
    def writable(self) -> bool:
        """exiftool cannot write metadata into AVI, MPG or VOB."""
        return self.kind not in ("avi", "mpg", "vob")


def cases() -> list[Case]:
    none = "none: the file's date is the only time"
    return [
        # A modify date with an offset, the one source of an offset in
        # `captureTime` seen so far (D05, +02:00).
        Case(
            "F01",
            "jpg",
            "only EXIF ModifyDate 14:00 + OffsetTime -07:00 (D05 with a negative offset)",
            {"EXIF:ModifyDate": "2024:08:01 14:00:00", "EXIF:OffsetTime": "-07:00"},
        ),
        Case(
            "F02",
            "jpg",
            "only XMP xmp:ModifyDate 14:00-07:00, no EXIF date",
            {"XMP-xmp:ModifyDate": "2024:08:01 14:00:00-07:00"},
        ),
        Case(
            "F03",
            "jpg",
            "only XMP xmp:CreateDate 14:00-07:00, no EXIF date",
            {"XMP-xmp:CreateDate": "2024:08:01 14:00:00-07:00"},
        ),
        Case(
            "F04",
            "jpg",
            "only XMP xmp:CreateDate with no seconds, 14:00, no offset",
            {"XMP-xmp:CreateDate": "2024-08-01T14:00"},
        ),
        Case(
            "F05",
            "jpg",
            "only XMP xmp:CreateDate with no seconds, 14:00-07:00",
            {"XMP-xmp:CreateDate": "2024-08-01T14:00-07:00"},
        ),
        Case(
            "F06",
            "jpg",
            "only XMP xmp:MetadataDate 14:00-07:00",
            {"XMP-xmp:MetadataDate": "2024:08:01 14:00:00-07:00"},
        ),
        Case(
            "F07",
            "jpg",
            "only IPTC DateCreated + TimeCreated 14:00:00-07:00 (as scanners write)",
            {"IPTC:DateCreated": "2024:08:01", "IPTC:TimeCreated": "14:00:00-07:00"},
        ),
        Case(
            "F08",
            "jpg",
            "EXIF DateTimeOriginal 14:00 with no offset, and XMP xmp:ModifyDate 14:00-07:00",
            {
                "EXIF:DateTimeOriginal": "2024:08:01 14:00:00",
                "XMP-xmp:ModifyDate": "2024:08:01 14:00:00-07:00",
            },
        ),
        # No date at all: what does Lightroom make of the file's own date?
        Case(
            "F10",
            "jpg",
            "no dates; file modified in summer (2024-08-01 21:00 UTC)",
            mtime=SUMMER_MTIME,
            true_utc=none,
        ),
        Case(
            "F11",
            "jpg",
            "no dates; file modified in winter (2024-01-15 22:00 UTC)",
            mtime=WINTER_MTIME,
            true_utc=none,
        ),
        Case(
            "F12",
            "png",
            "PNG, no dates; file modified in summer",
            mtime=SUMMER_MTIME,
            true_utc=none,
        ),
        Case(
            "F13",
            "tif",
            "TIFF, no dates; file modified in summer",
            mtime=SUMMER_MTIME,
            true_utc=none,
        ),
        # Video containers `make_cases.py` has none of.
        Case(
            "F20",
            "avi",
            "AVI, no dates; file modified in summer",
            mtime=SUMMER_MTIME,
            true_utc=none,
        ),
        Case(
            "F21",
            "avi",
            "AVI, no dates; file modified in winter",
            mtime=WINTER_MTIME,
            true_utc=none,
        ),
        Case(
            "F22",
            "avi",
            "AVI with a RIFF creation date (ICRD) 2024-08-01; file modified 2001",
            {"ffmpeg:date": "2024-08-01"},
            true_utc="2024-08-01, no time",
        ),
        Case(
            "F23",
            "mpg",
            "MPEG-1, no dates; file modified in summer",
            mtime=SUMMER_MTIME,
            true_utc=none,
        ),
        Case(
            "F24",
            "vob",
            "VOB (MPEG-2), no dates; file modified in summer",
            mtime=SUMMER_MTIME,
            true_utc=none,
        ),
        Case(
            "F25",
            "mov",
            "MOV, QuickTime dates empty, XMP xmp:ModifyDate 14:00-07:00",
            {"XMP-xmp:ModifyDate": "2024:08:01 14:00:00-07:00"},
        ),
        Case(
            "F26",
            "mov",
            "MOV, QuickTime dates empty, iPhone-style Keys:CreationDate 14:00:00-07:00 (a negative V03)",
            {"Keys:CreationDate": "2024:08:01 14:00-07:00"},
        ),
        Case(
            "F27",
            "mp4",
            "MP4, QuickTime dates empty, XMP xmp:CreateDate with no seconds, 14:00",
            {"XMP-xmp:CreateDate": "2024-08-01T14:00"},
        ),
    ]


def run(*args):
    subprocess.run(args, check=True, capture_output=True)


def make_media(c: Case, path: str):
    if c.kind in ("jpg", "png", "tif"):
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
        return
    codec = {
        "mov": ["-c:v", "libx264", "-pix_fmt", "yuv420p"],
        "mp4": ["-c:v", "libx264", "-pix_fmt", "yuv420p"],
        # Motion JPEG and PCM sound, as old cameras wrote; Lightroom
        # refused an MPEG-4 Part 2 AVI as unrecognizable.
        "avi": [
            "-f",
            "lavfi",
            "-i",
            "anullsrc=r=8000:cl=mono",
            "-shortest",
            "-c:v",
            "mjpeg",
            "-q:v",
            "5",
            "-pix_fmt",
            "yuvj420p",
            "-c:a",
            "pcm_u8",
        ],
        "mpg": ["-c:v", "mpeg1video", "-f", "mpeg"],
        "vob": ["-c:v", "mpeg2video", "-f", "vob"],
    }[c.kind]
    meta = [
        a
        for k, v in c.tags.items()
        if k.startswith("ffmpeg:")
        for a in ("-metadata", f"{k[7:]}={v}")
    ]
    run(
        "ffmpeg",
        "-loglevel",
        "error",
        "-f",
        "lavfi",
        "-i",
        "color=black:s=320x240:d=2:r=25",
        *codec,
        "-map_metadata",
        "-1",
        *meta,
        path,
    )


def write_tags(c: Case, path: str):
    if c.writable:
        args = ["exiftool", "-q", "-overwrite_original", "-m"]
        if c.kind in ("jpg", "png", "tif"):
            args.append("-all=")
        args += [f"-{k}={v}" for k, v in c.tags.items() if not k.startswith("ffmpeg:")]
        args += [f"-XMP-dc:Title={c.name}", f"-XMP-dc:Description={c.asks}", path]
        run(*args)
    # Last, so nothing above moves it.
    run("exiftool", "-q", "-overwrite_original", f"-FileModifyDate={c.mtime}", path)


def written(path: str) -> str:
    """Every date and time tag the file now holds, as exiftool reads it."""
    out = subprocess.run(
        ["exiftool", "-a", "-G1", "-s", "-time:all", "-x", "System:all", path],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return "; ".join(" ".join(line.split()) for line in out.splitlines())


def sha256(path: str) -> str:
    with open(path, "rb") as f:
        return hashlib.sha256(f.read()).hexdigest()


def main():
    out_dir = sys.argv[1]
    os.makedirs(out_dir, exist_ok=True)
    if os.listdir(out_dir):
        sys.exit(f"{out_dir} is not empty")
    rows = []
    for c in cases():
        path = os.path.join(out_dir, c.file)
        make_media(c, path)
        write_tags(c, path)
        rows.append(
            {
                "file": c.file,
                "true_moment_utc": c.true_utc,
                "asks": c.asks,
                "file_modified": c.mtime,
                "written": written(path),
            }
        )
    with open(os.path.join(out_dir, "manifest.csv"), "w", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(rows[0]))
        w.writeheader()
        w.writerows(rows)
    with open(os.path.join(out_dir, "hashes.txt"), "w") as f:
        for c in cases():
            f.write(f"{sha256(os.path.join(out_dir, c.file))}  {c.file}\n")
    print(f"{len(rows)} files in {out_dir}")


if __name__ == "__main__":
    main()
