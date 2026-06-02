# Fake test data — Star Trek: TNG edition

Hand-curated, obviously-fake input fixtures that mirror the on-disk shape of
the three real backup sources Sculptor ingests. These are checked in so that:

1. A fresh `git clone` can run the ingest end-to-end without anyone's real
   export.
2. Integration tests have known row counts, attachment shapes, and message
   threads to assert against.
3. Demos / screenshots produce content that is unmistakably fictional.

## Layout

```
fixtures/
├── anthropic_export/          source-of-truth shape for `provider: anthropic, kind: export_dir, provenance: export`
│   ├── users.json             list[Account]
│   ├── conversations.json     list[Conversation] (each with embedded chat_messages)
│   └── projects/<uuid>.json   per-project metadata
│
├── anthropic_api/             same parser, but provenance: api — adds _source, model, settings, platform, is_starred, current_leaf_message_uuid; richer block types (thinking, tool_use, tool_result)
│   ├── users.json
│   └── conversations.json
│
├── chatgpt_api/               `provider: openai, kind: chatgpt_api_dir, provenance: api`
│   ├── me.json                user record
│   ├── conversations.json     listing index
│   └── conversations/<id>.json   per-conversation node tree (message mapping with parent/children)
│
├── github_api/                event-store JSONL written by `download/github_web.py`.
│   └── <entity>/{created,updated}/events.jsonl
│   Entities: self_identity, pull_request, issue_comment, pr_review,
│   pr_review_comment. Repo: `enterprise-d/replicator-firmware`. Two PRs
│   (#42 Earl-Grey-temp merged; #43 holodeck-safety open) with threaded
│   review comments — #42 has a Riker → Picard reply pair anchored to
│   src/replicator/tea.c:17 to exercise `in_reply_to_id` tree-rebuilding.
│
├── gitlab_api/                event-store JSONL written by `download/gitlab_web.py`.
│   └── <entity>/{created,updated}/events.jsonl
│   Entities: self_identity, merge_request, discussion. Project:
│   `enterprise-d/holodeck`. Two MRs (!17 merged; !18 open) with a mix
│   of position-anchored discussions (line-level diff threads with
│   `position.new_path`/`new_line`) and free-form discussions
│   (`individual_note: true`) so consumers see both shapes.
│
└── notion_web/                event-store JSONL written by `download/notion_web.py`.
    └── <entity>/{created,updated}/events.jsonl
    Mirrors Notion's native recordMap tables 1:1 (one entity per
    `KNOWN_TABLES` entry in `notion_web.py`). Workspace:
    "USS Enterprise-D Operations". Covers all 14 Notion tables and
    every block `type` the downloader emits — see the variation table
    below.
```

None of github_api / gitlab_api / notion_web is wired into the ingest
pipeline yet — these are checked-in samples that mirror the on-disk
shape of the downloaders' output, available for future parser tests
and UI mocks.

## Coverage of source variation

Aim: at least one example of every shape we've seen in real backups.

| Variation                      | Where                                             |
|--------------------------------|---------------------------------------------------|
| Multiple accounts              | `anthropic_export/users.json` (Picard, La Forge)  |
| Conversation in a project      | `c0000001` (Holodeck Program Library)             |
| Conversation w/o project       | `c0000002`                                        |
| Multi-turn thread w/ parent IDs| every fixture                                     |
| `attachments[]` (extracted)    | `c0000002` message `20000001` (CSV telemetry)     |
| `files[]` (image)              | `c0000004` message `40000001`                     |
| Block type `text`              | all fixtures                                      |
| Block type `thinking`          | `c0000003` message `30000002`                     |
| Block type `tool_use`          | `c0000003` message `30000002`                     |
| Block type `tool_result`       | `c0000003` message `30000002`                     |
| ChatGPT `model_editable_context` (system) | `68fa0001` first message              |
| ChatGPT `text` content_type    | `68fa0001`                                        |
| ChatGPT `code` content_type    | `68fa0002` message `msg-fake-poly-0002`           |
| Starred / not starred          | `c0000003` (starred), `c0000004` (not)            |
| Multiple senders               | every conversation                                |
| Notion `space` + `team`        | `notion_web/notion_space/`, `notion_team/`        |
| Notion `notion_user` (7 crew)  | `notion_web/notion_user/`                         |
| Notion `space_view` / `space_user` / `user_root` / `user_settings` / `sidebar_section` | `notion_web/` (one each) |
| Notion block type `page` (nested) | `notion_block/` root + `Engineering Wiki` + `Warp Core Maintenance` subpage |
| Notion `collection_view_page`  | `b10cb10c-...0003` (Mission Logs DB)              |
| Notion inline `collection_view` block | `b10cb10c-...0006`                         |
| Notion `collection` w/ rich schema (title, status, person, date, multi_select, last_edited_time, button) | `notion_collection/` |
| Notion `collection_view` (board + table) | `notion_collection_view/` (two views)   |
| Notion DB rows (parent_table=collection) | `b10cb10c-...0100` / `...0101`         |
| Block types text/header/sub_header/sub_sub_header/bulleted_list/numbered_list/to_do/toggle/quote/callout/code/divider/image/file/embed/table/table_row/column_list/column | `notion_block/created/events.jsonl` |
| Rich text marks (bold/italic/code/link/user-mention/page-mention/date) | `b10cb10c-...0014`                |
| To-do checked + unchecked      | `b10cb10c-...001a` / `...001b`                    |
| Toggle with nested child       | `b10cb10c-...001c` → `...001d`                    |
| Discussion (unresolved + resolved) | `notion_discussion/` (two)                    |
| Comment thread (Riker → Picard reply pair) | `notion_comment/` (`c00ffee1` → `c00ffee2`) |
| Activity type `commented`      | `ac710001-...0001`                                |
| Activity type `edited-block-value` (before/after) | `ac710001-...0002`             |
| Notion `updated` stream (version bump) | `notion_block/updated/events.jsonl` (root page title changed v10→v11) |

## Star Trek: TNG dramatis personae

| Account UUID                              | Persona             |
|-------------------------------------------|---------------------|
| `00000001-1701-4d00-8000-000000000001`    | Jean-Luc Picard     |
| `00000002-1701-4d00-8000-000000000002`    | Geordi La Forge     |
| `00000003-1701-4d00-8000-000000000003`    | Beverly Crusher     |
| `user-FAKE0DATAANDROID0POSITRONIC1`       | Lt. Cmdr. Data (ChatGPT) |
| `00000004-1701-4d00-8000-000000000004`    | Lt. Worf            |
| `00000005-1701-4d00-8000-000000000005`    | Lt. Cmdr. Geordi La Forge (Notion) |
| `00000006-1701-4d00-8000-000000000006`    | Dr. Beverly Crusher (Notion) |
| `00000007-1701-4d00-8000-000000000007`    | Cmdr. Deanna Troi (Notion) |
| `5face1d0-1701-4d00-8000-000000000001`    | Workspace: USS Enterprise-D Operations (Notion space) |

UUIDs follow the pattern `XXXXXXXX-1701-4d00-8000-...` so they sort
predictably and scream "test data" in any debugger output.

## Cached "ingested" artifact

These source JSONs are also fed through the full ingest+render+dump
pipeline by a Bazel genrule, producing two byte-stable artifacts that
downstream tests (Rust, UI integration, Python consumers) can depend on
without re-running the pipeline:

```
bazelisk build //tests/fixtures:ingested_tng
# bazel-bin/tests/fixtures/ingested/backend_index.doltlite_db
# bazel-bin/tests/fixtures/ingested/qmd.tar
```

**Determinism**: the genrule pins `--now` to a fixed TNG-era timestamp,
the orchestrator inserts rows in primary-key order, and the tar
normalizes mtime/uid/gid. A clean rebuild produces byte-identical
outputs (verified). The trailing per-run `dolt_commit` lands a single
deterministic entry in `dolt_log` whose hash is stable given identical
inputs.

**Reading the doltlite_db.** It's a SQLite-shaped file. Consumers that
link doltlite (via `//third-party/doltlite:sqlite3`) get the full
version-control surface; consumers that link stock libsqlite3 get the
same table schemas without the `dolt_*` SQL functions. Either way, a
plain `SELECT` works:

```rust
let pool = sqlx::sqlite::SqlitePool::connect(
    &format!("sqlite://{}", db_path.display())
).await?;
let n: i64 = sqlx::query_scalar("SELECT count(*) FROM grid_rows")
    .fetch_one(&pool).await?;
```

**Constraints**: the genrule is fully hermetic. No host `dolt` install
is needed; the sync binary statically links doltlite via
`//third-party/doltlite:sqlite3`.

## Maintenance

These fixtures are **hand-edited** at every layer. When you change
any provider parser or `schemas/grid_rows.schema.json`:

1. Run `uv run pytest tests/test_fixtures.py` —
   the parser tests will break first if a new required field is added.
2. Update the relevant JSON files here with realistic-but-fake values
   that match the new shape.
3. If you add a new block type / content_type / attachment kind in
   real-life data, add an example to the table above and a fixture
   entry — so demos and integration tests cover it.
4. UI mocks live at `frankweiler/ui/tests/mocks/`. Keep them aligned
   with whatever the HTTP backend (`frankweiler/backend/http`) returns
   on the matching route.

**Golden snapshots.** `tests/test_snapshots.py` writes
plain-text goldens under `tests/__snapshots__/test_snapshots/`: one
`.sql` per table (the `CREATE TABLE` + sorted `INSERT`s for that
table), and one `.md` per rendered conversation. Custom syrupy
extensions in `tests/snapshot_extensions.py` make these viewable
on their own (no `.ambr` framing) — the `.md` files render as
real Markdown in any previewer. After an intentional fixture or
schema change:

```
bazelisk build //tests/fixtures:ingested_tng
uv run pytest tests/test_snapshots.py --snapshot-update
```

Review the diff under `tests/__snapshots__/` before committing.

There is no codegen / regen script for the source JSON — those
fixtures are not derived from anything. The trade-off is per-layer flexibility (e.g. the UI
mock can show a row that is not in the ingestion fixture) at the cost
of having to update each layer when the schema changes.
