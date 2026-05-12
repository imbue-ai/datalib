#!/usr/bin/env python3
"""Incrementally fetch chatgpt.com web conversations and cache them as JSON.

This is the OpenAI/ChatGPT counterpart to scripts/sync_claude_web.py. For now
it only fetches and caches; ingest into Dolt under an `openai_*` schema is a
separate (forthcoming) step.

Auth + transport: every request shells out to `latchkey curl`, which
injects the registered `chatgpt` service cookies/headers. To clear
Cloudflare's managed challenges we point `LATCHKEY_CURL` at
`latchkey_curl_shim.py` (a `curl_cffi` wrapper with a Chrome TLS
fingerprint) — see `download.latchkey_curl_shim`.

Usage:
    uv run python scripts/sync_chatgpt_web.py fetch [options]

Output layout (default ~/backups/chatgpt_api/):
    me.json                       # current user profile
    conversations.json            # combined paginated listing index
    conversations/<id>.json       # full conversation tree (mapping/DAG)
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import time
from datetime import datetime
from pathlib import Path
from typing import Any

import typer
from tqdm import tqdm

from download.latchkey_curl_shim import latchkey_env as _latchkey_env

DEFAULT_OUT_DIR = Path.home() / "backups" / "chatgpt_api"
# Inter-fetch sleep. We don't have evidence ChatGPT throttles us at any
# polite rate; 0.1s is enough to stop us from looking like a tight loop
# while not doubling per-conv latency on top of ~0.4s GETs.
SLEEP_BETWEEN = 0.1
PAGE_SIZE = 100
BASE = "https://chatgpt.com"
LATCHKEY_TIMEOUT = 120

# When ChatGPT 429s us, it does so for many minutes. After this much total
# wait without a successful retry, give up and let the user resume later.
RATE_LIMIT_GIVE_UP_AFTER = 300.0  # seconds


class RateLimited(RuntimeError):
    """Raised when /backend-api keeps returning HTTP 429 after backoff."""


def _parse_status_and_headers(dump: str) -> tuple[int, dict[str, str]]:
    """Parse a `curl -D -` dump (status line + headers, possibly multi-block
    when redirects were followed) into the *final* response's
    (status, lowercased-headers)."""
    status = 0
    headers: dict[str, str] = {}
    for block in dump.split("\r\n\r\n"):
        block = block.strip()
        if not block:
            continue
        lines = block.splitlines()
        first = lines[0]
        if first.startswith("HTTP/"):
            parts = first.split(None, 2)
            if len(parts) >= 2 and parts[1].isdigit():
                status = int(parts[1])
                headers = {}
            for line in lines[1:]:
                if ":" not in line:
                    continue
                name, value = line.split(":", 1)
                headers[name.strip().lower()] = value.strip()
    return status, headers


# ---------------------------------------------------------------------------
# Web API client: shells out to `latchkey curl` (with our shim wired into
# `LATCHKEY_CURL`) for every request. The shim handles the Chrome TLS
# fingerprint that Cloudflare requires; latchkey handles cookie injection.
# ---------------------------------------------------------------------------


class ChatGPTWebClient:
    def __init__(self) -> None:
        self._env = _latchkey_env()
        # Cumulative network time (excludes sleeps), updated by _get.
        self.network_seconds: float = 0.0
        self.requests: int = 0

    def _curl_get(self, url: str) -> tuple[int, str, dict[str, str]]:
        """Run `latchkey curl` once. Returns (status, body, response headers).

        We pass `-D -` to capture *response* headers (not request headers —
        latchkey deliberately hides those from us, since they carry the
        injected auth credentials). The only response-side fields we
        actually read are `Retry-After` (for 429 backoff) and the
        `cf-mitigated` diagnostic on errors; both are server-side metadata,
        not secrets. Exponential backoff works fine without Retry-After, so
        we could drop this if it ever becomes a maintenance burden."""
        with tempfile.NamedTemporaryFile(
            prefix="chatgpt-", suffix=".json", delete=False
        ) as bodyf:
            body_path = Path(bodyf.name)
        try:
            cmd = [
                "latchkey",
                "curl",
                "-sS",
                "-D",
                "-",
                "-H",
                "Accept: application/json",
                "-o",
                str(body_path),
                url,
            ]
            t0 = time.perf_counter()
            proc = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                timeout=LATCHKEY_TIMEOUT,
                check=False,
                env=self._env,
            )
            self.network_seconds += time.perf_counter() - t0
            self.requests += 1
            if proc.returncode != 0:
                raise RuntimeError(
                    f"latchkey curl exit {proc.returncode}; stderr={proc.stderr[:300]}"
                )
            status, resp_headers = _parse_status_and_headers(proc.stdout)
            body_text = body_path.read_text(errors="replace")
            return status, body_text, resp_headers
        finally:
            body_path.unlink(missing_ok=True)

    def _get(self, path: str) -> Any:
        url = f"{BASE}{path}"
        waited = 0.0
        while True:
            status, body_text, resp_headers = self._curl_get(url)
            if status == 200:
                try:
                    return json.loads(body_text)
                except json.JSONDecodeError as e:
                    raise RuntimeError(
                        f"GET {path} -> 200 but non-JSON body: {e}; "
                        f"body[:200]={body_text[:200]}"
                    )
            if status == 429:
                # Honor Retry-After when present; otherwise exponential backoff
                # capped at 60s. Give up after RATE_LIMIT_GIVE_UP_AFTER total.
                ra = resp_headers.get("retry-after")
                try:
                    wait = (
                        float(ra)
                        if ra
                        else min(60.0, 5.0 * (2 ** min(4, int(waited / 5))))
                    )
                except ValueError:
                    wait = 30.0
                if waited + wait > RATE_LIMIT_GIVE_UP_AFTER:
                    raise RateLimited(
                        f"429 for {path}; gave up after {waited:.0f}s of backoff. "
                        "Re-run later — incremental skip will resume from here."
                    )
                print(f"    (429 on {path[:60]}; sleeping {wait:.0f}s)")
                time.sleep(wait)
                waited += wait
                continue
            raise RuntimeError(
                f"GET {path} -> HTTP {status} "
                f"cf-mitigated={resp_headers.get('cf-mitigated')} "
                f"body={body_text[:300]}"
            )

    def me(self) -> dict:
        return self._get("/backend-api/me")

    def list_conversations_page(self, offset: int, limit: int) -> dict:
        return self._get(
            f"/backend-api/conversations?offset={offset}&limit={limit}&order=updated"
        )

    def get_conversation(self, conv_id: str) -> dict:
        return self._get(f"/backend-api/conversation/{conv_id}")


# ---------------------------------------------------------------------------
# Fetch logic.
# ---------------------------------------------------------------------------


def _read_json(path: Path) -> Any:
    if not path.exists():
        return None
    return json.loads(path.read_text())


def _write_json(path: Path, data: Any) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, ensure_ascii=False, indent=2))


def _list_all_conversations(
    client: ChatGPTWebClient, max_pages: int | None
) -> list[dict]:
    """Walk the paginated /backend-api/conversations listing until exhausted."""
    items: list[dict] = []
    offset = 0
    pages = 0
    while True:
        page = client.list_conversations_page(offset, PAGE_SIZE)
        page_items = page.get("items") or []
        items.extend(page_items)
        total = page.get("total")
        print(
            f"    page offset={offset:>5} got={len(page_items):>3} "
            f"total={total} cum={len(items)}"
        )
        offset += len(page_items)
        pages += 1
        if not page_items:
            break
        if total is not None and offset >= total:
            break
        if max_pages is not None and pages >= max_pages:
            print(f"    (stopping at --max-pages={max_pages})")
            break
        time.sleep(SLEEP_BETWEEN)
    return items


def fetch(
    out_dir: Path = typer.Option(
        DEFAULT_OUT_DIR,
        "--out-dir",
        help=f"API-fetched dir (default {DEFAULT_OUT_DIR}).",
    ),
    max_pages: int | None = typer.Option(
        None, "--max-pages", help="Cap the listing walk (debugging)."
    ),
    limit: int | None = typer.Option(
        None,
        "--limit",
        help=(
            "Stop after N successful conversation fetches (skipped/cached "
            "items don't count). For debugging."
        ),
    ),
    sleep_between: float = typer.Option(
        SLEEP_BETWEEN,
        "--sleep-between",
        help=f"Seconds between successful fetches (default {SLEEP_BETWEEN}). 0 disables.",
    ),
) -> None:
    """Incrementally fetch chatgpt.com conversations to JSON cache."""
    out_dir = out_dir.expanduser()
    out_dir.mkdir(parents=True, exist_ok=True)
    convs_dir = out_dir / "conversations"
    convs_dir.mkdir(exist_ok=True)
    index_path = out_dir / "conversations.json"
    me_path = out_dir / "me.json"
    # Local time with explicit offset, per the project timestamp convention.
    started_at = datetime.now().astimezone().isoformat()

    client = ChatGPTWebClient()

    me = client.me()
    _write_json(me_path, me)
    print(f"me: {me.get('email')} ({me.get('id')})")

    print("listing conversations...")
    listing = _list_all_conversations(client, max_pages)
    print(f"listing total: {len(listing)}")

    # Per-conversation incremental fetch: skip when the cached detail's
    # update_time matches the listing's (so we don't refetch unchanged convs).
    # Reorder so missing conversations come first — if we 429 partway through,
    # we want our rate-limit budget spent on actual fetches, not on iterating
    # through already-cached items.
    missing = [it for it in listing if not (convs_dir / f"{it['id']}.json").exists()]
    present = [it for it in listing if (convs_dir / f"{it['id']}.json").exists()]
    ordered = missing + present
    print(f"prioritizing {len(missing)} missing before {len(present)} cached")

    fetched = skipped = errors = 0
    loop_t0 = time.perf_counter()
    sleep_seconds = 0.0
    pbar = tqdm(ordered, unit="conv")
    for item in pbar:
        if limit is not None and fetched + errors >= limit:
            tqdm.write(f"    (--limit {limit} reached; stopping)")
            break
        cid = item["id"]
        api_update = item.get("update_time")
        cache_path = convs_dir / f"{cid}.json"
        cached = _read_json(cache_path)
        # The listing endpoint returns update_time as an ISO-8601 string,
        # but the detail endpoint returns it as a Unix-epoch float — so we
        # can't compare the listing value against cached["update_time"]
        # directly. Stash the listing value under a separate key so future
        # runs have a like-for-like comparison.
        if cached is not None and cached.get("_listing_update_time") == api_update:
            skipped += 1
            pbar.set_postfix(fetched=fetched, skipped=skipped, errors=errors)
            continue
        try:
            full = client.get_conversation(cid)
        except RateLimited as e:
            pbar.close()
            print(f"\n    ⏸ rate-limited; stopping early ({fetched} fetched). {e}")
            break
        except RuntimeError as e:
            tqdm.write(f"    ! {cid[:8]} failed: {e}")
            errors += 1
            pbar.set_postfix(fetched=fetched, skipped=skipped, errors=errors)
            continue
        # Stamp our own provenance/fetch timestamp so we can audit later.
        full["_fetched_at"] = started_at
        # Preserve the listing's update_time verbatim so the next run can
        # do a string-equality skip check (listing ISO-8601 vs detail float
        # would otherwise never match).
        full["_listing_update_time"] = api_update
        _write_json(cache_path, full)
        fetched += 1
        title = (item.get("title") or "")[:40]
        pbar.set_postfix_str(f"{fetched=} {skipped=} {errors=} | {title}")
        if sleep_between > 0:
            time.sleep(sleep_between)
            sleep_seconds += sleep_between

    _write_json(index_path, listing)
    loop_total = time.perf_counter() - loop_t0
    net = client.network_seconds
    other = max(0.0, loop_total - net - sleep_seconds)
    print(f"\nfetched={fetched} skipped={skipped} errors={errors}")
    print(
        f"timing: loop={loop_total:.1f}s  network={net:.1f}s "
        f"({client.requests} req, {net / max(1, client.requests):.2f}s/req)  "
        f"sleep={sleep_seconds:.1f}s  other={other:.1f}s"
    )
    print(f"index: {index_path} ({len(listing)} rows)")
    print(f"details dir: {convs_dir}")


# ---------------------------------------------------------------------------
# Entry point.
# ---------------------------------------------------------------------------


def main() -> None:
    typer.run(fetch)


if __name__ == "__main__":
    sys.exit(main() or 0)
