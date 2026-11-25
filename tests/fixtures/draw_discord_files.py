#!/usr/bin/env python3
"""Draw the pictures, clips and sounds the Discord fixture's messages
carry. Real, playable files, so that a page which shows or plays one can
be looked at; small, so that they can be checked in.

The pixels and samples are computed here; `ffmpeg` (with libx264,
libopus, libmp3lame and libwebp) encodes them. Two ffmpeg builds do not
write the same bytes, so this is run by hand, seldom, and its output is
checked in:

    uv run python tests/fixtures/draw_discord_files.py
    uv run python tests/fixtures/make_discord_fixtures.py   # it reads the sizes
"""

from __future__ import annotations

import math
import random
import struct
import subprocess
import tempfile
from collections.abc import Callable
from pathlib import Path

from make_discord_fixtures import FILES

Rgb = tuple[float, float, float]
Shader = Callable[[float, float], Rgb]

WIDTH, HEIGHT = 480, 300


def mix(a: Rgb, b: Rgb, t: float) -> Rgb:
    t = min(max(t, 0.0), 1.0)
    return (
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    )


def noise(x: float, y: float) -> float:
    """Smooth, repeatable, between -1 and 1."""
    return (
        math.sin(x * 1.7 + math.sin(y * 1.3) * 2.1)
        + 0.5 * math.sin(x * 3.9 - y * 4.3 + 1.7)
        + 0.25 * math.sin(y * 9.1 + math.sin(x * 7.7))
    ) / 1.75


def ppm(shader: Shader, width: int, height: int) -> bytes:
    """`shader` over x in -1.6..1.6 and y in -1..1, as a picture."""
    out = bytearray(f"P6 {width} {height} 255\n".encode())
    for row in range(height):
        for col in range(width):
            x = (2 * col / width - 1) * width / height
            y = 2 * row / height - 1
            out += bytes(int(min(max(c, 0.0), 1.0) * 255) for c in shader(x, y))
    return bytes(out)


STARS = [
    (random.Random(1701 + n).uniform(-1.6, 1.6), random.Random(n).uniform(-1, 1))
    for n in range(140)
]
LIT = {(round(x * 150), round(y * 150)) for x, y in STARS}


def space(x: float, y: float) -> Rgb:
    lit = (round(x * 150), round(y * 150)) in LIT
    return (0.95, 0.95, 1.0) if lit else (0.01, 0.01, 0.04)


def earth(x: float, y: float) -> Rgb:
    r = math.hypot(x, y - 1.9)
    if r > 1.6:
        glow = math.exp(-(r - 1.6) * 14)
        return mix(space(x, y), (0.35, 0.65, 1.0), glow * 0.9)
    land = noise(x * 2.2, (y - 1.9) * 3.1)
    clouds = noise(x * 5 + 9, y * 8)
    ground = mix((0.03, 0.16, 0.42), (0.16, 0.38, 0.16), (land - 0.1) * 6)
    return mix(ground, (0.95, 0.97, 1.0), (clouds - 0.2) * 1.4)


def spot(x: float, y: float) -> Rgb:
    """An orange tabby, looking at you."""
    for side in (-1, 1):
        ex, ey = x - side * 0.27, y + 0.05
        if (ex / 0.13) ** 2 + (ey / 0.1) ** 2 < 1:
            return (0.05, 0.05, 0.05) if abs(ex) < 0.028 else (0.55, 0.8, 0.25)
        tip = abs(x - side * 0.42) * 1.9 + (-0.35 - y)
        if -0.95 < y < -0.35 and abs(x - side * 0.42) < 0.28 * (y + 0.95) / 0.6:
            return (0.95, 0.6, 0.55) if tip < 0.22 else (0.82, 0.45, 0.14)
    if (x / 0.07) ** 2 + ((y - 0.2) / 0.05) ** 2 < 1:
        return (0.85, 0.45, 0.45)
    if abs(y - 0.27 - abs(x) * 0.12) < 0.008 and 0.08 < abs(x) < 0.75:
        return (0.98, 0.98, 0.95)
    if (x / 0.66) ** 2 + (y / 0.6) ** 2 < 1:
        stripe = math.sin(x * 22) > 0.55 and y < -0.12
        return (0.6, 0.28, 0.06) if stripe else (0.9, 0.55, 0.2)
    return mix((0.23, 0.25, 0.3), (0.12, 0.13, 0.17), (y + 1) / 2)


def poker(x: float, y: float) -> Rgb:
    """Five cards face up: a full house."""
    for n in range(5):
        cx, cy = x - (n - 2) * 0.55, y + abs(n - 2) * 0.04
        if abs(cx) < 0.24 and abs(cy) < 0.36:
            red = n % 2 == 0
            ink = (0.8, 0.1, 0.12) if red else (0.08, 0.08, 0.1)
            pips = 3 if n < 3 else 2
            for pip in range(pips):
                py = cy - (pip - (pips - 1) / 2) * 0.22
                diamond = abs(cx) * 1.3 + abs(py) < 0.09
                if diamond if red else math.hypot(cx, py) < 0.07:
                    return ink
            return (0.97, 0.96, 0.92)
    return mix((0.05, 0.3, 0.16), (0.02, 0.18, 0.1), math.hypot(x, y) / 1.8)


def cake(x: float, y: float) -> Rgb:
    if 0.1 < y < 0.75 and abs(x) < 0.8:
        wave = 0.28 + 0.05 * math.sin(x * 18)
        return (0.97, 0.95, 0.96) if y < wave else (0.45, 0.25, 0.14)
    for n in range(-2, 3):
        if abs(x - n * 0.3) < 0.025 and -0.2 < y <= 0.1:
            return (0.3, 0.55, 0.9)
        if math.hypot((x - n * 0.3) / 0.05, (y + 0.28) / 0.09) < 1:
            return (1.0, 0.8, 0.25)
    return (0.12, 0.1, 0.18)


def warp_core(x: float, y: float) -> Rgb:
    rings = 0.5 + 0.5 * math.cos(y * 26)
    core = math.exp(-((abs(x) / (0.16 + 0.05 * rings)) ** 2))
    if abs(x) < 0.5 and abs(y) < 0.05:
        return (0.75, 0.78, 0.85)
    frame = 0.16 if int((x + 9) * 5) % 2 else 0.12
    return mix(
        (frame, frame, frame + 0.04), (0.45, 0.8, 1.0), core * (0.6 + 0.4 * rings)
    )


def red_alert(frame: int) -> Shader:
    lit = 0.5 + 0.5 * math.cos(frame / 8 * 2 * math.pi)

    def shade(x: float, y: float) -> Rgb:
        if abs(y) < 0.62 and int((abs(y) + 0.06) * 8) % 2 == 0 and abs(x) < 1.3:
            return (0.35 + 0.6 * lit, 0.04, 0.05)
        return (0.05, 0.05, 0.06)

    return shade


def nebula(t: float) -> Shader:
    def shade(x: float, y: float) -> Rgb:
        n = noise(x * 1.5 + t, y * 1.5 - t * 0.4)
        base = mix((0.05, 0.02, 0.12), (0.75, 0.25, 0.55), n * 0.9)
        return mix(base, (0.3, 0.6, 0.95), noise(x * 2 - t, y * 2 + 4) * 0.7)

    return shade


def samples(seconds: float, at: Callable[[float], float]) -> bytes:
    rate = 48_000
    count = int(seconds * rate)
    return b"".join(
        struct.pack("<h", int(max(-1.0, min(1.0, at(n / rate))) * 32_000))
        for n in range(count)
    )


def voice(t: float) -> float:
    """Something that rises and falls as speech does."""
    pitch = 120 + 18 * math.sin(t * 5.1) + 9 * math.sin(t * 13)
    syllable = max(0.0, math.sin(t * 15) + 0.3 * math.sin(t * 6.3))
    tone = sum(math.sin(2 * math.pi * pitch * k * t) / k for k in range(1, 9))
    return 0.22 * syllable * tone


NOTES = [233.08, 261.63, 293.66, 311.13, 349.23, 392.0, 440.0, 466.16]


def trombone(t: float) -> float:
    """A B-flat scale, as practised."""
    n = min(int(t / 0.35), len(NOTES) - 1)
    into = t - n * 0.35
    slide = NOTES[n] * (1 - 0.06 * math.exp(-into * 30))
    tone = sum(math.sin(2 * math.pi * slide * k * t) / k for k in range(1, 7))
    return 0.25 * min(1.0, into * 25) * tone


def ffmpeg(*args: str) -> None:
    subprocess.run(
        ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", *args], check=True
    )


PCM = ("-f", "s16le", "-ar", "48000", "-ac", "1")
# No encoder's name or date in the file.
BARE = ("-map_metadata", "-1", "-fflags", "+bitexact", "-flags", "+bitexact")
# Few colours and no dither, which a drawing does not need and a PNG or a
# GIF cannot pack.
FLAT = (
    "-filter_complex",
    "split[a][b];[a]palettegen=max_colors=64[p];[b][p]paletteuse=dither=none",
)


def main() -> None:
    FILES.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)

        def still(name: str, shader: Shader, *args: str) -> None:
            src = work / "still.ppm"
            src.write_bytes(ppm(shader, WIDTH, HEIGHT))
            ffmpeg("-i", str(src), *BARE, *args, str(FILES / name))

        still("earth_from_orbit.jpg", earth, "-q:v", "6")
        still("spot.png", spot, *FLAT)
        still("full_house.png", poker, *FLAT)
        still("SPOILER_captains_cake.png", cake, *FLAT)
        still("warp_core.webp", warp_core, "-quality", "60")

        for n in range(8):
            (work / f"alert{n}.ppm").write_bytes(ppm(red_alert(n), 200, 120))
        ffmpeg(
            *("-framerate", "8", "-i", str(work / "alert%d.ppm")),
            *(*BARE, *FLAT),
            str(FILES / "red_alert.gif"),
        )
        for n in range(24):
            (work / f"nebula{n}.ppm").write_bytes(ppm(nebula(n / 12), 320, 200))
        (work / "hum.pcm").write_bytes(
            samples(2, lambda t: 0.2 * math.sin(2 * math.pi * 70 * t))
        )
        ffmpeg(
            *("-framerate", "12", "-i", str(work / "nebula%d.ppm")),
            *(*PCM, "-i", str(work / "hum.pcm")),
            *BARE,
            *("-c:v", "libx264", "-crf", "30", "-pix_fmt", "yuv420p"),
            *("-c:a", "aac", "-b:a", "32k", "-movflags", "+faststart"),
            str(FILES / "nebula_approach.mp4"),
        )
        (work / "voice.pcm").write_bytes(samples(4, voice))
        ffmpeg(
            *(*PCM, "-i", str(work / "voice.pcm")),
            *(*BARE, "-c:a", "libopus", "-b:a", "16k"),
            str(FILES / "voice-message.ogg"),
        )
        (work / "trombone.pcm").write_bytes(samples(2.8, trombone))
        ffmpeg(
            *(*PCM, "-i", str(work / "trombone.pcm")),
            *(*BARE, "-c:a", "libmp3lame", "-b:a", "48k"),
            str(FILES / "trombone_practice.mp3"),
        )
    for f in sorted(FILES.iterdir()):
        print(f"{f.stat().st_size:>8}  {f.name}")


if __name__ == "__main__":
    main()
