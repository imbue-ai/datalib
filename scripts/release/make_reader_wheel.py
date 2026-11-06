#!/usr/bin/env python3
"""Writes one imbue-datalib-reader wheel from a built libdoltlite.

A wheel is a zip with a fixed layout, so the standard library writes it and no
build backend runs where a release is assembled. The same inputs give the same
bytes. docs/dev/release_steps.md §"The reader wheels" says where this runs.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import stat
import zipfile
from pathlib import Path

DISTRIBUTION = "imbue-datalib-reader"
IMPORT_NAME = "datalib_reader"
ZIP_EPOCH = (1980, 1, 1, 0, 0, 0)


def platform_tags(libc: str, arch: str) -> list[str]:
    """PEP 600's name for the glibc floor, then the older name of the same floor
    for a pip from before PEP 600."""
    if libc == "manylinux":
        return [f"manylinux_2_17_{arch}", f"manylinux2014_{arch}"]
    if libc == "musllinux":
        return [f"musllinux_1_2_{arch}"]
    raise ValueError(f"unknown libc {libc!r}")


def wheel_name(version: str, libc: str, arch: str) -> str:
    tags = ".".join(platform_tags(libc, arch))
    return f"{DISTRIBUTION.replace('-', '_')}-{version}-py3-none-{tags}.whl"


def metadata(version: str, readme: str, license_names: list[str]) -> str:
    headers = [
        "Metadata-Version: 2.4",
        f"Name: {DISTRIBUTION}",
        f"Version: {version}",
        "Summary: Read a datalib library's doltlite stores from any Python",
        "Author: Imbue",
        "License-Expression: MIT AND Apache-2.0 AND blessing",
        *[f"License-File: {name}" for name in license_names],
        "Project-URL: Source, https://github.com/imbue-ai/datalib",
        "Requires-Python: >=3.9",
        "Description-Content-Type: text/markdown",
    ]
    return "\n".join(headers) + "\n\n" + readme


def wheel_file(libc: str, arch: str) -> str:
    tags = "".join(f"Tag: py3-none-{tag}\n" for tag in platform_tags(libc, arch))
    return (
        "Wheel-Version: 1.0\n"
        "Generator: datalib make_reader_wheel.py\n"
        "Root-Is-Purelib: false\n"
    ) + tags


def record(files: dict[str, bytes], record_name: str) -> bytes:
    lines = []
    for name, content in files.items():
        digest = base64.urlsafe_b64encode(hashlib.sha256(content).digest())
        lines.append(f"{name},sha256={digest.decode().rstrip('=')},{len(content)}\n")
    lines.append(f"{record_name},,\n")
    return "".join(lines).encode()


def wheel_contents(
    version: str,
    libc: str,
    arch: str,
    module: bytes,
    library_name: str,
    library: bytes,
    readme: str,
    licenses: dict[str, bytes],
) -> dict[str, bytes]:
    """What the zip holds, in order: the package, then its `.dist-info`, whose
    RECORD comes last and lists everything before it."""
    dist_info = f"{DISTRIBUTION.replace('-', '_')}-{version}.dist-info"
    files = {
        f"{IMPORT_NAME}/__init__.py": module,
        f"{IMPORT_NAME}/{library_name}": library,
    }
    for name, content in licenses.items():
        files[f"{dist_info}/licenses/{name}"] = content
    files[f"{dist_info}/METADATA"] = metadata(version, readme, list(licenses)).encode()
    files[f"{dist_info}/WHEEL"] = wheel_file(libc, arch).encode()
    record_name = f"{dist_info}/RECORD"
    files[record_name] = record(files, record_name)
    return files


def write_wheel(path: Path, files: dict[str, bytes]) -> None:
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED) as wheel:
        for name, content in files.items():
            entry = zipfile.ZipInfo(name, date_time=ZIP_EPOCH)
            entry.compress_type = zipfile.ZIP_DEFLATED
            mode = 0o755 if "/libdoltlite." in name else 0o644
            entry.external_attr = (stat.S_IFREG | mode) << 16
            wheel.writestr(entry, content)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--libc", required=True, choices=["manylinux", "musllinux"])
    parser.add_argument("--arch", required=True)
    parser.add_argument("--module", required=True, type=Path)
    parser.add_argument("--library", required=True, type=Path)
    parser.add_argument("--readme", required=True, type=Path)
    parser.add_argument(
        "--license",
        required=True,
        action="append",
        help="<name in the wheel>=<path>; say it once per file",
    )
    parser.add_argument("--out-dir", required=True, type=Path)
    args = parser.parse_args()

    licenses = {}
    for entry in args.license:
        name, _, path = entry.partition("=")
        licenses[name] = Path(path).read_bytes()
    files = wheel_contents(
        args.version,
        args.libc,
        args.arch,
        args.module.read_bytes(),
        args.library.name,
        args.library.read_bytes(),
        args.readme.read_text(),
        licenses,
    )
    path = args.out_dir / wheel_name(args.version, args.libc, args.arch)
    write_wheel(path, files)
    print(path)


if __name__ == "__main__":
    main()
