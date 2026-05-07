# claude-mirror — agent runbook

Quick references for AI/human contributors. See `CLAUDE_WEB_SCHEMA.md` for
the conceptual model and field-level diffs between the two transports.

## Common commands

```bash
# Run unit + smoke tests
uv run pytest

# Ingest configured sources into the Dolt repo (per ~/.config/claude-mirror/config.yaml)
uv run python -m claude_mirror

# Incrementally fetch new conversations from the claude.ai web API
uv run python scripts/sync_claude_web.py fetch

# Verify the API mirror matches the bulk export on overlapping conversations
uv run python scripts/sync_claude_web.py verify --port 3307

# Both, back-to-back
uv run python scripts/sync_claude_web.py sync --port 3307
```

`verify` spins up a temp Dolt repo, ingests both `~/backups/claude/`
(source=export) and `~/backups/claude_api/` (source=api) into separate
commits, then runs `dolt_diff_<table>` over the overlap. It classifies
diffs as **cosmetic** (`raw_json`, `last_seen_at`, `first_seen_at`) vs.
**real** (everything else). Use `--keep` to leave the temp repo around
for manual `dolt sql` poking.

## Provenance / "API wins"

Every row in the `anthropic_*` tables has a `source` column. API rows are
authoritative: a later export ingest will not clobber them, and an API
ingest deletes-and-reinserts content blocks/attachments per message so
trimmed blocks don't leave orphans. See `CLAUDE_WEB_SCHEMA.md` for the
SQL pattern.

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

`scripts/sync_claude_web.py` reads the `sessionKey` cookie out of
`latchkey curl -v` stderr and then issues the actual requests via
`curl_cffi` with `impersonate="chrome"` so Cloudflare's JA3 wall passes.
If the cookie is missing or expired, `latchkey auth set claude-ai` fixes
it; if Cloudflare still 403s, the IP/UA may be flagged — wait it out or
swap networks.
