"""Download one URL to one path and refuse it unless its sha256 matches.

The build action behind `//third-party/qmd_models`: a pinned download
that runs as an ordinary, remote-cached action rather than at repository
fetch time, so a run that needs none of the bytes never moves them.
Standard library only, like every script the build runs.
"""

from __future__ import annotations

import hashlib
import os
import sys
import time
import urllib.error
import urllib.request

# HuggingFace's CDN has answered a cold fetch with 429 before and failed a
# merge-gating build; a cache miss here reaches it directly, so back off
# and try again rather than give up on the first refusal.
RETRY_STATUSES = {429, 500, 502, 503, 504}
ATTEMPTS = 6
CHUNK = 1 << 20


def fetch(url: str, out: str) -> str:
    digest = hashlib.sha256()
    tmp = out + ".partial"
    with urllib.request.urlopen(url, timeout=120) as resp, open(tmp, "wb") as f:
        while chunk := resp.read(CHUNK):
            digest.update(chunk)
            f.write(chunk)
    os.replace(tmp, out)
    return digest.hexdigest()


def main(argv: list[str]) -> int:
    if len(argv) != 4:
        print(f"usage: {argv[0]} <url> <sha256> <out>", file=sys.stderr)
        return 2
    url, want, out = argv[1], argv[2].lower(), argv[3]
    delay = 2.0
    for attempt in range(1, ATTEMPTS + 1):
        try:
            got = fetch(url, out)
        except urllib.error.HTTPError as e:
            if e.code in RETRY_STATUSES and attempt < ATTEMPTS:
                wait = float(e.headers.get("Retry-After") or delay)
                print(
                    f"{url}: HTTP {e.code}, retrying in {wait:.0f}s ({attempt}/{ATTEMPTS})",
                    file=sys.stderr,
                )
                time.sleep(wait)
                delay *= 2
                continue
            print(f"{url}: HTTP {e.code}", file=sys.stderr)
            return 1
        except (urllib.error.URLError, TimeoutError, OSError) as e:
            if attempt < ATTEMPTS:
                print(
                    f"{url}: {e}, retrying in {delay:.0f}s ({attempt}/{ATTEMPTS})",
                    file=sys.stderr,
                )
                time.sleep(delay)
                delay *= 2
                continue
            print(f"{url}: {e}", file=sys.stderr)
            return 1
        if got != want:
            os.remove(out)
            print(f"{url}: sha256 {got}, expected {want}", file=sys.stderr)
            return 1
        return 0
    return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
