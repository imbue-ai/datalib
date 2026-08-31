#!/usr/bin/env python3
"""Generate the `media` provider's fixture corpus.

Hand-built rather than produced by encoders, for the same three reasons
`make_pdf_fixtures.py` gives: the files stay small enough to review, the
bytes are deterministic (their blake3s are the provider's primary keys,
so drift would churn every assertion), and we can place an ID3 tag, an
EXIF block or a DNG preview exactly where the payload-hash tests need
one.

The corpus is built around **pairs**: two files with identical signal
and different metadata. Those are what prove `payload_blake3` does the
job it exists for, and they are the reason this generator writes bytes
directly instead of shelling out to ffmpeg.

Output is checked in; this script exists to regenerate it:

    uv run python tests/fixtures/make_media_fixtures.py

It lives here rather than beside the provider because `tests/fixtures`
is one of the PYTHON_LINT_ROOTS (see scripts/lint_repo.py), so a
generator parked in a provider directory would be linted by nothing.

TNG-themed, per the repo's fixture convention.
"""

from __future__ import annotations

import pathlib
import struct
import zlib
from collections.abc import Sequence

OUT = (
    pathlib.Path(__file__).resolve().parents[2]
    / "datalib"
    / "backend"
    / "etl"
    / "providers"
    / "media"
    / "tests"
    / "fixtures"
    / "media_tng"
)

# ─────────────────────────────────────────────────────────────────────
# MP3


def mpeg_frame(fill: int) -> bytes:
    """One MPEG-1 Layer III frame: 128 kbps, 44.1 kHz, joint stereo.

    144 * 128000 / 44100 = 417 bytes, no padding. The four header bytes
    are what the provider's frame walker locks onto; the rest is filler
    standing in for coded audio.
    """
    return bytes([0xFF, 0xFB, 0x90, 0x44]) + bytes([fill]) * 413


def xing_frame() -> bytes:
    """A VBR header frame: structurally a real frame, decodes to
    silence, carries the seek table and ReplayGain.

    The `Xing` magic sits 36 bytes in, which is 4 (header) + 32 (Layer
    III side info for MPEG-1 stereo). Tools rewrite this frame without
    touching audio, which is exactly why the payload hash skips it.
    """
    f = bytearray(mpeg_frame(0x00))
    f[36:40] = b"Xing"
    f[40:44] = struct.pack(">I", 0x0F)  # flags: frames/bytes/toc/quality
    return bytes(f)


def syncsafe(n: int) -> bytes:
    """ID3v2's seven-bits-per-byte size encoding, so a size field can
    never look like a frame sync."""
    return bytes([(n >> 21) & 0x7F, (n >> 14) & 0x7F, (n >> 7) & 0x7F, n & 0x7F])


def id3v2(fields: dict[str, str], pad: int = 0) -> bytes:
    """An ID3v2.4 tag. `fields` maps four-character frame ids to text."""
    body = b""
    for fid, text in fields.items():
        payload = b"\x03" + text.encode()  # 0x03 = UTF-8
        body += fid.encode() + syncsafe(len(payload)) + b"\x00\x00" + payload
    body += b"\x00" * pad
    return b"ID3\x04\x00\x00" + syncsafe(len(body)) + body


def id3v1(title: str, artist: str) -> bytes:
    """The flat 128-byte trailer. Still written by plenty of tools."""
    return (
        b"TAG"
        + title.encode().ljust(30, b"\0")
        + artist.encode().ljust(30, b"\0")
        + b"\0" * 30  # album
        + b"\0" * 4  # year
        + b"\0" * 30  # comment
        + b"\x00"  # genre
    )


# The audio every `ode_to_spot` variant shares. The payload hash must be
# identical across all of them.
ODE_AUDIO = mpeg_frame(0xA1) + mpeg_frame(0xA2) + mpeg_frame(0xA3)


def mp3_file(tags: dict[str, str], *, xing: bool = True, v1: bool = False) -> bytes:
    out = id3v2(tags)
    if xing:
        out += xing_frame()
    out += ODE_AUDIO
    if v1:
        out += id3v1(tags.get("TIT2", ""), tags.get("TPE1", ""))
    return out


# ─────────────────────────────────────────────────────────────────────
# FLAC


def flac_block(kind: int, last: bool, body: bytes) -> bytes:
    return bytes([kind | (0x80 if last else 0)]) + len(body).to_bytes(3, "big") + body


def streaminfo(sample_rate: int, channels: int, bits: int, samples: int) -> bytes:
    """The 34-byte STREAMINFO block.

    The packed field is 20 bits of sample rate, 3 of (channels - 1), 5
    of (bits - 1), then 36 of total samples. lofty reads its duration
    and sample rate from here.
    """
    packed = (sample_rate << 44) | ((channels - 1) << 41) | ((bits - 1) << 36) | samples
    return (
        struct.pack(">HH", 4096, 4096)  # min/max block size
        + (0).to_bytes(3, "big")  # min frame size
        + (0).to_bytes(3, "big")  # max frame size
        + packed.to_bytes(8, "big")
        + b"\x00" * 16  # MD5 of the decoded samples; see flac.rs on why unused
    )


def vorbis_comment(vendor: str, tags: dict[str, str]) -> bytes:
    out = struct.pack("<I", len(vendor)) + vendor.encode()
    out += struct.pack("<I", len(tags))
    for k, v in tags.items():
        item = f"{k}={v}".encode()
        out += struct.pack("<I", len(item)) + item
    return out


HUM_AUDIO = b"\xff\xf8\x69\x18" + bytes(range(256)) * 4


def flac_file(tags: dict[str, str], *, art: bytes = b"") -> bytes:
    blocks = [flac_block(0, False, streaminfo(44100, 2, 16, 132300))]
    blocks.append(flac_block(4, False, vorbis_comment("reference libFLAC", tags)))
    if art:
        blocks.append(flac_block(6, False, art))
    blocks.append(flac_block(1, True, b"\x00" * 64))  # PADDING
    return b"fLaC" + b"".join(blocks) + HUM_AUDIO


# ─────────────────────────────────────────────────────────────────────
# RIFF: WAV


def riff_chunk(cid: bytes, body: bytes) -> bytes:
    out = cid + struct.pack("<I", len(body)) + body
    return out + (b"\x00" if len(body) % 2 else b"")


TEA_SAMPLES = bytes(range(256)) * 8


def wav_file(info: dict[bytes, str]) -> bytes:
    fmt = struct.pack("<HHIIHH", 1, 2, 44100, 44100 * 4, 4, 16)
    body = b"WAVE" + riff_chunk(b"fmt ", fmt)
    if info:
        list_body = b"INFO"
        for k, v in info.items():
            list_body += riff_chunk(k, v.encode() + b"\x00")
        body += riff_chunk(b"LIST", list_body)
    body += riff_chunk(b"data", TEA_SAMPLES)
    return b"RIFF" + struct.pack("<I", len(body)) + body


# ─────────────────────────────────────────────────────────────────────
# TIFF / EXIF
#
# One encoder serves the JPEG APP1 block and the DNG, because they are
# the same structure: a TIFF header, a chain of IFDs, and values that
# live inline when they fit in four bytes and at an offset when they do
# not. That four-byte inline rule is the classic TIFF footgun and is
# exercised deliberately below (Orientation inline, Make out-of-line).

BYTE, ASCII, SHORT, LONG, RATIONAL = 1, 2, 3, 4, 5
TYPE_SIZE = {BYTE: 1, ASCII: 1, SHORT: 2, LONG: 4, RATIONAL: 8}

# One IFD entry: `(tag, field type, values)`. The value shape depends on
# the type — a str for ASCII, a list of ints for SHORT/LONG, a list of
# (num, den) pairs for RATIONAL — so the third slot is deliberately
# untyped and `_encode` dispatches on the type code.
Entry = tuple[int, int, object]


def _encode(typ: int, values) -> tuple[int, bytes]:
    if typ == ASCII:
        raw = values.encode() + b"\x00"
        return len(raw), raw
    if typ == BYTE:
        return len(values), bytes(values)
    if typ == SHORT:
        return len(values), b"".join(struct.pack("<H", v) for v in values)
    if typ == LONG:
        return len(values), b"".join(struct.pack("<I", v) for v in values)
    if typ == RATIONAL:
        return len(values), b"".join(struct.pack("<II", n, d) for n, d in values)
    raise ValueError(f"unsupported TIFF type {typ}")


def pack_ifd(entries: Sequence[Entry], data_at: int, next_ifd: int = 0):
    """Pack one IFD. Returns `(ifd_bytes, data_bytes)`.

    `data_at` is the absolute offset the out-of-line block will occupy,
    which the caller has already worked out from the IFD sizes.
    """
    entries = sorted(entries, key=lambda e: e[0])  # TIFF requires tag order
    ifd = struct.pack("<H", len(entries))
    data = b""
    for tag, typ, values in entries:
        count, raw = _encode(typ, values)
        ifd += struct.pack("<HHI", tag, typ, count)
        if len(raw) <= 4:
            ifd += raw.ljust(4, b"\x00")
        else:
            ifd += struct.pack("<I", data_at + len(data))
            data += raw
            if len(data) % 2:
                data += b"\x00"  # keep following values word-aligned
    ifd += struct.pack("<I", next_ifd)
    return ifd, data


def ifd_size(entries: Sequence[Entry]) -> int:
    return 2 + 12 * len(entries) + 4


def exif_block(
    *,
    make: str,
    model: str,
    lens: str,
    description: str,
    when: str,
    offset: str,
    iso: int,
    width: int,
    height: int,
    gps: tuple[float, float, float] | None,
) -> bytes:
    """A complete TIFF block: IFD0, the Exif SubIFD, and the GPS IFD.

    Returned without the `Exif\\0\\0` prefix, so the same bytes can go
    into a JPEG APP1 segment or stand alone.
    """
    gps_entries: list[Entry] = []
    if gps:
        lat, lon, alt = gps

        def dms(v: float):
            v = abs(v)
            d = int(v)
            m = int((v - d) * 60)
            s = round((v - d - m / 60) * 3600 * 100)
            return [(d, 1), (m, 1), (s, 100)]

        gps_entries = [
            (1, ASCII, "N" if lat >= 0 else "S"),
            (2, RATIONAL, dms(lat)),
            (3, ASCII, "E" if lon >= 0 else "W"),
            (4, RATIONAL, dms(lon)),
            (5, BYTE, [0 if alt >= 0 else 1]),
            (6, RATIONAL, [(int(abs(alt) * 100), 100)]),
        ]

    exif_entries: list[Entry] = [
        (0x829A, RATIONAL, [(1, 250)]),  # ExposureTime
        (0x829D, RATIONAL, [(28, 10)]),  # FNumber
        (0x8827, SHORT, [iso]),  # PhotographicSensitivity
        (0x9003, ASCII, when),  # DateTimeOriginal
        (0x9011, ASCII, offset),  # OffsetTimeOriginal
        (0x920A, RATIONAL, [(50, 1)]),  # FocalLength
        (0xA002, LONG, [width]),  # PixelXDimension
        (0xA003, LONG, [height]),  # PixelYDimension
        (0xA434, ASCII, lens),  # LensModel
    ]
    ifd0_entries: list[Entry] = [
        (0x010E, ASCII, description),  # ImageDescription
        (0x010F, ASCII, make),  # Make
        (0x0110, ASCII, model),  # Model
        (0x0112, SHORT, [1]),  # Orientation — inline, being 2 bytes
        (0x8769, LONG, [0]),  # ExifIFDPointer, patched below
    ]
    if gps_entries:
        ifd0_entries.append((0x8825, LONG, [0]))  # GPSInfoIFDPointer

    ifd0_at = 8
    exif_at = ifd0_at + ifd_size(ifd0_entries)
    gps_at = exif_at + ifd_size(exif_entries)
    data_at = gps_at + (ifd_size(gps_entries) if gps_entries else 0)

    # Now that the offsets are known, fill in the pointers.
    ifd0_entries = [
        (t, ty, ([exif_at] if t == 0x8769 else [gps_at] if t == 0x8825 else v))
        for (t, ty, v) in ifd0_entries
    ]

    ifd0, d0 = pack_ifd(ifd0_entries, data_at)
    exif, d1 = pack_ifd(exif_entries, data_at + len(d0))
    if gps_entries:
        gps_ifd, d2 = pack_ifd(gps_entries, data_at + len(d0) + len(d1))
    else:
        gps_ifd, d2 = b"", b""

    return (
        b"II\x2a\x00"
        + struct.pack("<I", ifd0_at)
        + ifd0
        + exif
        + gps_ifd
        + d0
        + d1
        + d2
    )


# ─────────────────────────────────────────────────────────────────────
# JPEG


def jpeg_segment(marker: int, body: bytes) -> bytes:
    return bytes([0xFF, marker]) + struct.pack(">H", len(body) + 2) + body


# Quantization and Huffman tables plus the entropy-coded scan: the part
# of a JPEG the payload hash keeps. The scan deliberately contains a
# stuffed `FF 00` and a restart marker, so the scan-end walk is
# exercised by a real fixture and not only by a unit test.
DQT = bytes([0x00]) + bytes(range(16, 80))
SOF0 = bytes([0x08, 0x01, 0x40, 0x01, 0xE0, 0x01, 0x01, 0x11, 0x00])
DHT = (
    bytes([0x00])
    + bytes([0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0])
    + bytes(range(12))
)
SOS_HDR = bytes([0x01, 0x01, 0x00, 0x00, 0x3F, 0x00])
BRIDGE_SCAN = bytes([0xA1, 0xB2, 0xFF, 0x00, 0xC3, 0xFF, 0xD0]) + bytes(range(200)) * 2


def jpeg_file(exif: bytes, *, comment: str = "") -> bytes:
    out = b"\xff\xd8"
    out += jpeg_segment(0xE0, b"JFIF\x00\x01\x02\x01\x00\x48\x00\x48\x00\x00")
    if exif:
        out += jpeg_segment(0xE1, b"Exif\x00\x00" + exif)
    if comment:
        out += jpeg_segment(0xFE, comment.encode())
    out += jpeg_segment(0xDB, DQT)
    out += jpeg_segment(0xC0, SOF0)
    out += jpeg_segment(0xC4, DHT)
    out += jpeg_segment(0xDA, SOS_HDR)
    out += BRIDGE_SCAN
    out += b"\xff\xd9"
    return out


# ─────────────────────────────────────────────────────────────────────
# PNG


def png_chunk(ctype: bytes, body: bytes) -> bytes:
    return (
        struct.pack(">I", len(body))
        + ctype
        + body
        + struct.pack(">I", zlib.crc32(ctype + body) & 0xFFFFFFFF)
    )


def png_file(*, text: dict[str, str]) -> bytes:
    width, height = 4, 3
    ihdr = struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0)
    raw = b"".join(
        b"\x00" + bytes([0x40 + y, 0x80, 0xC0, 0xFF] * width) for y in range(height)
    )
    out = b"\x89PNG\r\n\x1a\n" + png_chunk(b"IHDR", ihdr)
    for k, v in text.items():
        out += png_chunk(b"tEXt", k.encode() + b"\x00" + v.encode())
    out += png_chunk(b"IDAT", zlib.compress(raw, 9))
    out += png_chunk(b"IEND", b"")
    return out


# ─────────────────────────────────────────────────────────────────────
# TIFF: DNG
#
# The layout that makes the payload hash worth having: IFD0 is a
# reduced-resolution preview (NewSubfileType bit 0 set) that Lightroom
# rewrites on every edit, IFD1 is the full-resolution sensor data that
# never changes.

SENSOR_RAW = bytes(range(251)) * 6
PREVIEW_V1 = b"preview-rendered-at-develop-time-v1" + bytes(range(64))
PREVIEW_V2 = b"preview-rendered-at-develop-time-v2-and-longer" + bytes(range(96))


# The preview and the sensor image are deliberately *different* sizes,
# because the bug this guards against is reporting the preview's
# dimensions as the photograph's. `DefaultCropSize` is a few pixels
# smaller than the readout, as it is on a real RAW file.
PREVIEW_SIZE = (64, 48)
SENSOR_SIZE = (320, 240)
SENSOR_CROP = (316, 236)


def dng_file(preview: bytes, description: str) -> bytes:
    def entries(
        subfile_type: int,
        desc: str,
        strip_at: int,
        strip_len: int,
        size: tuple[int, int],
        crop: tuple[int, int] | None = None,
    ) -> list[Entry]:
        e: list[Entry] = [
            (0x00FE, LONG, [subfile_type]),  # NewSubfileType
            (0x0100, LONG, [size[0]]),  # ImageWidth
            (0x0101, LONG, [size[1]]),  # ImageLength
            (0x0111, LONG, [strip_at]),  # StripOffsets
            (0x0117, LONG, [strip_len]),  # StripByteCounts
        ]
        if crop:
            e.append((0xC620, LONG, list(crop)))  # DefaultCropSize
        if desc:
            e.append((0x010E, ASCII, desc))
        return e

    # Sizes first (they do not depend on the offsets), then offsets.
    e0 = entries(1, description, 0, len(preview), PREVIEW_SIZE)
    e1 = entries(0, "", 0, len(SENSOR_RAW), SENSOR_SIZE, SENSOR_CROP)
    ifd0_at = 8
    ifd1_at = ifd0_at + ifd_size(e0)
    data_at = ifd1_at + ifd_size(e1)
    # Out-of-line values come first, then the two image blocks.
    _, d0_probe = pack_ifd(e0, data_at)
    _, d1_probe = pack_ifd(e1, data_at + len(d0_probe))
    preview_at = data_at + len(d0_probe) + len(d1_probe)
    raw_at = preview_at + len(preview)

    e0 = entries(1, description, preview_at, len(preview), PREVIEW_SIZE)
    e1 = entries(0, "", raw_at, len(SENSOR_RAW), SENSOR_SIZE, SENSOR_CROP)
    ifd0, d0 = pack_ifd(e0, data_at, next_ifd=ifd1_at)
    ifd1, d1 = pack_ifd(e1, data_at + len(d0))
    assert data_at + len(d0) + len(d1) == preview_at, "DNG layout drifted"
    return (
        b"II\x2a\x00"
        + struct.pack("<I", ifd0_at)
        + ifd0
        + ifd1
        + d0
        + d1
        + preview
        + SENSOR_RAW
    )


# ─────────────────────────────────────────────────────────────────────
# ISO base media: MP4


def box(btype: bytes, body: bytes) -> bytes:
    return struct.pack(">I", len(body) + 8) + btype + body


def full_box(btype: bytes, version: int, body: bytes) -> bytes:
    return box(btype, bytes([version, 0, 0, 0]) + body)


CLIP_VIDEO = bytes(range(256)) * 3
CLIP_AUDIO = bytes(range(128, 256)) * 2


def ilst(tags: dict[bytes, str]) -> bytes:
    """An iTunes-style metadata block: `meta` > `hdlr` + `ilst`.

    This is where MP4 tags actually live, and it is a different place
    from the bare `\xa9day` / `\xa9xyz` atoms QuickTime writes straight
    into `udta`. A real phone video has both, which is why the provider
    reads both.
    """
    items = b""
    for key, value in tags.items():
        # `data` box: a 4-byte type indicator (1 = UTF-8), 4 bytes of
        # locale, then the text.
        payload = struct.pack(">II", 1, 0) + value.encode()
        items += box(key, box(b"data", payload))
    hdlr = full_box(b"hdlr", 0, b"\x00" * 4 + b"mdir" + b"appl" + b"\x00" * 9)
    return full_box(b"meta", 0, hdlr + box(b"ilst", items))


def qt_atom(name: bytes, text: str) -> bytes:
    """A bare QuickTime `udta` atom: a 16-bit length and language ahead
    of the text, with no `data` box around it."""
    return box(name, struct.pack(">HH", len(text), 0x55C4) + text.encode())


def mp4_file(
    tags: dict[bytes, str],
    when: str,
    *,
    brand: bytes = b"isom",
    tracks: tuple[tuple[int, bytes, bytes, int, int], ...] = (),
    location: str | None = "+37.7749-122.4194+010.000/",
) -> bytes:
    """A BMFF file. `tracks` is `(id, sample fourcc, samples, w, h)`.

    Defaults to the two-track clip; pass a single `mp4a` track for an
    audio file. The tracks are separate payload-hash groups, so a change
    to one leaves the other's digest alone.
    """
    if not tracks:
        tracks = (
            (1, b"avc1", CLIP_VIDEO, 320, 240),
            (2, b"mp4a", CLIP_AUDIO, 0, 0),
        )

    def sample_entry(fourcc: bytes, w: int, h: int) -> bytes:
        """A real `stsd` sample entry.

        Our own parser reads only the four-CC, so a stub would do for
        the payload hash — but `lofty` parses the whole
        `AudioSampleEntry` to report channels and sample rate, and an
        8-byte stub makes it give up on the file with a bare "failed to
        parse Mp4 file". A fixture the library under test cannot read is
        not a fixture.
        """
        common = b"\x00" * 6 + struct.pack(">H", 1)  # reserved + data ref index
        if w and h:
            # VisualSampleEntry: 78 bytes.
            return box(
                fourcc,
                common
                + struct.pack(">HHIII", 0, 0, 0, 0, 0)  # pre_defined + reserved
                + struct.pack(">HH", w, h)
                + struct.pack(">II", 0x00480000, 0x00480000)  # 72 dpi
                + struct.pack(">I", 0)
                + struct.pack(">H", 1)  # frame count
                + b"\x00" * 32  # compressor name
                + struct.pack(">Hh", 0x0018, -1),  # depth, pre_defined
            )
        # AudioSampleEntry: 28 bytes. The sample rate is 16.16 fixed
        # point, which is the field everyone gets wrong.
        return box(
            fourcc,
            common
            + struct.pack(">HHI", 0, 0, 0)  # version, revision, vendor
            + struct.pack(">HH", 2, 16)  # channels, sample size
            + struct.pack(">HH", 0, 0)  # pre_defined, reserved
            + struct.pack(">I", 44100 << 16),
        )

    def stbl(fourcc: bytes, offset: int, size: int, w: int, h: int) -> bytes:
        stsd = full_box(
            b"stsd",
            0,
            struct.pack(">I", 1) + sample_entry(fourcc, w, h),
        )
        stsz = full_box(b"stsz", 0, struct.pack(">II", size, 1))
        stsc = full_box(b"stsc", 0, struct.pack(">IIII", 1, 1, 1, 1))
        stco = full_box(b"stco", 0, struct.pack(">II", 1, offset))
        return box(b"stbl", stsd + stsz + stsc + stco)

    def trak(
        track_id: int, fourcc: bytes, offset: int, size: int, w: int, h: int
    ) -> bytes:
        tkhd_body = (
            struct.pack(">II", 0, 0)  # creation, modification
            + struct.pack(">I", track_id)
            + b"\x00" * 4  # reserved
            + struct.pack(">I", 900)  # duration
            + b"\x00" * 8  # reserved
            + struct.pack(">hhhh", 0, 0, 0, 0)  # layer, group, volume, pad
            + b"\x00" * 36  # transform matrix
            + struct.pack(">II", w << 16, h << 16)
        )
        mdhd = full_box(b"mdhd", 0, struct.pack(">IIIIHH", 0, 0, 600, 900, 0x55C4, 0))
        # `mdia > hdlr` is how a reader tells the sound track from the
        # picture track. Our own parser does not need it — it groups by
        # `tkhd` track id — but `lofty` finds the audio track through
        # this atom and nothing else, so a file without one is simply
        # "failed to parse Mp4 file" with no further explanation.
        handler = b"soun" if not (w and h) else b"vide"
        hdlr = full_box(
            b"hdlr",
            0,
            b"\x00" * 4 + handler + b"\x00" * 12 + b"\x00",
        )
        minf = box(b"minf", stbl(fourcc, offset, size, w, h))
        return box(
            b"trak",
            full_box(b"tkhd", 0, tkhd_body) + box(b"mdia", mdhd + hdlr + minf),
        )

    ftyp = box(b"ftyp", brand + b"\x00\x00\x02\x00" + brand + b"iso2avc1mp41")
    udta_body = ilst(tags) + qt_atom(b"\xa9day", when)
    if location:
        udta_body += qt_atom(b"\xa9xyz", location)
    udta = box(b"udta", udta_body)
    # `mvhd` creation time is seconds since 1904. Version 0 stores it in
    # 32 bits, which runs out in 2040 — so a 24th-century timestamp
    # needs version 1, whose creation/modification/duration fields are
    # 64-bit. 14_520_000_000 lands in 2364.
    mvhd = full_box(
        b"mvhd",
        1,
        struct.pack(">QQIQ", 14_520_000_000, 14_520_000_000, 600, 900) + b"\x00" * 80,
    )

    def build(first_at: int) -> bytes:
        body = mvhd
        off = first_at
        for tid, fourcc, data, w, h in tracks:
            body += trak(tid, fourcc, off, len(data), w, h)
            off += len(data)
        return box(b"moov", body + udta)

    probe = build(0)
    first_at = len(ftyp) + len(probe) + 8
    payload = b"".join(data for _, _, data, _, _ in tracks)
    return ftyp + build(first_at) + box(b"mdat", payload)


# ─────────────────────────────────────────────────────────────────────


def write(rel: str, data: bytes) -> None:
    p = OUT / rel
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_bytes(data)
    print(f"  {rel:<44} {len(data):>7} bytes")


def main() -> None:
    if OUT.exists():
        for old in sorted(OUT.rglob("*")):
            if old.is_file():
                old.unlink()
    OUT.mkdir(parents=True, exist_ok=True)
    print(f"writing {OUT}")

    # ── audio ────────────────────────────────────────────────────────
    ode_tags = {
        "TIT2": "Ode to Spot",
        "TPE1": "Data",
        "TALB": "Bridge Recitals",
        "TPE2": "USS Enterprise Ensemble",
        "TCOM": "Data",
        "TCON": "Spoken Word",
        "TRCK": "3/9",
        "TPOS": "1/2",
        "TDRC": "2364-04-13",
    }
    write("music/ode_to_spot.mp3", mp3_file(ode_tags))
    # The payload-hash pair: same audio, retagged and given an ID3v1
    # trailer, exactly what a tagging pass does. `blake3` must differ
    # and `payload_blake3` must not.
    write(
        "music/ode_to_spot_retagged.mp3",
        mp3_file(
            {**ode_tags, "TCON": "Poetry", "TIT2": "Ode to Spot (Remastered)"}, v1=True
        ),
    )
    # A byte-identical copy in another folder: two paths, one item.
    write("archive/ode_to_spot_copy.mp3", mp3_file(ode_tags))
    # No tags at all, and no Xing frame — the bare case.
    write("music/untagged_hum.mp3", mp3_file({}, xing=False))

    write(
        "music/warp_core_hum.flac",
        flac_file(
            {
                "TITLE": "Warp Core Hum",
                "ARTIST": "Geordi La Forge",
                "ALBUM": "Engineering Ambience",
                "ALBUMARTIST": "Engineering",
                "DATE": "2364",
                "TRACKNUMBER": "1",
                "GENRE": "Ambient",
            }
        ),
    )
    # Same audio, plus cover art and a changed tag.
    write(
        "music/warp_core_hum_with_art.flac",
        flac_file(
            {
                "TITLE": "Warp Core Hum",
                "ARTIST": "Geordi La Forge",
                "ALBUM": "Engineering Ambience",
                "ALBUMARTIST": "Engineering",
                "DATE": "2364-05-01",
                "TRACKNUMBER": "1",
                "GENRE": "Ambient",
            },
            art=b"\x00\x00\x00\x03image/png\x00\x00\x00\x00" + b"\xcc" * 512,
        ),
    )

    write(
        "music/tea_earl_grey.wav",
        wav_file({b"INAM": "Tea, Earl Grey, Hot", b"IART": "Replicator"}),
    )
    write("music/tea_earl_grey_untagged.wav", wav_file({}))

    # ── stills ───────────────────────────────────────────────────────
    bridge_exif = exif_block(
        make="Starfleet Optical",
        model="Tricorder Mk VII",
        lens="Standard 50mm",
        description="Main bridge, alpha shift",
        when="2364:04:13 08:45:00",
        offset="-07:00",
        iso=400,
        width=320,
        height=480,
        gps=(37.7749, -122.4194, 10.0),
    )
    write("photos/bridge.jpg", jpeg_file(bridge_exif))
    # The payload-hash pair: identical scan, re-tagged.
    recaptioned = exif_block(
        make="Starfleet Optical",
        model="Tricorder Mk VII",
        lens="Standard 50mm",
        description="Main bridge, alpha shift, red alert",
        when="2364:04:13 08:45:00",
        offset="-07:00",
        iso=400,
        width=320,
        height=480,
        gps=(37.7749, -122.4194, 10.0),
    )
    write(
        "photos/bridge_recaptioned.jpg",
        jpeg_file(recaptioned, comment="Reviewed by Cmdr Riker"),
    )
    # No EXIF at all: dimensions must still come from the SOF marker.
    write("photos/no_exif.jpg", jpeg_file(b""))

    write(
        "photos/holodeck.png",
        png_file(text={"Author": "Reginald Barclay", "Description": "Program 9"}),
    )
    write("photos/holodeck_untagged.png", png_file(text={}))

    write("photos/sensor.dng", dng_file(PREVIEW_V1, "Nebula, exposure 1"))
    # The Lightroom case: the preview is re-rendered, the sensor data is
    # not. Same payload hash, different file hash.
    write(
        "photos/sensor_edited.dng", dng_file(PREVIEW_V2, "Nebula, exposure 1 (edited)")
    )

    # ── video ────────────────────────────────────────────────────────
    # A music video: `ilst` tags AND capture metadata in one file. An
    # item gets one class, so a reader chosen by class drops one half —
    # this is the fixture that catches that.
    write(
        "video/holodeck_clip.mp4",
        mp4_file(
            {
                b"\xa9nam": "Holodeck Program 9",
                b"\xa9ART": "Reginald Barclay",
                b"\xa9alb": "Holodeck Sessions",
            },
            "2364-04-13T08:45:00-0700",
        ),
    )

    # The mirror image: an audio file that also carries a capture date,
    # the way a voice memo or a phone-recorded track does.
    write(
        "music/bridge_recital.m4a",
        mp4_file(
            {
                b"\xa9nam": "Bridge Recital",
                b"\xa9ART": "Data",
                b"\xa9alb": "Bridge Recitals",
            },
            "2364-04-13T09:15:00-0700",
            brand=b"M4A ",
            tracks=((1, b"mp4a", CLIP_AUDIO, 0, 0),),
            location=None,
        ),
    )

    # ── playlists ────────────────────────────────────────────────────
    write(
        "playlists/bridge_ambience.m3u",
        (
            "#EXTM3U\n"
            "#PLAYLIST:Bridge Ambience\n"
            "#EXTINF:227,Data - Ode to Spot\n"
            "../music/ode_to_spot.mp3\n"
            "#EXTINF:-1,Geordi La Forge - Warp Core Hum\n"
            "../music/warp_core_hum.flac\n"
            "#EXTINF:12,A track that was deleted years ago\n"
            "../music/deleted_long_ago.mp3\n"
            "#EXTINF:0,A stream\n"
            "http://holodeck.local/stream.mp3\n"
            "/Volumes/Enterprise/tracks/tea_earl_grey.m4a\n"
            "..\\music\\tea_earl_grey.wav\n"
        ).encode(),
    )
    # Latin-1, as a desktop player would write it, and with the same
    # track twice — order is the content, so neither is dropped.
    write(
        "playlists/café_deck_ten.m3u",
        b"#EXTM3U\n#EXTINF:1,Caf\xe9 Ambience\n../music/tea_earl_grey.wav\n"
        b"../music/tea_earl_grey.wav\n",
    )
    # An HLS manifest wearing the playlist extension. Must be skipped.
    write(
        "playlists/stream.m3u8",
        (
            "#EXTM3U\n"
            "#EXT-X-VERSION:3\n"
            "#EXT-X-TARGETDURATION:10\n"
            "#EXTINF:9.009,\n"
            "segment0.ts\n"
            "#EXTINF:9.009,\n"
            "segment1.ts\n"
            "#EXT-X-ENDLIST\n"
        ).encode(),
    )

    # ── things that must not derail a scan ───────────────────────────
    # Truncated mid-file: recorded, with a NULL payload hash.
    write("music/corrupt.mp3", id3v2({"TIT2": "Truncated"}) + b"\x00" * 64)
    # A recognized extension whose bytes are nothing we parse.
    write("video/mystery.avi", b"NOT-A-RIFF-FILE-AT-ALL" + bytes(range(64)))
    # Not media. Must never appear in any table.
    write("readme.txt", b"Captain's log, supplemental.\n")

    print("done")


if __name__ == "__main__":
    main()
