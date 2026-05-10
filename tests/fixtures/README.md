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
└── chatgpt_api/               `provider: openai, kind: chatgpt_api_dir, provenance: api`
    ├── me.json                user record
    ├── conversations.json     listing index
    └── conversations/<id>.json   per-conversation node tree (message mapping with parent/children)
```

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

## Star Trek: TNG dramatis personae

| Account UUID                              | Persona             |
|-------------------------------------------|---------------------|
| `00000001-1701-4d00-8000-000000000001`    | Jean-Luc Picard     |
| `00000002-1701-4d00-8000-000000000002`    | Geordi La Forge     |
| `00000003-1701-4d00-8000-000000000003`    | Beverly Crusher     |
| `user-FAKE0DATAANDROID0POSITRONIC1`       | Lt. Cmdr. Data (ChatGPT) |

UUIDs follow the pattern `XXXXXXXX-1701-4d00-8000-...` so they sort
predictably and scream "test data" in any debugger output.

## Cached "ingested" artifact

These source JSONs are also fed through the full ingest+render+dump
pipeline by a Bazel genrule, producing two byte-stable artifacts that
downstream tests (Rust, UI integration, Python consumers) can depend on
without re-running the pipeline:

```
bazelisk build //tests/fixtures:ingested_tng
# bazel-bin/tests/fixtures/ingested/dump.sql
# bazel-bin/tests/fixtures/ingested/qmd.tar
```

**Determinism**: the genrule pins `--now` to a fixed TNG-era timestamp,
the dumper sorts rows by primary key and canonicalizes JSON, and the
tar normalizes mtime/uid/gid. A clean rebuild produces byte-identical
outputs (verified). Dolt commit hashes themselves are non-deterministic
and intentionally **not** included in either artifact \u2014 the SQL dump is
keyed on provider UUIDs, which are stable.

**Why not the live `.dolt/` directory?** Dolt's chunk store / journal
files differ across runs (and across Dolt versions). The SQL dump is the
canonical projection of the data; load it into a fresh Dolt (or any
MySQL-compatible engine) to reconstruct the live DB on demand.

**Reading the dump without Dolt.** The DDL is the portable subset shared
by Dolt, MySQL, and SQLite, so consumers that only need to *query*
ingested data can skip the Dolt subprocess entirely:

```python
from ingest.sqlite_load import load_dump_into_memory
conn = load_dump_into_memory(Path(".../dump.sql"))   # in-memory SQLite
conn.execute("SELECT full_name FROM anthropic_accounts").fetchall()
```

Loading into `:memory:` is sub-millisecond and hermetic — preferred for
unit tests over spinning up Dolt. Use Dolt only when you need its
write/versioning semantics.

**Constraints**: the genrule requires `dolt` on PATH and is tagged
`requires-dolt`, `no-remote`, `no-sandbox`. It runs locally only \u2014 not
on RBE.

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
