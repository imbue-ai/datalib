#!/usr/bin/env python3
"""A minimal curl-CLI-compatible wrapper around `curl_cffi`.

Latchkey's `LATCHKEY_CURL` env var lets us point latchkey at a custom curl
binary. We want Cloudflare-protected Notion endpoints to go out with a
Chrome TLS fingerprint, but there is no curl-impersonate CLI installed on
this machine — only `curl_cffi`, the Python library, which bundles
libcurl-impersonate as a shared library.

This shim is the bridge: it parses just enough of curl's CLI surface to
cover the requests latchkey emits + the requests our own downloader emits,
runs them via `curl_cffi.requests` with `impersonate="chrome"`, and behaves
like real curl on the outside (writes the body to `-o`, prints `-w` format
to stdout, exits non-zero on failure).

Supported flags (everything our caller chain uses today):

    -X / --request          method
    -H / --header           "Name: value" (repeatable)
    -d / --data             body
    --data-raw / --data-binary
    -o / --output           write body here
    -D / --dump-header      write response headers here ("-" = stdout)
    -w / --write-out        only "%{http_code}" is interpreted
    -s / --silent           accepted, no-op
    -S / --show-error       accepted, no-op
    -L / --location         accepted, no-op (curl_cffi follows by default off;
                            see allow_redirects below)
    --compressed            accepted, no-op
    -v / --verbose          accepted, mostly no-op (logs to stderr)
    -sS / -sSL / etc.       combined short flags

Anything else fails fast with a clear error so we notice if latchkey
starts using a new flag.
"""

from __future__ import annotations

import os
import shlex
import sys
import tempfile
from pathlib import Path
from typing import Iterable

from curl_cffi import requests as curl_requests

IMPERSONATE = "chrome"


# ---------------------------------------------------------------------------
# Caller-side helper: build a subprocess env that wires `LATCHKEY_CURL` to
# this shim, so `latchkey curl ...` invocations get a Chrome TLS fingerprint.
# Used by every downloader that needs Cloudflare-protected hosts (notion,
# chatgpt, claude). The shim runs under the *current* Python interpreter so
# it sees the same `curl_cffi` install as the caller (a uv venv, typically).
# ---------------------------------------------------------------------------


_SHIM_PATH = Path(__file__).resolve()
_WRAPPER_PATH = Path(tempfile.gettempdir()) / "latchkey_curl_shim.sh"


def _ensure_shim_wrapper() -> Path:
    """Write a small executable shell wrapper that invokes this shim with the
    current interpreter. We do this because `LATCHKEY_CURL` is treated as a
    single binary path by latchkey — we can't pass "python shim.py" as a
    space-separated string."""
    contents = (
        f"#!/bin/sh\nexec {shlex.quote(sys.executable)} "
        f'{shlex.quote(str(_SHIM_PATH))} "$@"\n'
    )
    if not _WRAPPER_PATH.exists() or _WRAPPER_PATH.read_text() != contents:
        _WRAPPER_PATH.write_text(contents)
        _WRAPPER_PATH.chmod(0o755)
    return _WRAPPER_PATH


def latchkey_env() -> dict[str, str]:
    """Return a `subprocess.run(env=...)` value that points `LATCHKEY_CURL`
    at this shim. Idempotent if the caller has already exported their own
    `LATCHKEY_CURL` (e.g. a real curl-impersonate-chrome binary), we leave
    it alone."""
    env = os.environ.copy()
    env.setdefault("LATCHKEY_CURL", str(_ensure_shim_wrapper()))
    return env


class ShimError(SystemExit):
    pass


def _explode_short_flags(tok: str) -> list[str]:
    """`-sSL` → `["-s", "-S", "-L"]`. Only safe for value-less flags."""
    return [f"-{c}" for c in tok[1:]]


_VALUELESS_SHORT = set("sSLv")
_VALUE_SHORT = set("XHdoOwD")


def _split_combined(tok: str) -> list[str]:
    """Split `-sSL` style. If the bundle contains a value-taking short
    (e.g. `-sH`), the value short must be last; everything before it is
    valueless, and the last char keeps its value via the *next* argv slot.

    Returns a list of single-flag tokens; the caller continues parsing.
    """
    if len(tok) <= 2 or not tok.startswith("-") or tok.startswith("--"):
        return [tok]
    chars = tok[1:]
    out: list[str] = []
    for i, c in enumerate(chars):
        if c in _VALUE_SHORT:
            if i != len(chars) - 1:
                raise ShimError(
                    f"latchkey_curl_shim: combined short flag bundle {tok!r} has a "
                    f"value-taking option {c!r} before the end; refusing to guess."
                )
            out.append(f"-{c}")
            return out
        if c not in _VALUELESS_SHORT:
            raise ShimError(
                f"latchkey_curl_shim: unsupported short flag {c!r} in bundle {tok!r}"
            )
        out.append(f"-{c}")
    return out


def parse_args(argv: list[str]) -> dict:
    method = "GET"
    headers: list[tuple[str, str]] = []
    data: str | None = None
    out_path: str | None = None
    dump_header_path: str | None = None
    write_out: str | None = None
    url: str | None = None
    follow_redirects = False

    # Pre-expand combined short flags.
    expanded: list[str] = []
    for tok in argv:
        if tok.startswith("-") and not tok.startswith("--") and len(tok) > 2:
            expanded.extend(_split_combined(tok))
        else:
            expanded.append(tok)

    it = iter(expanded)

    def need(flag: str) -> str:
        try:
            return next(it)
        except StopIteration:
            raise ShimError(f"latchkey_curl_shim: {flag} requires a value")

    for tok in it:
        if tok in ("-X", "--request"):
            method = need(tok).upper()
        elif tok in ("-H", "--header"):
            raw = need(tok)
            if ":" not in raw:
                raise ShimError(f"latchkey_curl_shim: malformed header {raw!r}")
            name, value = raw.split(":", 1)
            headers.append((name.strip(), value.strip()))
        elif tok in ("-d", "--data", "--data-raw", "--data-binary"):
            data = need(tok)
            if method == "GET":
                method = "POST"
        elif tok in ("-o", "--output"):
            out_path = need(tok)
        elif tok in ("-D", "--dump-header"):
            dump_header_path = need(tok)
        elif tok in ("-w", "--write-out"):
            write_out = need(tok)
        elif tok in ("-s", "--silent", "-S", "--show-error", "--compressed"):
            pass
        elif tok in ("-L", "--location"):
            follow_redirects = True
        elif tok in ("-v", "--verbose"):
            pass
        elif tok.startswith("-"):
            raise ShimError(f"latchkey_curl_shim: unsupported flag {tok!r}")
        else:
            if url is not None:
                raise ShimError(
                    f"latchkey_curl_shim: multiple URLs supplied: {url!r}, {tok!r}"
                )
            url = tok

    if url is None:
        raise ShimError("latchkey_curl_shim: no URL provided")

    return {
        "method": method,
        "headers": headers,
        "data": data,
        "out_path": out_path,
        "dump_header_path": dump_header_path,
        "write_out": write_out,
        "url": url,
        "follow_redirects": follow_redirects,
    }


def _merge_headers(pairs: Iterable[tuple[str, str]]) -> dict[str, str]:
    """Last-write-wins on duplicate header names, matching curl semantics
    closely enough for our needs (Notion never sends duplicate names)."""
    out: dict[str, str] = {}
    for k, v in pairs:
        out[k] = v
    return out


def main(argv: list[str] | None = None) -> int:
    args = parse_args(list(argv if argv is not None else sys.argv[1:]))
    headers = _merge_headers(args["headers"])
    r = curl_requests.request(
        args["method"],
        args["url"],
        headers=headers,
        data=args["data"],
        impersonate=IMPERSONATE,
        allow_redirects=args["follow_redirects"],
    )
    if args["dump_header_path"] is not None:
        # curl writes a status line + each header `Name: value`, CRLF
        # separated, terminated by a blank line. Reproduce that shape.
        reason = getattr(r, "reason", "") or ""
        lines = [f"HTTP/1.1 {r.status_code} {reason}".rstrip()]
        for name, value in r.headers.items():
            lines.append(f"{name}: {value}")
        dump = "\r\n".join(lines) + "\r\n\r\n"
        if args["dump_header_path"] == "-":
            sys.stdout.write(dump)
        else:
            with open(args["dump_header_path"], "w") as f:
                f.write(dump)

    body_bytes = r.content
    if args["out_path"] is not None:
        with open(args["out_path"], "wb") as f:
            f.write(body_bytes)
    else:
        sys.stdout.buffer.write(body_bytes)

    if args["write_out"] is not None:
        # Only `%{http_code}` is interpreted; everything else passes through.
        rendered = args["write_out"].replace("%{http_code}", str(r.status_code))
        sys.stdout.write(rendered)

    # Real curl exits 0 on any HTTP status; non-zero only on transport errors.
    # curl_cffi raises on transport failure, which becomes a Python traceback
    # → non-zero exit, which matches.
    return 0


if __name__ == "__main__":
    sys.exit(main())
