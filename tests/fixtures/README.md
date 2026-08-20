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
├── notion_web/                event-store JSONL written by `download/notion_web.py`.
    └── <entity>/{created,updated}/events.jsonl
    Mirrors Notion's native recordMap tables 1:1 (one entity per
    `KNOWN_TABLES` entry in `notion_web.py`). Workspace:
    "USS Enterprise-D Operations". Covers all 14 Notion tables and
    every block `type` the downloader emits — see the variation table
    below.
│
└── (yolink lives with its provider:
    datalib/backend/etl/providers/yolink/tests/fixtures/yolink_tng/tng.json)
    A *spec*, not a capture: `yolink-make-fixture` expands it into a
    doltlite raw store, and the pipeline runs the source render-only.
    YoLink's downloader shells out to `curl` for signed-URL CSVs, so it
    has no playback tape to replay — see `run_sync_pipeline.py`'s
    `RENDER_ONLY`. Four Enterprise-D sensors, 288 five-minute samples
    each over 2369-04-14, deterministic values (sine + hash jitter, no
    RNG).
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
| Render-only source (no download step) | `yolink` — raw store seeded by `yolink-make-fixture` |
| Timeseries render (one page of plots) | `yolink/rendered_md/index.md` + `plots/*.html` |
| Non-SI unit converted at render | `sickbay_plasma_fridge` reports `temperature_f`; plots in °C |
| Two metrics of one quantity on split axes | `deck_12_water_main` — per-sample litres left, totalizer right |
| Relative `<iframe src>` in a rendered body | yolink plot embeds (rewritten to `/api/asset/…` by the UI) |

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

YoLink's fixture names compartments rather than people:

| Device                  | Kind                   | Reports                                   |
|-------------------------|------------------------|-------------------------------------------|
| `ten_forward_cooler`    | `temperature_humidity` | ~3 °C drinks chiller                       |
| `stasis_unit_alpha`     | `temperature_humidity` | ~-18.5 °C sample freezer                   |
| `sickbay_plasma_fridge` | `temperature_humidity` | ~44.6 **°F** — the unit-conversion case    |
| `deck_12_water_main`    | `watermeter`           | gallons, per-sample + lifetime totalizer   |

`sickbay_plasma_fridge` is the one fixture device that could not exist
upstream today: `download/mod.rs` pins each device kind to a fixed CSV
header and rejects a ℉ value under a ℃ header, so nothing writes a
`temperature_f` row. The fixture writes it directly, deliberately, so
the render side's ℉ → ℃ conversion has end-to-end coverage and the
"two devices, two units, one axis" case has a real example. Its
`fake_device_id` is likewise a stand-in — the real column holds half of
a per-device read credential, which never belongs in a fixture.

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
normalizes mtime/uid/gid.

This section used to claim a clean rebuild produces byte-identical
outputs, "(verified)". That is too strong. Measured 2026-08-20 by
running the full pipeline into two fresh roots more than a clock second
apart — do it that way, since two runs inside the same second can agree
by luck, which is how the original claim survived:

| | byte-stable? |
|---|---|
| `grid_rows` / `markdowns` / `edges` **contents** | **yes** |
| `backend_index.doltlite_db` **file** | no |
| rendered `.md` trees | yes, except notion + yolink (below) |
| `_render_cursor.json` | no |

**The table contents are the property worth relying on, and they hold.**
Dump them (`.mode json`, `SELECT * … ORDER BY 1`) and two independent
runs agree byte for byte. `//datalib/backend/core:fixture_db_snapshot_test`
is an insta snapshot of exactly that, which is why it can exist at all.

The **file** cannot be byte-stable, and no amount of `--now` pinning will
change that: doltlite's own "Initialize data repository" commit and the
shared layer's "schema: apply DDL" (`doltlite_raw::open`) both take the
wall clock, and commit hashes chain, so every later hash moves with them.
A source can pin its own commit — `yolink-make-fixture` passes
`--now` through to `dolt_commit --date` — but not those two. This is a
property of the store format, not a bug to fix here.

Two things leak that instability into files that otherwise would be
stable:

* **`_render_cursor.json`**, for every stanza: it records
  `last_render_at` from the local clock and `last_rendered_hash` from
  the store, both of which move. It is pipeline state that happens to
  live inside `rendered_md/`, so `tar_qmd.py` sweeps it into `qmd.tar`.
* **`yolink/rendered_md/index.md`**, in its "Store" section only: the
  page reports the store's HEAD and commit log, which *is* the content —
  a page describing a store legitimately changes when the store's
  identity does. Its `source_fingerprint` is deliberately **not**
  HEAD-derived (see `render/render.rs::compute_fingerprint`), which is
  what keeps the `markdowns` row stable.

Separately, and unrelated to any of the above: **notion's renderer emits
its blocks in a nondeterministic order.** Two runs produce the same
lines shuffled (`pages/b1d6e000-…-000000000001/index.md` and siblings).
It does not reach `grid_rows` — those come out identical — but it does
reach the markdown body the preview pane shows and qmd indexes, so
semantic-search results can differ run to run. Looks like a hash-map
iteration order leak. Not tracked anywhere else; noted here because it
is the kind of thing the old blanket "(verified)" claim was hiding.

Bazel keys its action cache on *inputs*, so the residue costs
reproducibility and cross-machine cache sharing, not day-to-day rebuild
churn.

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
4. UI mocks live at `datalib/ui/tests/mocks/`. Keep them aligned
   with whatever the HTTP backend (`datalib/backend/http`) returns
   on the matching route.

**Golden snapshots.** There are none, despite what this section used to
describe. `tests/test_snapshots.py`, `tests/snapshot_extensions.py`, and
`tests/__snapshots__/` do not exist in the tree (checked 2026-08-20);
`tests/` contains only `fixtures/`, and `bazelisk query //tests/...`
lists no snapshot target. `AGENTS.md` carries a second copy of the same
instruction, telling you to run `bazelisk test //tests:test_snapshots`.

What does assert on the fixture today: `:ingested_tng_test` (row counts,
per-provider coverage, three-run idempotence), each provider's own insta
snapshots under `datalib/backend/etl/providers/*/`, and
`//datalib/ui:e2e_test` against a materialized root.

There is no codegen / regen script for the source JSON — those
fixtures are not derived from anything. The trade-off is per-layer flexibility (e.g. the UI
mock can show a row that is not in the ingestion fixture) at the cost
of having to update each layer when the schema changes.
