# personal-mirror — agent runbook

Quick references for AI/human contributors. See
`src/download/CLAUDE_WEB_SCHEMA.md` for the conceptual model and
field-level diffs between the bulk-export and web-API transports.

## Repo layout

```
schemas/         cross-language source of truth. Each *.schema.json gets
                 codegen-emitted Python (DDL + dataclass), Rust (struct +
                 const DDL), and TypeScript artifacts. See docs/grid_rows.md.
src/
  download/    Per-provider incremental downloaders (claude.ai, chatgpt.com).
               Output a "raw" tree on disk; not under our control schema.
  ingest/      Config-driven CLI that takes raw + takeout dirs and writes
               into Dolt + renders qmd markdown. The "owned" output.
               `grid_rows.py` populates the denormalized grid_rows union
               table from the per-provider tables on every ingest.
tests/         pytest suite; Bazel-only goldens under tests/__snapshots__/
tests/fixtures/  TNG-themed source JSON + cached `ingested/` artifact.
docs/          architecture notes (grid_rows.md, ...).
```

## The grid_rows union table

The Vue grid is backed by a single denormalized table, `grid_rows`,
populated at the end of every ingest from the authoritative per-provider
tables. The Rust backend (`frankweiler/backend/core/src/db.rs`) issues
*one* SELECT against `grid_rows` to render the grid — no per-provider
branches in the query path. Schema (column names, types, per-provider
mappings) lives in `schemas/grid_rows.schema.json`; codegen produces
matching Python/Rust/TypeScript types. See `docs/grid_rows.md` for the
full architecture.

When you add or change a `grid_rows` column:

1. Edit `schemas/grid_rows.schema.json` (don't forget `x-mapping`).
2. Re-run codegen (see README).
3. Update `src/ingest/grid_rows.py` to populate the new column from each
   provider's per-provider tables.
4. Update the row mapper in `frankweiler/backend/core/src/db.rs` to read
   it back, plus `SearchRow` in `search.rs` if the column reaches the API.
5. Re-bake snapshots: `uv run pytest tests/test_snapshots.py --snapshot-update`.

## QMDs are write-only

Ingest renders QMD markdown files for human/Quarto consumption. The
backend serves those files **verbatim** (frontmatter stripped) at
`/api/chat/{uuid}` — it never parses them back. Structured fields
(name, account, project, channel, created_at, source_label) come from
`grid_rows` in `mirror.sqlite`. Per-message anchors used by the UI
(scroll-to-message, highlight) come from `<div id="m-{uuid}"
data-msg-index="N" class="msg msg--{provider}">` wrappers the renderer
emits in the body. If you find yourself writing a QMD parser in the
backend, stop — add the field to `grid_rows` instead.

## Running tests

**Default to `bazelisk test //...` for any "are tests passing?" question.**
It's the source of truth: it runs Python, Rust, cross-language goldens,
and the Playwright e2e suite in one shot, the same way CI does. Bazel's
action cache makes re-runs cheap — unchanged targets are served from
cache, so iterating costs only what you actually touched. Reach for
`uv run pytest` / `cargo test` / `pnpm test` only for tight inner-loop
iteration on a single language, and confirm with `bazelisk test //...`
before declaring done.

## Common commands

```bash
# Source of truth — run this before claiming tests pass
bazelisk test //...

# Python-only inner loop (faster, narrower)
uv run pytest

# Ingest configured sources into the Dolt repo (per ~/.config/personal-mirror/config.yaml)
uv run python -m ingest

# Incrementally fetch new conversations from the claude.ai web API
uv run python -m download.claude_web

# Same for chatgpt.com
uv run python -m download.chatgpt_web

# Incrementally export Slack channels/threads/messages/reactions to JSONL
uv run python -m download.slack_web --channels general engineering
```

## Provenance / "API wins"

Each parsed source carries an `"export"` / `"api"` tag. The
`merge_anthropic` step in `src/ingest/providers/anthropic/ingest.py`
applies api-wins precedence in memory: api-tagged rows beat export-tagged
rows on the same primary key, and api ingests own content
blocks/attachments wholesale per message (replacing any earlier export
blocks for that message) so trimmed blocks don't leave orphans. The
union `grid_rows` table is the only SQL artifact this produces; per-
provider Dolt tables no longer exist.

Configure provenance in `config.yaml` per source:

```yaml
sources:
  - { name: bulk-export, provider: anthropic, kind: export_dir, path: ~/backups/claude,     provenance: export }
  - { name: web-api,     provider: anthropic, kind: export_dir, path: ~/backups/claude_api, provenance: api    }
```

## Timestamp convention

Every timestamp stored anywhere in this project — Dolt columns, JSON cache
files, QMD frontmatter — is an **ISO-8601 string that preserves the
timezone offset present in the source**.

- If the upstream API gave us `2026-05-04T03:42:05-07:00`, we store
  `2026-05-04T03:42:05-07:00` verbatim. Don't normalize to UTC — the local
  offset itself carries information (it's how the timestamp would have
  rendered to the human who saw it), and once dropped we can't get it back.
- If the upstream gave us `...Z`, leave it as `Z` — that's still a valid
  offset.
- If the upstream gave us a unix-epoch number (no source offset), render
  it as UTC with an explicit `+00:00` suffix, e.g. `2026-05-04T10:42:05.123456+00:00`.
  Use `datetime.fromtimestamp(t, tz=timezone.utc).isoformat()` —
  *not* `.strftime("...Z")`.
- For our own "now" timestamps (`first_seen_at`, `last_seen_at`,
  ingest-started markers, `_fetched_at`): use **local** time with explicit
  offset, `datetime.now().astimezone().isoformat()`. The local offset is
  itself information — it tells future-you what wall-clock time the ingest
  happened in the zone where it actually ran. Don't normalize to UTC.

If you find yourself writing `strftime("%Y-%m-%dT%H:%M:%SZ")`, stop and
use `isoformat()` instead. The columns are `VARCHAR(40)`, wide enough for
the longest offset-suffixed form including microseconds.

## Auth (web API)

`src/download/claude_web.py` reads the `sessionKey` cookie out of
`latchkey curl -v` stderr and then issues the actual requests via
`curl_cffi` with `impersonate="chrome"` so Cloudflare's JA3 wall passes.
If the cookie is missing or expired, `latchkey auth set claude-ai` fixes
it; if Cloudflare still 403s, the IP/UA may be flagged — wait it out or
swap networks.
