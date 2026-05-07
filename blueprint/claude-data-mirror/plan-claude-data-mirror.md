# Personal Data Mirror — v1 (Anthropic / claude.ai)

## Overview

A user-owned, on-disk mirror of personal data from cloud "walled garden" services. v1 ingests Anthropic claude.ai exports into a **Dolt** database and renders each conversation as a [QMD](https://github.com/tobi/qmd) file under a single user-configured root directory. Re-ingesting an updated export over the top of an existing repo produces a clean dolt commit/diff that captures exactly what changed since the previous export.

Future, deferred targets: OpenAI/ChatGPT history, Claude Code sessions, then messaging silos (Slack, Signal, WhatsApp, GChat).

## Goals (v1)

- Ingest a claude.ai data export (`users.json`, `conversations.json`, `projects/*.json`) into Dolt.
- Provide a single YAML config that names: 1) target data root, including dolt db and qmd files, 2) list configuring input sources to ingestt
- Spin up `dolt sql-server` from Python pointed at a Dolt repo inside the data root.
- Re-ingestion is idempotent on stable provider UUIDs; changed rows show up as dolt diffs.
- Render every conversation as a `.qmd` file at a stable path.
- CLI: `mirror ingest <export_path>` (which also re-renders QMD as a side effect).

## Non-goals (v1)

- No network sync (no internal-API scraping, no official-export polling).
- No OpenAI, no Claude Code session ingest, no messaging apps.
- No search index, no web/CLI search UI.
- No schedule/daemon mode.
- No archival of the raw export — dolt commits are the audit trail.

## Inputs

The program takes **one** input: a path to a YAML config (default `~/.config/claude-mirror/config.yaml`). The config names the data root and lists the sources to ingest. No source paths are passed on the command line.

```yaml
root: /Users/thad/data/mirror
dolt:
  port: 3306        # optional override

sources:
  - name: claude-personal       # human label, used in logs and dolt commit messages; must be unique
    provider: anthropic         # discriminator → selects parser + schema module
    kind: export_dir            # discriminator within provider (future: api, zip, …)
    path: ~/backups/claude      # provider/kind-specific config
    enabled: true               # default true; lets users keep stale sources around
```

The config is parsed by a **Pydantic** model (C1) with a discriminated union on `(provider, kind)`. Each provider/kind pair owns its own Pydantic source schema (e.g. `AnthropicExportDirSource`) so we get per-source field validation and good error messages. Adding a new provider adds a new Pydantic class registered into the union; nothing else in the config shape changes.

Example **Anthropic export directory** layout (the only `kind` shipped in v1):
```
<path>/users.json
<path>/conversations.json
<path>/projects/<project_uuid>.json
```

## Filesystem layout under `root`

```
<root>/
  .dolt-repo/                      # Dolt working dir (repo for all providers)
  anthropic/
    <account_uuid>/
      llm_chats/
        <conversation_uuid>__<slug>.qmd
```

Notes:
- Single Dolt repo, partitioning is by table columns (`provider`, `account_uuid`), not by directory.
- `<slug>` is a kebab-case sanitized `conversation.name` truncated to ~60 chars; the conversation UUID prefix guarantees uniqueness and stability across renames.
- No date in the path (a conversation can span months).

---

## Components

### C1. Config loader (`mirror.config`)
- **Responsibility:** Parse the YAML config into a Pydantic `Config` model, resolve `root` and all source `path`s to absolute paths (expanding `~`), ensure `root` exists, validate that source `name`s are unique, return the typed `Config`.
- **Interface:** `load_config(path: Path | None = None) -> Config`
- **Pydantic shape:**
  - `Config(root: Path, dolt: DoltConfig, sources: list[SourceConfig])`
  - `DoltConfig(port: int = 3306)`
  - `SourceConfig` is a **discriminated union** on `(provider, kind)`. v1 ships one variant: `AnthropicExportDirSource(name: str, provider: Literal["anthropic"], kind: Literal["export_dir"], path: Path, enabled: bool = True)`.
  - Adding a provider/kind adds a new Pydantic class to the union; the dispatch in C5 picks the right parser via the discriminator.
- **Notes:** Pydantic v2. Single source of truth for `root`, dolt port, and the source list.

### C2. Dolt service manager (`mirror.dolt_service`)
- **Responsibility:** Initialize the Dolt repo on first use (`dolt init`, run schema migrations), start `dolt sql-server` as a subprocess on the configured port, expose a SQLAlchemy/PyMySQL connection, shut down cleanly on exit.
- **Interface:**
  - `class DoltService(ContextManager)` — `__enter__` starts the server (or attaches to an existing one), `__exit__` stops it.
  - `connection() -> Connection` for SQL access.
  - `commit(message: str) -> str` — wraps `CALL DOLT_COMMIT(...)`, returns commit hash.
- **Notes:** Detect a running server on the port and attach instead of relaunching. Capture stdout/stderr to a logfile inside `<root>/.dolt-repo/logs/`.

### C3. Schema (`mirror.providers.anthropic.schema`)
- **Responsibility:** Create the Anthropic-specific relational tables on first run; idempotent migrations. Each provider owns its own schema module and its own table namespace — table names are prefixed `anthropic_` so future providers (`openai_*`, `claude_code_*`, `slack_*`, …) can evolve independent shapes without colliding.
- **Tables (every row keeps a `raw_json` column for round-trip fidelity):**
  - `anthropic_accounts(account_uuid PK, email, full_name, raw_json, first_seen_at, last_seen_at)`
  - `anthropic_projects(account_uuid, project_uuid PK, name, description, is_starter, created_at, updated_at, raw_json, last_seen_at)`
  - `anthropic_conversations(account_uuid, conversation_uuid PK, project_uuid NULL, name, summary, created_at, updated_at, raw_json, last_seen_at)`
  - `anthropic_messages(conversation_uuid, message_uuid PK, parent_message_uuid, sender, text, created_at, updated_at, raw_json, last_seen_at)`
  - `anthropic_content_blocks(message_uuid, block_index, type, text, start_timestamp, stop_timestamp, raw_json)` — composite PK `(message_uuid, block_index)`
  - `anthropic_attachments(message_uuid, attachment_index, kind, raw_json)` — covers both `attachments[]` and `files[]` from the export, distinguished by `kind`; composite PK `(message_uuid, attachment_index)`
- **Notes:**
  - The `provider` column is no longer needed on each row — provider identity is encoded in the table name. Cross-provider queries use UNION over per-provider tables (or views built later).
  - All `raw_json` columns are `JSON` (or `LONGTEXT` if Dolt's JSON support is incomplete). `last_seen_at` is the timestamp of the most recent ingest run that observed the row — used for tombstone detection (C5).
  - Future providers add their own `mirror.providers.<name>.schema` module; a top-level `mirror.schema` thin wrapper invokes whichever provider schemas are enabled by config.

### C4. Anthropic export parser (`mirror.providers.anthropic.parse`)
- **Responsibility:** Read `users.json`, `conversations.json`, and `projects/*.json`; yield typed records ready for upsert. Pure function, no I/O beyond reading the export directory.
- **Interface:**
  - `parse_export(export_dir: Path) -> ParsedExport` where `ParsedExport` has `accounts`, `projects`, `conversations`, `messages`, `content_blocks`, `attachments` iterables.
- **Notes:** Validates the bundle's basic shape; tolerates unknown fields (preserved verbatim in `raw_json`).

### C5. Ingest pipeline (`mirror.ingest`)
- **Responsibility:** Drive a full ingest across **every enabled source** in the config: connect to Dolt, dispatch each source to its provider/kind-specific parser + upserter, mark `last_seen_at = <ingest_started_at>`, then commit. Rows whose `last_seen_at` is older than the current run (within the relevant provider account scope) are tombstone candidates — v1 default leaves them in place (claude.ai exports don't always include archived items); a `deleted_at` column or view can be added later.
- **Interface:**
  - `ingest(config: Config) -> IngestSummary` — no extra args; everything comes from the config.
  - `IngestSummary`: per-source rows inserted/updated/unchanged per table, dolt commit hash, parsed counts.
- **Provider dispatch:** `mirror.providers` maintains a registry mapping `(provider, kind)` → `SourceIngester` callable. The Pydantic discriminator chosen in C1 picks the right entry. v1 registers `("anthropic", "export_dir") → mirror.providers.anthropic.ingest_export_dir`.
- **Steps:**
  1. Start `DoltService`; ensure each enabled provider's schema (C3) is applied.
  2. For each enabled `SourceConfig`:
     a. Parse via the provider's parser (C4 for Anthropic).
     b. UPSERT in dependency order: e.g. `anthropic_accounts` → `_projects` → `_conversations` → `_messages` → `_content_blocks` → `_attachments`.
     c. Update `last_seen_at` on every observed row to the run timestamp.
  3. `CALL DOLT_ADD('-A')` then `CALL DOLT_COMMIT('-m', f"ingest {','.join(source_names)} {ingest_started_at}")`. Returns the commit hash; skip the commit if there are no changes.
  4. Trigger render (C7) for the affected provider/account scopes.

### C6. QMD renderer (`mirror.render.qmd`)
- **Responsibility:** Render one conversation to one `.qmd` file given the rows from Dolt.
- **Interface:** `render_conversation(conn, conversation_uuid: str, root: Path) -> Path`
- **Notes:**
  - Output path: `<root>/anthropic/<account_uuid>/llm_chats/<conversation_uuid>__<slug>.qmd`
  - Frontmatter: YAML block with `uuid`, `name`, `created_at`, `updated_at`, `account_uuid`, `project_uuid`, `provider: anthropic`.
  - Body: each message becomes a section with the sender (`human`/`assistant`) as the heading; content blocks rendered in order, preserving text verbatim. Attachments listed at the end of the relevant message section.
  - Output is deterministic — re-rendering an unchanged conversation produces a byte-identical file.

### C7. Render driver (`mirror.render`)
- **Responsibility:** Iterate conversations in scope and call C6 for each. Garbage-collect QMD files whose conversation UUIDs no longer exist in Dolt.
- **Interface:** `render_all(config: Config, scope: RenderScope = RenderScope.ALL) -> RenderSummary`
- **Notes:** Slug changes on rename: detect by querying for files matching `<conversation_uuid>__*.qmd` and renaming if the slug has changed.

### C8. CLI (`mirror.cli`)
- **Responsibility:** Single Typer/Click app exposing `mirror ingest`.
- **Commands (v1):**
  - `mirror ingest [--config PATH]` — loads the config (C1), runs C5 over every enabled source, then runs C7. Prints `IngestSummary` + `RenderSummary`. No positional arg; the config is the single source of input definitions.
- **Notes:** No other subcommands in v1; `mirror render`, `mirror status`, `mirror dolt …` deferred.

---

## Data flow (one ingest run)

```
YAML config ──► Config (C1, Pydantic) ──► [SourceConfig, …]
                   │
                   ▼
           DoltService (C2) ── ensures Schema (C3) per enabled provider
                   │
                   ▼
           Ingest pipeline (C5) ── for each source: dispatch on (provider, kind) ──► Parser (C4) ──► UPSERTs ──► Dolt tables
                   │
                   ▼
           dolt commit ──► commit hash
                   │
                   ▼
           Render driver (C7) ── per conv ──► QMD renderer (C6) ──► .qmd files on disk
```

## Open questions / explicit deferrals

- **Tombstone semantics.** Claude exports may omit archived conversations. Decision deferred: in v1 we keep stale rows (with old `last_seen_at`) and a SQL view surfaces "missing in last export". A `deleted_at` column can be added later if needed.
- **JSON column type.** Dolt's `JSON` support is improving; verify at implementation time whether to use `JSON` or `LONGTEXT`. Pick during the Schema task.
- **Slug stability on rename.** First implementation: rename file when slug changes; alternative is a redirect index. The UUID prefix in the filename keeps links findable either way.
- **Dolt port collisions.** v1 uses a fixed default with config override. Auto-pick free port deferred.

---

## Task list

### Phase 1 — scaffolding
1. Initialize Python project (`pyproject.toml`, package layout `mirror/`, formatter config).
2. Add dependencies: `pyyaml`, `pydantic>=2`, `pymysql` or `sqlalchemy`, `typer`/`click`, `pytest`.
3. Implement **C1 config loader** as a Pydantic v2 model with a discriminated `sources` union; default config search path; unit test with a temp YAML file (valid + invalid + unknown-provider cases).

### Phase 2 — Dolt up and running
4. Add a Dolt install probe (`dolt version`) and a clear error if missing.
5. Implement **C2 DoltService**: `dolt init` if needed, start `dolt sql-server`, attach over PyMySQL, clean shutdown.
6. Smoke test: spin up the service in a temp root, run `SELECT 1`, shut down.

### Phase 3 — schema
7. Implement **C3 schema** module with idempotent `CREATE TABLE IF NOT EXISTS …` for every table listed above.
8. Test: schema applies cleanly twice in a row with no diff.

### Phase 4 — parser
9. Implement **C4 parser** against the real export at `~/backups/claude/`.
10. Tests: golden snapshot of parsed counts (accounts/projects/conversations/messages/content_blocks/attachments) for the sample export.

### Phase 5 — ingest
11. Implement **C5 ingest** with UPSERTs in dependency order and `last_seen_at` bumps.
12. Implement dolt commit step (skip when no changes).
13. Test: ingest twice; second run produces zero dolt diff. Mutate one message in the source JSON, re-ingest, assert the dolt diff contains exactly that row.

### Phase 6 — QMD render
14. Implement **C6 conversation renderer** with deterministic output.
15. Implement **C7 render driver** + slug-rename + GC of orphaned `.qmd` files.
16. Test: render the sample export to a temp dir, assert idempotent re-render is byte-identical.

### Phase 7 — CLI + glue
17. Implement **C8** `mirror ingest [--config PATH]` wiring C1 → C5 → C7. No positional source arg — sources come from config.
18. End-to-end test: write a config pointing at `~/backups/claude` into a temp root, run `mirror ingest --config <tmp>`.
19. README with the YAML config example and a one-line quickstart.

### Phase 8 — finish
20. Manual smoke run against the user's real export; eyeball a handful of `.qmd` files and the dolt log.

---

## Acceptance criteria

- Given a config that lists one Anthropic `export_dir` source pointing at `~/backups/claude`, `mirror ingest --config <path>` against an empty root produces:
  - A working Dolt repo under `<root>/.dolt-repo/`.
  - One row per account/project/conversation/message/content_block/attachment from the export.
  - One `<conversation_uuid>__<slug>.qmd` file per conversation under `anthropic/<account_uuid>/llm_chats/`.
  - A single `dolt commit` whose message names the source(s) and the ingest timestamp.
- Re-running the same command with the same config + export produces:
  - No dolt diff (no new commit).
  - No QMD files modified on disk.
- Re-running with a modified export produces a dolt commit whose diff contains exactly the changed rows, and re-renders only the affected `.qmd` files.
