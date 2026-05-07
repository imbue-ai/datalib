#!/usr/bin/env python3
"""Incrementally fetch chatgpt.com web conversations and cache them as JSON.

This is the OpenAI/ChatGPT counterpart to scripts/sync_claude_web.py. For now
it only fetches and caches; ingest into Dolt under an `openai_*` schema is a
separate (forthcoming) step.

Auth: assumes `latchkey curl` is configured for the `chatgpt` service. We
read the injected `Authorization: Bearer <accessToken>` and `Cookie`
(includes `cf_clearance` + `__Secure-next-auth.session-token`) out of
latchkey's `-v` output, then issue the real requests via `curl_cffi` so we
get a Chrome TLS fingerprint and clear Cloudflare's managed challenges.

Usage:
    uv run python scripts/sync_chatgpt_web.py fetch [options]

Output layout (default ~/backups/chatgpt_api/):
    me.json                       # current user profile
    conversations.json            # combined paginated listing index
    conversations/<id>.json       # full conversation tree (mapping/DAG)
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import time
from datetime import datetime
from pathlib import Path
from typing import Any

from curl_cffi import requests as curl_requests
from tqdm import tqdm

DEFAULT_OUT_DIR = Path.home() / "backups" / "chatgpt_api"
SLEEP_BETWEEN = 0.5
PAGE_SIZE = 100
IMPERSONATE = "chrome"
BASE = "https://chatgpt.com"

# When ChatGPT 429s us, it does so for many minutes. After this much total
# wait without a successful retry, give up and let the user resume later.
RATE_LIMIT_GIVE_UP_AFTER = 300.0  # seconds


class RateLimited(RuntimeError):
    """Raised when /backend-api keeps returning HTTP 429 after backoff."""


# ---------------------------------------------------------------------------
# Auth: pull Authorization + Cookie out of latchkey at run time.
# ---------------------------------------------------------------------------

def get_auth_headers() -> dict[str, str]:
    """Run `latchkey curl -v` against /api/auth/session and harvest the
    Authorization/Cookie/User-Agent headers latchkey injects."""
    proc = subprocess.run(
        ["latchkey", "curl", "-v", "-o", "/dev/null", "-s",
         f"{BASE}/api/auth/session"],
        capture_output=True, text=True, check=False,
    )
    headers: dict[str, str] = {}
    for name in ("Authorization", "Cookie", "User-Agent"):
        m = re.search(rf"^> {name}: (.+)$", proc.stderr, flags=re.MULTILINE)
        if not m:
            raise RuntimeError(
                f"Could not extract {name!r} from `latchkey curl -v`. "
                "Is the `chatgpt` service registered with `latchkey auth set`?\n"
                f"latchkey stderr tail:\n{proc.stderr[-500:]}"
            )
        headers[name] = m.group(1).strip()
    headers["Accept"] = "application/json"
    return headers


# ---------------------------------------------------------------------------
# Web API client (curl_cffi with Chrome TLS fingerprint).
# ---------------------------------------------------------------------------

class ChatGPTWebClient:
    def __init__(self, headers: dict[str, str]) -> None:
        self._headers = headers

    def _get(self, path: str) -> Any:
        url = f"{BASE}{path}"
        waited = 0.0
        while True:
            r = curl_requests.get(url, impersonate=IMPERSONATE,
                                  headers=self._headers)
            if r.status_code == 200:
                return r.json()
            if r.status_code == 429:
                # Honor Retry-After when present; otherwise exponential backoff
                # capped at 60s. Give up after RATE_LIMIT_GIVE_UP_AFTER total.
                ra = r.headers.get("Retry-After")
                try:
                    wait = float(ra) if ra else min(60.0, 5.0 * (2 ** min(4, int(waited / 5))))
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
                f"GET {path} -> HTTP {r.status_code} "
                f"cf-mitigated={r.headers.get('cf-mitigated')} body={r.text[:300]}"
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


def _list_all_conversations(client: ChatGPTWebClient,
                            max_pages: int | None) -> list[dict]:
    """Walk the paginated /backend-api/conversations listing until exhausted."""
    items: list[dict] = []
    offset = 0
    pages = 0
    while True:
        page = client.list_conversations_page(offset, PAGE_SIZE)
        page_items = page.get("items") or []
        items.extend(page_items)
        total = page.get("total")
        print(f"    page offset={offset:>5} got={len(page_items):>3} "
              f"total={total} cum={len(items)}")
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


def fetch(args: argparse.Namespace) -> None:
    out_dir: Path = args.out_dir
    out_dir.mkdir(parents=True, exist_ok=True)
    convs_dir = out_dir / "conversations"
    convs_dir.mkdir(exist_ok=True)
    index_path = out_dir / "conversations.json"
    me_path = out_dir / "me.json"
    # Local time with explicit offset, per the project timestamp convention.
    started_at = datetime.now().astimezone().isoformat()

    headers = get_auth_headers()
    client = ChatGPTWebClient(headers)

    me = client.me()
    _write_json(me_path, me)
    print(f"me: {me.get('email')} ({me.get('id')})")

    print("listing conversations...")
    listing = _list_all_conversations(client, args.max_pages)
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
    pbar = tqdm(ordered, unit="conv")
    for item in pbar:
        cid = item["id"]
        api_update = item.get("update_time")
        cache_path = convs_dir / f"{cid}.json"
        cached = _read_json(cache_path)
        if cached is not None and cached.get("update_time") == api_update:
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
        _write_json(cache_path, full)
        fetched += 1
        title = (item.get("title") or "")[:40]
        pbar.set_postfix_str(f"{fetched=} {skipped=} {errors=} | {title}")
        time.sleep(SLEEP_BETWEEN)

    _write_json(index_path, listing)
    print(f"\nfetched={fetched} skipped={skipped} errors={errors}")
    print(f"index: {index_path} ({len(listing)} rows)")
    print(f"details dir: {convs_dir}")


# ---------------------------------------------------------------------------
# Entry point.
# ---------------------------------------------------------------------------

def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--out-dir", type=Path, default=DEFAULT_OUT_DIR,
                        help=f"API-fetched dir (default {DEFAULT_OUT_DIR})")
    parser.add_argument("--max-pages", type=int, default=None,
                        help="Cap the listing walk (debugging).")
    args = parser.parse_args(argv)
    fetch(args)
    return 0


if __name__ == "__main__":
    sys.exit(main())
