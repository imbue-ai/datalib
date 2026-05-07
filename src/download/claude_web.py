#!/usr/bin/env python3
"""Incrementally fetch claude.ai web conversations and write them in the same
shape as Anthropic's bulk export, so the ingest pipeline can consume either
indistinguishably.

Auth: assumes `latchkey curl` is configured for the `claude-ai` service. The
script reads the sessionKey out of latchkey's `-v` output and then makes the
real requests via `curl_cffi` so we get a Chrome TLS fingerprint and clear
Cloudflare's managed challenges.

Mirrors the shape of `download/chatgpt_web.py` so both downloaders behave
the same way: incremental, missing conversations fetched first (so a 429
spends our budget on conversations that move us forward), tqdm progress.

Usage:
    uv run python -m download.claude_web [options]
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Iterable

from curl_cffi import requests as curl_requests
from tqdm import tqdm

DEFAULT_EXPORT_DIR = Path.home() / "backups" / "claude"
DEFAULT_OUT_DIR = Path.home() / "backups" / "claude_api"
DEFAULT_OVERLAP = 3
SLEEP_BETWEEN = 0.4
IMPERSONATE = "chrome"

CLAUDE_BASE = "https://claude.ai/api"


# ---------------------------------------------------------------------------
# Auth: pull the sessionKey cookie out of latchkey at run time.
# ---------------------------------------------------------------------------

def get_session_cookie() -> str:
    proc = subprocess.run(
        ["latchkey", "curl", "-v", "-o", "/dev/null", "-s", f"{CLAUDE_BASE}/organizations"],
        capture_output=True, text=True, check=False,
    )
    # latchkey writes its injected request headers to stderr (curl -v).
    m = re.search(r"^> Cookie: (.+)$", proc.stderr, flags=re.MULTILINE)
    if not m:
        raise RuntimeError(
            "Could not extract Cookie from `latchkey curl -v`. "
            "Is the `claude-ai` service registered with `latchkey auth set`?"
        )
    return m.group(1).strip()


# ---------------------------------------------------------------------------
# Web API client (curl_cffi with Chrome TLS fingerprint).
# ---------------------------------------------------------------------------

class ClaudeWebClient:
    def __init__(self, cookie: str) -> None:
        self._cookie = cookie

    def _get(self, path: str) -> Any:
        url = f"{CLAUDE_BASE}{path}"
        r = curl_requests.get(url, impersonate=IMPERSONATE,
                              headers={"Cookie": self._cookie,
                                       "Accept": "application/json"})
        if r.status_code != 200:
            raise RuntimeError(f"GET {path} -> HTTP {r.status_code}: {r.text[:300]}")
        return r.json()

    def list_orgs(self) -> list[dict]:
        return self._get("/organizations")

    def list_conversations(self, org_uuid: str) -> list[dict]:
        return self._get(f"/organizations/{org_uuid}/chat_conversations")

    def get_conversation(self, org_uuid: str, conv_uuid: str) -> dict:
        q = "tree=True&rendering_mode=messages&render_all_tools=true&consistency=strong"
        return self._get(f"/organizations/{org_uuid}/chat_conversations/{conv_uuid}?{q}")


# ---------------------------------------------------------------------------
# Fetch logic.
# ---------------------------------------------------------------------------

def _load_conv_index(path: Path) -> dict[str, dict]:
    if not path.exists():
        return {}
    data = json.loads(path.read_text())
    return {c["uuid"]: c for c in data if c.get("uuid")}


def _normalize_to_export_shape(api_conv: dict, account_uuid: str | None,
                               org_uuid: str) -> dict:
    """Coerce the /chat_conversations/{id}?tree=True response into the export
    shape. The API:
      - omits `account` (we synthesize from a known account_uuid)
      - leaves message.text empty and puts prose in content[].text (we join)
      - drops `flags` from content blocks (export has flags=null)
    The parser is permissive about extra fields so we leave the rest alone."""
    out = dict(api_conv)
    if account_uuid:
        out.setdefault("account", {"uuid": account_uuid})
    msgs = out.get("chat_messages") or []
    for m in msgs:
        if not m.get("text"):
            m["text"] = _synthesize_message_text(m.get("content") or [])
        for b in m.get("content") or []:
            b.setdefault("flags", None)
    out["_source"] = {"via": "claude.ai/api", "org_uuid": org_uuid}
    return out


def _synthesize_message_text(blocks: list[dict]) -> str:
    """Recreate the export's top-level message.text. The export joins each
    content block's prose: text-block.text, thinking-block.thinking. The export
    also inserts placeholders around redacted thinking that we cannot
    reproduce, so this won't be byte-identical for messages with cut-off
    thinking — but it preserves all the actual content the API returns."""
    parts: list[str] = []
    for b in blocks:
        t = b.get("type")
        if t == "text":
            parts.append(b.get("text") or "")
        elif t == "thinking":
            parts.append(b.get("thinking") or "")
    return "".join(parts)


def _account_uuid_from_users(export_dir: Path) -> str | None:
    """Read the first account uuid out of the export's users.json so we can
    backfill conversation.account.uuid (the listing endpoint returns
    user_uuid=null for personal-org conversations)."""
    p = export_dir / "users.json"
    if not p.exists():
        return None
    try:
        users = json.loads(p.read_text())
        if isinstance(users, list) and users:
            return users[0].get("uuid")
    except Exception:
        return None
    return None


def fetch(args: argparse.Namespace) -> None:
    out_dir: Path = args.out_dir
    out_dir.mkdir(parents=True, exist_ok=True)
    out_conv_path = out_dir / "conversations.json"
    out_users_path = out_dir / "users.json"

    export_dir: Path = args.export_dir
    existing_export = _load_conv_index(export_dir / "conversations.json")
    existing_api = _load_conv_index(out_conv_path)
    print(f"export: {len(existing_export)} convs | api-cache: {len(existing_api)}")

    # Overlap: N most-recently-updated conversations from the export, refetched
    # so we can verify the API and export agree.
    export_sorted = sorted(existing_export.values(),
                           key=lambda c: c.get("updated_at") or "", reverse=True)
    overlap_uuids = {c["uuid"] for c in export_sorted[: args.overlap]}
    print(f"overlap (refetch from API): {len(overlap_uuids)} most-recent export convs")

    # users.json: copy from export verbatim if available (preserves account_uuid).
    src_users = export_dir / "users.json"
    if src_users.exists() and not out_users_path.exists():
        shutil.copy(src_users, out_users_path)

    account_uuid = _account_uuid_from_users(export_dir) or \
                   _account_uuid_from_users(out_dir)
    if not account_uuid:
        print("warning: no users.json found — conversation.account.uuid will be empty")

    cookie = get_session_cookie()
    client = ClaudeWebClient(cookie)

    orgs = client.list_orgs()
    print(f"orgs from API: {len(orgs)}")

    merged: dict[str, dict] = dict(existing_api)
    fetched = skipped = forbidden = errors = 0

    for org in orgs:
        org_uuid = org["uuid"]
        org_name = org.get("name") or org_uuid[:8]
        try:
            listing = client.list_conversations(org_uuid)
        except RuntimeError as e:
            if "HTTP 403" in str(e):
                print(f"  [{org_name}] 403 — skipping (no chat permission for this org)")
                forbidden += 1
                continue
            raise
        print(f"  [{org_name}] {len(listing)} conversations")
        time.sleep(SLEEP_BETWEEN)

        # Decide what to do with each listing item, then sort so fully-new
        # conversations are pulled first. Mirrors the chatgpt_web ordering:
        # if we get rate-limited or interrupted, the budget went to fetches
        # that move us forward rather than to revalidating cached items.
        plan: list[tuple[str, dict, str]] = []
        for item in listing:
            uuid = item["uuid"]
            api_updated = item.get("updated_at")
            in_export = existing_export.get(uuid)
            in_api = existing_api.get(uuid)
            if in_export is None and in_api is None:
                plan.append(("new", item, ""))
            elif uuid in overlap_uuids:
                plan.append(("overlap", item, ""))
            elif in_api is not None and in_api.get("updated_at") != api_updated:
                plan.append(("updated", item, ""))
            elif in_api is None and in_export is not None and \
                 in_export.get("updated_at") != api_updated:
                plan.append(("export-stale", item, ""))
            else:
                skipped += 1

        priority = {"new": 0, "overlap": 1, "updated": 2, "export-stale": 3}
        plan.sort(key=lambda t: priority[t[0]])

        pbar = tqdm(plan, unit="conv", desc=org_name, leave=False)
        for why, item, _ in pbar:
            uuid = item["uuid"]
            org_uuid = org["uuid"]
            try:
                full = client.get_conversation(org_uuid, uuid)
            except RuntimeError as e:
                tqdm.write(f"    ! {uuid[:8]} ({why}) failed: {e}")
                errors += 1
                continue
            merged[uuid] = _normalize_to_export_shape(full, account_uuid, org_uuid)
            fetched += 1
            pbar.set_postfix(fetched=fetched, skipped=skipped, errors=errors)
            time.sleep(SLEEP_BETWEEN)
        pbar.close()

    sorted_convs = sorted(merged.values(),
                          key=lambda c: c.get("updated_at") or "", reverse=True)
    out_conv_path.write_text(json.dumps(sorted_convs, ensure_ascii=False, indent=2))

    print(f"\nfetched={fetched} skipped={skipped} forbidden_orgs={forbidden} errors={errors}")
    print(f"total in {out_conv_path}: {len(sorted_convs)}")

# ---------------------------------------------------------------------------
# Entry point.
# ---------------------------------------------------------------------------

def _add_common(p: argparse.ArgumentParser) -> None:
    p.add_argument("--export-dir", type=Path, default=DEFAULT_EXPORT_DIR,
                   help=f"Anthropic bulk-export dir (default {DEFAULT_EXPORT_DIR})")
    p.add_argument("--out-dir", "--api-dir", dest="out_dir", type=Path,
                   default=DEFAULT_OUT_DIR,
                   help=f"API-fetched dir (default {DEFAULT_OUT_DIR})")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    _add_common(parser)
    parser.add_argument("--overlap", type=int, default=DEFAULT_OVERLAP,
                        help="N most-recent export convs to refetch as overlap")
    args = parser.parse_args(argv)
    fetch(args)
    return 0


if __name__ == "__main__":
    sys.exit(main())
