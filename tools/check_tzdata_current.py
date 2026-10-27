#!/usr/bin/env python3
"""Fail when the tz database our binaries bundle is older than IANA's
latest release.

Every named zone is read from the copy `jiff-tzdb` embeds
(`datalib/backend/time/src/zone.rs`). Nothing bumps
`datalib/backend/Cargo.lock` on a schedule (`.github/dependabot.yml` says
why), so this is what notices a release such as British Columbia's
permanent -07:00. Run weekly by `.github/workflows/tzdata.yml`; needs the
network.

    tools/check_tzdata_current.py
"""

from __future__ import annotations

import io
import json
import re
import sys
import tarfile
import urllib.request
from pathlib import Path

_LOCK = Path(__file__).resolve().parent.parent / "datalib/backend/Cargo.lock"
_IANA_VERSION = "https://data.iana.org/time-zones/tzdb/version"
_CRATE = "https://static.crates.io/crates/jiff-tzdb/jiff-tzdb-{v}.crate"
_CRATE_INFO = "https://crates.io/api/v1/crates/jiff-tzdb"


def release_key(release: str) -> tuple[int, int, str]:
    """`2026e` sorts before `2027a`; a release past `z` is `za`."""
    m = re.fullmatch(r"(\d{4})([a-z]+)", release)
    if not m:
        raise ValueError(f"not a tz release name: {release!r}")
    return int(m[1]), len(m[2]), m[2]


def locked_crate_version(lock: str) -> str:
    m = re.search(r'name = "jiff-tzdb"\nversion = "([^"]+)"', lock)
    if not m:
        raise ValueError("jiff-tzdb is not in Cargo.lock")
    return m[1]


def bundled_release(crate_tarball: bytes) -> str:
    with tarfile.open(fileobj=io.BytesIO(crate_tarball), mode="r:gz") as tar:
        for member in tar.getmembers():
            if member.name.endswith("/tzname.rs"):
                f = tar.extractfile(member)
                assert f is not None
                m = re.search(
                    r'VERSION: Option<&str> = Some\(r"([^"]+)"\)', f.read().decode()
                )
                if m:
                    return m[1]
    raise ValueError("no VERSION in the crate's tzname.rs")


def _get(url: str) -> bytes:
    req = urllib.request.Request(url, headers={"User-Agent": "datalib tzdata check"})
    with urllib.request.urlopen(req, timeout=60) as r:
        return r.read()


def main() -> int:
    ours_crate = locked_crate_version(_LOCK.read_text())
    ours = bundled_release(_get(_CRATE.format(v=ours_crate)))
    latest = _get(_IANA_VERSION).decode().strip()
    print(f"bundled: tzdata {ours} (jiff-tzdb {ours_crate}); IANA latest: {latest}")
    if release_key(ours) >= release_key(latest):
        return 0
    newest_crate = json.loads(_get(_CRATE_INFO))["crate"]["max_version"]
    newest = bundled_release(_get(_CRATE.format(v=newest_crate)))
    if release_key(newest) > release_key(ours):
        print(
            f"jiff-tzdb {newest_crate} has tzdata {newest}: in datalib/backend run "
            "`cargo update -p jiff-tzdb`, build, and open a PR."
        )
    else:
        print(
            f"no jiff-tzdb release carries {latest} yet (newest is {newest_crate}, {newest})."
        )
    return 1


if __name__ == "__main__":
    sys.exit(main())
