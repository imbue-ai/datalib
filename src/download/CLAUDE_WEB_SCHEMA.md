# claude.ai web API vs. Anthropic bulk-export schema

This project ingests Anthropic chat data from two transports:

1. **Bulk export** — the zip you get from
   `claude.ai → Settings → Privacy → Export data`. Lands in
   `~/backups/claude/` after manual unzip.
2. **Web API** — incremental scrape of the JSON endpoints that back the
   `claude.ai` SPA. See `scripts/sync_claude_web.py`. Lands in
   `~/backups/claude_api/`.

Both transports describe the same underlying objects (accounts, projects,
conversations, messages, content blocks, attachments) but with different
field coverage. This document records the differences and the precedence
rules the ingest pipeline applies.

## Endpoints (web API)

All paths are relative to `https://claude.ai/api`.

| Method | Path                                                                             | Notes                              |
| ------ | -------------------------------------------------------------------------------- | ---------------------------------- |
| GET    | `/organizations`                                                                 | Lists every org the session can see (Personal + each Team plan). |
| GET    | `/organizations/{org_uuid}/chat_conversations`                                   | Index of conversations for an org. Cheap; safe to poll. `updated_at` drives incrementality. |
| GET    | `/organizations/{org_uuid}/chat_conversations/{conv_uuid}?tree=True&rendering_mode=messages&render_all_tools=true&consistency=strong` | Full conversation tree with messages, content blocks, attachments. |

The conversation listing returns `user_uuid: null` for the personal org;
`account_uuid` has to be patched in from `users.json` (we read the first
uuid in the bulk export's `users.json` for that purpose — see
`_account_uuid_from_users` in `sync_claude_web.py`).

## Auth

claude.ai is gated by Cloudflare. Two layers:

1. **Cookie auth.** `sessionKey` is the long-lived auth cookie. We never
   store it in the repo — it's read out of `latchkey curl -v` stderr at
   run time (latchkey holds the credential).
2. **TLS fingerprint.** Cloudflare returns `cf-mitigated: challenge`
   (HTTP 403) to anything whose JA3/JA4 doesn't look like a real browser,
   regardless of headers. We use `curl-cffi` with `impersonate="chrome"`,
   which bundles curl-impersonate and produces a Chrome JA3.

`cf_clearance` is HttpOnly; if you ever need to copy it manually, grab it
from DevTools → Application → Cookies.

## Field-level schema differences

| Object | Field | Export | API (`tree=True`) | Notes |
| ------ | ----- | ------ | ----------------- | ----- |
| conversation | `account` | `{uuid, ...}` | absent on personal-org listings | Synthesized from `users.json`. |
| conversation | `summary` | string | string | Same. |
| message | `text` | concatenated text + thinking + redaction placeholders | empty string | Synthesized in normalizer by joining `content[].text` and `content[].thinking`. |
| content block | `flags` | `null` (always present) | absent | Normalizer sets `flags: null` so raw_json matches. |
| content block | redaction placeholders (e.g. `[REDACTED]` segments) | present | absent | Cannot be reproduced — accounts for ~11/170 residual text diffs in the verify step. |
| message | extra UI/provenance fields | absent | present (e.g. truncated state, citation refs) | Stored as raw_json, not surfaced as columns. |

Net: the API generally carries **strictly more** structured detail per
message; the export carries some lossy human-readable artifacts (redaction
placeholders, pre-flattened `text`) that the API does not.

## Precedence: API wins

Each ingested directory is tagged `'export'` or `'api'`, and
`merge_anthropic` (in `providers/anthropic/ingest.py`) applies api-wins
precedence in memory before anything reaches SQL:

- **API ingest is authoritative.** For every keyed entity (account,
  project, conversation, message), an api-tagged row beats an
  export-tagged row on the same primary key — every field (including
  `raw_json`) comes from the api parse. `content_blocks` and
  `attachments` are owned wholesale by api for any `message_uuid` an api
  ingest provided, so trimmed/reordered API blocks don't leave orphans.
- **Export ingest never clobbers API.** If an api ingest claimed a key
  earlier in the merge, a later export ingest can't displace it.
- Within the same precedence class, later in the input list wins.

The merged dataclasses then drive QMD rendering and the `grid_rows`
union table directly; per-provider Dolt tables don't exist anymore
(there is nothing to UPSERT against).

Provenance is plumbed end-to-end via the config:

```yaml
sources:
  - name: bulk-export
    provider: anthropic
    kind: export_dir
    path: ~/backups/claude
    provenance: export   # default
  - name: web-api
    provider: anthropic
    kind: export_dir
    path: ~/backups/claude_api
    provenance: api
```


Cosmetic diffs (`raw_json`, `last_seen_at`, `first_seen_at`) are
classified separately from real diffs. Current state: ~157 cosmetic-only
modifications + ~13 real diffs (2 legitimate `updated_at` advances + 11
residual text diffs from redaction placeholders only the export sees).
