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

## Auth (web API)

`scripts/sync_claude_web.py` reads the `sessionKey` cookie out of
`latchkey curl -v` stderr and then issues the actual requests via
`curl_cffi` with `impersonate="chrome"` so Cloudflare's JA3 wall passes.
If the cookie is missing or expired, `latchkey auth set claude-ai` fixes
it; if Cloudflare still 403s, the IP/UA may be flagged — wait it out or
swap networks.
