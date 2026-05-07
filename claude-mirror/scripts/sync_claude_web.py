#!/usr/bin/env python3
"""Incrementally fetch claude.ai web conversations and write them in the same
shape as Anthropic's bulk export, then verify the overlap matches the export
via Dolt's diffing features.

Auth: assumes `latchkey curl` is configured for the `claude-ai` service. The
script reads the sessionKey out of latchkey's `-v` output and then makes the
real requests via `curl_cffi` so we get a Chrome TLS fingerprint and clear
Cloudflare's managed challenges.

Usage:
    uv run python scripts/sync_claude_web.py fetch  [options]
    uv run python scripts/sync_claude_web.py verify [options]
    uv run python scripts/sync_claude_web.py sync   [options]   # fetch + verify
"""

from __future__ import annotations

import argparse
import json
import re
import shutil
import subprocess
import sys
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterable

from curl_cffi import requests as curl_requests

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

        for item in listing:
            uuid = item["uuid"]
            api_updated = item.get("updated_at")
            in_export = existing_export.get(uuid)
            in_api = existing_api.get(uuid)

            if in_export is None and in_api is None:
                why = "new"
            elif uuid in overlap_uuids:
                why = "overlap"
            elif in_api is not None and in_api.get("updated_at") != api_updated:
                why = "updated"
            elif in_api is None and in_export is not None and \
                 in_export.get("updated_at") != api_updated:
                why = "export-stale"
            else:
                skipped += 1
                continue

            try:
                full = client.get_conversation(org_uuid, uuid)
            except RuntimeError as e:
                print(f"    ! {uuid[:8]} ({why}) failed: {e}")
                errors += 1
                continue
            merged[uuid] = _normalize_to_export_shape(full, account_uuid, org_uuid)
            fetched += 1
            print(f"    + {uuid[:8]} ({why}) {(item.get('name') or '')[:60]!r}")
            time.sleep(SLEEP_BETWEEN)

    sorted_convs = sorted(merged.values(),
                          key=lambda c: c.get("updated_at") or "", reverse=True)
    out_conv_path.write_text(json.dumps(sorted_convs, ensure_ascii=False, indent=2))

    print(f"\nfetched={fetched} skipped={skipped} forbidden_orgs={forbidden} errors={errors}")
    print(f"total in {out_conv_path}: {len(sorted_convs)}")


# ---------------------------------------------------------------------------
# Verify: ingest both dirs into a temp Dolt repo and run dolt diff on overlap.
# ---------------------------------------------------------------------------

DIFFED_TABLES = [
    "anthropic_conversations",
    "anthropic_messages",
    "anthropic_content_blocks",
    "anthropic_attachments",
]


def verify(args: argparse.Namespace) -> None:
    # Lazy imports so `fetch` doesn't pay for them when Dolt isn't running.
    from claude_mirror.config import Config, DoltConfig  # noqa: WPS433
    from claude_mirror.dolt_service import DoltService  # noqa: WPS433
    from claude_mirror.providers.anthropic.ingest import ingest_export_dir  # noqa: WPS433

    export_dir: Path = args.export_dir
    api_dir: Path = args.api_dir

    export_uuids = set(_load_conv_index(export_dir / "conversations.json"))
    api_uuids = set(_load_conv_index(api_dir / "conversations.json"))
    overlap = sorted(export_uuids & api_uuids)
    print(f"overlap rows for diff: {len(overlap)} "
          f"(export={len(export_uuids)}, api={len(api_uuids)})")
    if not overlap:
        print("nothing to verify — no shared conversation UUIDs")
        return

    tmp = Path(tempfile.mkdtemp(prefix="claude-mirror-verify-"))
    print(f"temp dolt root: {tmp}")
    cfg = Config(root=tmp, dolt=DoltConfig(port=args.port), sources=[])

    started_export = "1970-01-01T00:00:00Z"  # synthetic timestamps so commits diff cleanly
    started_api = "1970-01-01T00:00:01Z"

    try:
        with DoltService(cfg) as dolt:
            with dolt.connect() as conn:
                ingest_export_dir(conn, export_dir, started_export)
            commit_a = dolt.commit(f"export: {export_dir}")
            with dolt.connect() as conn:
                ingest_export_dir(conn, api_dir, started_api)
            commit_b = dolt.commit(f"api: {api_dir}") or commit_a

            print(f"\ncommits: A={commit_a[:10] if commit_a else None} "
                  f"B={commit_b[:10] if commit_b else None}\n")
            _print_diff(dolt, commit_a, commit_b, overlap)
    finally:
        if not args.keep:
            shutil.rmtree(tmp, ignore_errors=True)
        else:
            print(f"\n(left dolt repo at {tmp})")


def _print_diff(dolt, commit_a: str | None, commit_b: str | None,
                overlap: list[str]) -> None:
    if not commit_a or not commit_b or commit_a == commit_b:
        print("(no diff between commits — API ingest produced zero changes)")
        return
    overlap_sql = ",".join(f"'{u}'" for u in overlap)
    # Columns expected to differ for benign reasons (API includes extra
    # provenance/UI fields in raw_json; ingest timestamps will always differ).
    COSMETIC = {"raw_json", "last_seen_at", "first_seen_at"}
    with dolt.connect() as conn, conn.cursor() as cur:
        real_diffs: list[str] = []
        cosmetic_only = 0
        added_total = 0
        modified_total = 0
        removed_total = 0
        # Build the set of message_uuids belonging to overlap convs (used for
        # filtering content_blocks / attachments diff queries).
        cur.execute(
            f"SELECT message_uuid FROM anthropic_messages "
            f"WHERE conversation_uuid IN ({overlap_sql})"
        )
        overlap_msg_uuids = [r[0] for r in cur.fetchall()]
        msg_sql = ",".join(f"'{u}'" for u in overlap_msg_uuids) or "''"

        for table in DIFFED_TABLES:
            # dolt_diff_<t> columns are prefixed to_/from_; filter on either side.
            if table in ("anthropic_content_blocks", "anthropic_attachments"):
                key = "message_uuid"
                in_list = msg_sql
            else:
                key = "conversation_uuid"
                in_list = overlap_sql
            where = (
                f"(COALESCE(to_{key}, from_{key}) IN ({in_list}))"
            )

            cur.execute(
                f"SELECT diff_type, COUNT(*) FROM dolt_diff_{table} "
                f"WHERE from_commit = %s AND to_commit = %s AND ({where}) "
                f"GROUP BY diff_type",
                (commit_a, commit_b),
            )
            rows = dict(cur.fetchall())
            if not rows:
                print(f"  {table}: (no rows in overlap differ)")
                continue
            print(f"  {table}: " + ", ".join(f"{k}={v}" for k, v in rows.items()))
            added_total += rows.get("added", 0)
            removed_total += rows.get("removed", 0)
            mod = rows.get("modified", 0)
            modified_total += mod

            # For modified rows, classify each by whether *any* non-cosmetic
            # column differs. Walk all of them, not just a sample.
            cur.execute(
                f"SELECT * FROM dolt_diff_{table} "
                f"WHERE from_commit = %s AND to_commit = %s AND ({where}) "
                f"AND diff_type = 'modified'",
                (commit_a, commit_b),
            )
            cols = [d[0] for d in cur.description]
            non_cosmetic_seen: dict[str, int] = {}
            for row in cur.fetchall():
                row_d = dict(zip(cols, row))
                to_cols = {k[3:]: v for k, v in row_d.items()
                           if k.startswith("to_") and k[3:] not in ("commit", "commit_date")}
                from_cols = {k[5:]: v for k, v in row_d.items()
                             if k.startswith("from_") and k[5:] not in ("commit", "commit_date")}
                changed_cols = {k for k in to_cols
                                if to_cols[k] != from_cols.get(k)}
                non_cosmetic = changed_cols - COSMETIC
                if non_cosmetic:
                    for c in non_cosmetic:
                        non_cosmetic_seen[c] = non_cosmetic_seen.get(c, 0) + 1
                else:
                    cosmetic_only += 1
            if non_cosmetic_seen:
                real_diffs.append(f"{table}: {non_cosmetic_seen}")
                print(f"    non-cosmetic columns differing: {non_cosmetic_seen}")

        # Final verdict.
        print()
        print(f"summary: modified={modified_total} (cosmetic-only={cosmetic_only}) "
              f"added={added_total} removed={removed_total}")
        if real_diffs:
            print("result: ⚠️ overlap rows have real (non-raw_json) differences:")
            for d in real_diffs:
                print(f"    - {d}")
        elif modified_total:
            print("result: ✅ overlap rows match — only raw_json/last_seen_at differ "
                  "(API has extra metadata fields the export omits)")
        else:
            print("result: ✅ overlap rows are byte-identical")


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
    sub = parser.add_subparsers(dest="cmd", required=True)

    p_fetch = sub.add_parser("fetch", help="Pull from claude.ai web API.")
    _add_common(p_fetch)
    p_fetch.add_argument("--overlap", type=int, default=DEFAULT_OVERLAP,
                         help="N most-recent export convs to refetch as overlap")
    p_fetch.set_defaults(func=fetch)

    p_verify = sub.add_parser("verify", help="Dolt-diff overlap rows.")
    _add_common(p_verify)
    p_verify.add_argument("--port", type=int, default=3307,
                          help="Dolt sql-server port for the temp repo")
    p_verify.add_argument("--keep", action="store_true",
                          help="Keep the temp dolt repo for inspection")
    # alias compat
    p_verify.set_defaults(func=lambda a: verify(_with_api_dir(a)))

    p_sync = sub.add_parser("sync", help="fetch + verify.")
    _add_common(p_sync)
    p_sync.add_argument("--overlap", type=int, default=DEFAULT_OVERLAP)
    p_sync.add_argument("--port", type=int, default=3307)
    p_sync.add_argument("--keep", action="store_true")
    p_sync.set_defaults(func=lambda a: (fetch(a), verify(_with_api_dir(a))))

    args = parser.parse_args(argv)
    args.func(args)
    return 0


def _with_api_dir(a: argparse.Namespace) -> argparse.Namespace:
    # `verify`/`sync` consume `api_dir` while `fetch` consumes `out_dir`.
    a.api_dir = a.out_dir
    return a


if __name__ == "__main__":
    sys.exit(main())
