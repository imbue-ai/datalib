#!/usr/bin/env python3
"""Incrementally fetch claude.ai web conversations and write them in the same
shape as Anthropic's bulk export, so the ingest pipeline can consume either
indistinguishably.

Auth + transport: every request shells out to `latchkey curl`, which
injects the registered `claude-ai` service cookies/headers. To clear
Cloudflare's managed challenges we point `LATCHKEY_CURL` at
`latchkey_curl_shim.py` (a `curl_cffi` wrapper with a Chrome TLS
fingerprint) — see `download.latchkey_curl_shim`.

Mirrors the shape of `download/chatgpt_web.py` so both downloaders behave
the same way: incremental, missing conversations fetched first (so a 429
spends our budget on conversations that move us forward), tqdm progress.

Usage:
    uv run python -m download.claude_web [options]
"""

from __future__ import annotations

import json
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any

import typer
from tqdm import tqdm

from download.latchkey_curl_shim import latchkey_env as _latchkey_env

DEFAULT_EXPORT_DIR = Path.home() / "backups" / "claude"
DEFAULT_OUT_DIR = Path.home() / "backups" / "claude_api"
DEFAULT_OVERLAP = 3
SLEEP_BETWEEN = 0.4
LATCHKEY_TIMEOUT = 120

CLAUDE_BASE = "https://claude.ai/api"


# ---------------------------------------------------------------------------
# Web API client: shells out to `latchkey curl` (with our shim wired into
# `LATCHKEY_CURL`) for every request. The shim handles the Chrome TLS
# fingerprint that Cloudflare requires; latchkey handles cookie injection.
# ---------------------------------------------------------------------------


class ClaudeWebClient:
    def __init__(self) -> None:
        self._env = _latchkey_env()

    def _get(self, path: str) -> Any:
        url = f"{CLAUDE_BASE}{path}"
        with tempfile.NamedTemporaryFile(
            prefix="claude-", suffix=".json", delete=False
        ) as bodyf:
            body_path = Path(bodyf.name)
        try:
            cmd = [
                "latchkey",
                "curl",
                "-sS",
                "-H",
                "Accept: application/json",
                "-o",
                str(body_path),
                "-w",
                "%{http_code}",
                url,
            ]
            proc = subprocess.run(
                cmd,
                capture_output=True,
                text=True,
                timeout=LATCHKEY_TIMEOUT,
                check=False,
                env=self._env,
            )
            if proc.returncode != 0:
                raise RuntimeError(
                    f"GET {path}: latchkey curl exit {proc.returncode}; "
                    f"stderr={proc.stderr[:300]}"
                )
            status_txt = proc.stdout.strip()
            try:
                status = int(status_txt) if status_txt else 0
            except ValueError:
                status = 0
            body_text = body_path.read_text(errors="replace")
        finally:
            body_path.unlink(missing_ok=True)
        if status != 200:
            raise RuntimeError(f"GET {path} -> HTTP {status}: {body_text[:300]}")
        return json.loads(body_text)

    def list_orgs(self) -> list[dict]:
        return self._get("/organizations")

    def list_conversations(self, org_uuid: str) -> list[dict]:
        return self._get(f"/organizations/{org_uuid}/chat_conversations")

    def get_conversation(self, org_uuid: str, conv_uuid: str) -> dict:
        q = "tree=True&rendering_mode=messages&render_all_tools=true&consistency=strong"
        return self._get(
            f"/organizations/{org_uuid}/chat_conversations/{conv_uuid}?{q}"
        )


# ---------------------------------------------------------------------------
# Fetch logic.
# ---------------------------------------------------------------------------


def _load_conv_index(path: Path) -> dict[str, dict]:
    if not path.exists():
        return {}
    data = json.loads(path.read_text())
    return {c["uuid"]: c for c in data if c.get("uuid")}


def _normalize_to_export_shape(
    api_conv: dict, account_uuid: str | None, org_uuid: str
) -> dict:
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


def fetch(
    export_dir: Path = typer.Option(
        DEFAULT_EXPORT_DIR,
        "--export-dir",
        help=f"Anthropic bulk-export dir (default {DEFAULT_EXPORT_DIR}).",
    ),
    out_dir: Path = typer.Option(
        DEFAULT_OUT_DIR,
        "--out-dir",
        "--api-dir",
        help=f"API-fetched dir (default {DEFAULT_OUT_DIR}).",
    ),
    overlap: int = typer.Option(
        DEFAULT_OVERLAP,
        "--overlap",
        help="N most-recent export convs to refetch as overlap.",
    ),
) -> None:
    """Incrementally fetch claude.ai conversations into the export shape."""
    out_dir = out_dir.expanduser()
    export_dir = export_dir.expanduser()
    out_dir.mkdir(parents=True, exist_ok=True)
    out_conv_path = out_dir / "conversations.json"
    out_users_path = out_dir / "users.json"

    existing_export = _load_conv_index(export_dir / "conversations.json")
    existing_api = _load_conv_index(out_conv_path)
    print(f"export: {len(existing_export)} convs | api-cache: {len(existing_api)}")

    # Overlap: N most-recently-updated conversations from the export, refetched
    # so we can verify the API and export agree.
    export_sorted = sorted(
        existing_export.values(), key=lambda c: c.get("updated_at") or "", reverse=True
    )
    overlap_uuids = {c["uuid"] for c in export_sorted[:overlap]}
    print(f"overlap (refetch from API): {len(overlap_uuids)} most-recent export convs")

    # users.json: copy from export verbatim if available (preserves account_uuid).
    src_users = export_dir / "users.json"
    if src_users.exists() and not out_users_path.exists():
        shutil.copy(src_users, out_users_path)

    account_uuid = _account_uuid_from_users(export_dir) or _account_uuid_from_users(
        out_dir
    )
    if not account_uuid:
        print("warning: no users.json found — conversation.account.uuid will be empty")

    client = ClaudeWebClient()

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
                print(
                    f"  [{org_name}] 403 — skipping (no chat permission for this org)"
                )
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
            elif (
                in_api is None
                and in_export is not None
                and in_export.get("updated_at") != api_updated
            ):
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

    sorted_convs = sorted(
        merged.values(), key=lambda c: c.get("updated_at") or "", reverse=True
    )
    out_conv_path.write_text(json.dumps(sorted_convs, ensure_ascii=False, indent=2))

    print(
        f"\nfetched={fetched} skipped={skipped} forbidden_orgs={forbidden} errors={errors}"
    )
    print(f"total in {out_conv_path}: {len(sorted_convs)}")


# ---------------------------------------------------------------------------
# Entry point.
# ---------------------------------------------------------------------------


def main() -> None:
    typer.run(fetch)


if __name__ == "__main__":
    sys.exit(main() or 0)
