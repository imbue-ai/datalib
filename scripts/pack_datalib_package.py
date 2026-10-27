#!/usr/bin/env python3
"""Pack a datalib package for publishing (docs/dev/packages.md).

    scripts/pack_datalib_package.py <package dir> <out dir>

<package dir> holds `datalib-package.toml` without an `[artifacts]` table,
and `build/<target>/` for each build: a target triple datalib ships for,
or `any`. <out dir> gets one `<name>-<version>-<target>.tar.gz` per build
and the manifest with `[artifacts]` naming each by its sha256. Upload
everything in <out dir> to one GitHub release, and `github:owner/repo`
installs it; or paste `file://<out dir>/datalib-package.toml` to try it.
"""

import hashlib
import io
import re
import sys
import tarfile
import tomllib
from pathlib import Path

TARGETS = {
    "any",
    "aarch64-apple-darwin",
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
}


def pack(build: Path) -> bytes:
    """A gzipped tar of `build`'s contents, the same bytes for the same files."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w:gz", format=tarfile.PAX_FORMAT) as tar:
        for path in sorted(build.rglob("*")):
            info = tar.gettarinfo(str(path), arcname=str(path.relative_to(build)))
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            info.mtime = 0
            if path.is_file():
                with path.open("rb") as f:
                    tar.addfile(info, f)
            else:
                tar.addfile(info)
    return buf.getvalue()


def main(src: Path, out: Path) -> None:
    text = (src / "datalib-package.toml").read_text()
    manifest = tomllib.loads(text)
    if "artifacts" in manifest:
        sys.exit("datalib-package.toml already has [artifacts]; this script writes it")
    slug = re.sub(r"[^a-z0-9]+", "-", manifest["name"].lower()).strip("-")
    out.mkdir(parents=True, exist_ok=True)
    lines = ["", "[artifacts]"]
    for build in sorted((src / "build").iterdir()):
        if build.name not in TARGETS:
            sys.exit(
                f"build/{build.name}: not a target datalib ships for ({', '.join(sorted(TARGETS))})"
            )
        data = pack(build)
        name = f"{slug}-{manifest['version']}-{build.name}.tar.gz"
        (out / name).write_bytes(data)
        sha = hashlib.sha256(data).hexdigest()
        lines.append(
            f'{build.name} = {{ url = "{name}", sha256 = "{sha}", size = {len(data)} }}'
        )
    # `[artifacts]` goes above the first array table, where it still
    # belongs to the top level rather than to the last `[[sources]]`.
    first = text.find("\n[[")
    head, tail = (text, "") if first < 0 else (text[: first + 1], text[first + 1 :])
    (out / "datalib-package.toml").write_text(
        head.rstrip("\n") + "\n" + "\n".join(lines) + "\n\n" + tail
    )
    print(out / "datalib-package.toml")


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    main(Path(sys.argv[1]), Path(sys.argv[2]))
