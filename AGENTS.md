# datalib — agent runbook

Quick references for AI/human contributors. See `docs/dev/grid_rows.md` for
the union-table architecture behind the grid.

## Repo layout

```
schemas/         cross-language source of truth. Each *.schema.json gets
                 codegen-emitted Rust (struct + const DDL) and TypeScript
                 artifacts. See docs/dev/grid_rows.md.
frankweiler/
  backend/     Rust workspace. ETL (extract/translate/load), HTTP API,
               qmd_indexer, Tauri backend. Per-provider crates under
               etl/providers/<p>/ each emit *.grid_rows.json sidecars
               that the shared Load step upserts into Dolt.
  ui/         Vue + AG Grid frontend.
tests/         goldens under tests/__snapshots__/ (Bazel-driven).
tests/fixtures/  TNG-themed source JSON + cached `ingested/` artifact.
docs/          dev/ architecture notes (dev/grid_rows.md, ...); user/ user-facing guides + config_examples/.
third-party/   vendored upstream code (see below).
```

## Vendored upstream: `third-party/qmd`

`third-party/qmd/` is a checked-in snapshot of
[`github.com/tobi/qmd`](https://github.com/tobi/qmd), pinned to **v2.1.0**.
It exists as a **reference for the qmd format** — we don't build or ship
from it; treat it as read-only documentation in code form. Our runtime
still consumes `@tobilu/qmd` via the registry pin in
`frankweiler/backend/qmd_indexer/` (which shells out to `npx -y
@tobilu/qmd@<version>`).

### Why we don't run from the vendored tree

It looks tempting to point the indexer at `third-party/qmd/bin/qmd` for
hermeticity, but the win is smaller than it looks and was deliberately
deferred:

- The vendored tree is source-only. Running it requires `pnpm install`
  (or `bun install`) **and** `pnpm run build` to produce `dist/`. The
  install step compiles native deps (`better-sqlite3`, `node-llama-cpp`,
  `sqlite-vec`, several `tree-sitter-*`) — that's the real network and
  build cost, not the qmd fetch itself.
- We'd still need node ≥22 and a working C toolchain on the host, so
  it's not actually hermetic in the Bazel sense — just "npx-free".
- `npx`'s cache already makes repeat invocations cheap.

If we want better isolation later, the more likely direction is to
**re-implement the bits of qmd we actually use** (indexing + retrieval
against our markdown tree) in Rust inside `frankweiler/backend/`, using
this vendored tree purely as the format/behavior reference. That keeps
runtime deps inside the Cargo workspace and avoids growing a node
toolchain footprint.

Pulled in via `git subtree add --squash`, so the upstream tree is one
squashed commit + a merge commit in our history (no full upstream log).
To bump the pin:

```sh
git subtree pull --prefix=third-party/qmd \
  https://github.com/tobi/qmd.git <new-tag> --squash
```

Do **not** edit files under `third-party/qmd/` — they will be overwritten
on the next pull. If you need local patches, layer them outside the
subtree and document why.

## The grid_rows union table

The Vue grid is backed by a single denormalized table, `grid_rows`,
populated at the end of every ingest from the authoritative per-provider
tables. The Rust backend (`frankweiler/backend/core/src/db.rs`) issues
*one* SELECT against `grid_rows` to render the grid — no per-provider
branches in the query path. Schema (column names, types, per-provider
mappings) lives in `schemas/grid_rows.schema.json`; codegen produces
matching Python/Rust/TypeScript types. See `docs/dev/grid_rows.md` for the
full architecture.

When you add or change a `grid_rows` column:

1. Edit `schemas/grid_rows.schema.json` (don't forget `x-mapping`).
2. Re-run codegen (see README).
3. Update each provider's `translate/grid_rows.rs` to populate the new
   column from that provider's parsed data.
4. Update the row mapper in `frankweiler/backend/core/src/dolt_repo.rs`
   to read it back, plus `SearchRow` in `search.rs` if the column reaches
   the API.
5. Re-bake the fixture: `bazelisk build //tests/fixtures:ingested_tng`.

## QMDs are write-only

Ingest renders QMD markdown files for human/Quarto consumption. The
backend serves those files **verbatim** (frontmatter stripped) at
`/api/chat/{uuid}` — it never parses them back. Structured fields
(name, account, project, channel, created_at, source_label) come from
`grid_rows` in Dolt. Per-message anchors used by the UI
(scroll-to-message, highlight) come from `<div id="m-{uuid}"
data-msg-index="N" class="msg msg--{provider}">` wrappers the renderer
emits in the body. If you find yourself writing a QMD parser in the
backend, stop — add the field to `grid_rows` instead.

## Feedback persistence (doltlite)

The backend opens the data root's `backend_index.doltlite_db` via
`sqlx::sqlite::SqlitePool` and wraps it in `DoltRepo`
(`frankweiler/backend/core/src/dolt_repo.rs`). doltlite is statically
linked into every Rust binary by `//third-party/doltlite:sqlite3`
(see `MODULE.bazel`); no host `dolt` install, no subprocess, no MySQL
client. The same pool serves reads and writes.

Every UUID-bearing UI surface has a "Feedback…" path. Right-click on
the grid emits `grid_cell` / `grid_row`; the search input emits
`filter_chip`; column headers emit `column_header`; the preview pane
cascades selection (`preview_selection`) → message (`preview_message`)
→ whole-thread (`page_header`); the page-header
`FeedbackButton` is `page_header`. The producer-side types and DOM
breadcrumb walker live in `frankweiler/ui/src/feedback/context.ts`;
the backend-side row + discriminated payload schema lives in
`schemas/feedback.schema.json` and is codegen'd into all three
languages.

Each `POST /api/feedback` inserts a row **and** runs
`SELECT dolt_commit('-Am', 'feedback: <uuid>')` on the same pooled
connection so the commit covers exactly the row we just wrote — no
chance of a concurrent writer's INSERT slipping into the same
`dolt_log` entry. Doltlite's working set is per-file, not
per-connection, so a sibling task on a different pool connection
sees the commit immediately.

Bazel stamps the binary with the git hash via
`tools/workspace_status.sh` (referenced from `.bazelrc`); cargo builds
get the same value from `frankweiler/backend/core/build.rs`. Read-back
of feedback rows is out of scope — query the doltlite_db directly with
any SQLite-shaped client.

## Git: prefer merges over rebases

When integrating remote changes into a local branch (e.g. `git pull` after
a rejected push), **prefer a merge commit over a rebase**. Rebasing
rewrites local commit hashes, which loses the "what actually happened"
history and can surprise other clones. A merge commit keeps both sides of
the history intact and is cheap to read with `git log --first-parent`.

In practice: `git pull` (default merge), not `git pull --rebase`. Force-
push is off the table on shared branches.

## Python deps: pyproject.toml → requirements.txt → Bazel

`uv` and Bazel read **different** files for Python deps:

- `uv run …` reads `pyproject.toml` + `uv.lock`.
- Bazel's `pip.parse` in `MODULE.bazel` reads `requirements.txt` (the
  hub is `@py_pip`, consumed via `requirement("…")` in BUILD files).

`requirements.txt` is a generated artifact — it must be regenerated
after any `pyproject.toml` dep change, or Bazel targets that try to
`requirement("newpkg")` will fail with
`no such package '@@…py_pip//newpkg': BUILD file not found`:

```sh
uv export --no-emit-project --no-emit-workspace --format requirements-txt -o requirements.txt
```

Then add `requirement("newpkg")` to the relevant `BUILD.bazel` `deps`.
A `uv run` smoke test won't catch a missing Bazel dep — the venv has it.
Run `bazelisk build //…` (or `//schemas:codegen`) to verify. Python is
now only used for schema codegen; everything else is Rust.

## Running tests

**"Build green" means `bazelisk test //...` passes — nothing less.** A
narrower invocation (`bazel build //some/subtree/...`,
`cargo test -p <crate>`, a single target's tests) is fine for inner-loop
iteration, but don't call the tree green based on one of those. If you
report "build green" without having run `bazelisk test //...`, say what
you actually ran instead.

**Coverage** uses `bazelisk coverage` with a one-shot wrapper that
captures Rust-subprocess hit counts too — see
[`docs/dev/coverage.md`](/docs/dev/coverage.md). The short form:

```bash
tools/run_coverage.sh //tests/fixtures:ingested_tng_test -- \
  //frankweiler/backend/sync:frankweiler_sync_bin \
  //frankweiler/backend/signal-backup:signal_make_fixture
```

**Default to `bazelisk test //...` for any "are tests passing?" question.**
It's the source of truth: it runs Rust, cross-language goldens, and the
Playwright e2e suite in one shot, the same way CI does. Bazel's action
cache makes re-runs cheap — unchanged targets are served from cache, so
iterating costs only what you actually touched. Reach for `cargo test`
/ `pnpm test` only for tight inner-loop iteration on a single language,
and confirm with `bazelisk test //...` before declaring done.

**Do not add `--test_tag_filters=-manual,-external` to this invocation.**
The canonical line is the bare `bazelisk test //...`. Filtering on
`-external` silently drops `//:precommit_test` (cargo fmt / clippy /
ruff / pyright / vue-tsc) and `//frankweiler/ui:e2e_test` (Playwright),
which lets fmt and UI regressions through. If a test is host- or
network-dependent it's tagged `requires-network` and/or `no-sandbox`,
which Bazel respects on its own — `external` is reserved for tests
that hit third-party services you don't want CI talking to. Prefer
`bazelisk` over `bazel` so the workspace's pinned Bazel version wins.

**Beware running snapshot tests outside Bazel**: those tests load
`bazel-bin/tests/fixtures/ingested/{dump.sql,qmd.tar}`, which is a Bazel
genrule output. Tools outside Bazel don't know how to rebuild it, so if
you change any ingest/render/schema code and re-run outside Bazel, you'll
diff fresh snapshots against a stale artifact and chase phantom failures.
Always run snapshot tests via `bazelisk test //tests:test_snapshots` (or
`//...`); Bazel rebuilds `//tests/fixtures:ingested_tng` first. Same
caveat applies to anything else that consumes a cached Bazel output.

### Updating insta snapshots (`.update` targets)

`bazel test` runs each action in a sandbox, so plain
`--test_env=INSTA_UPDATE=always` lands new `*.snap` files inside the
sandbox where they can't be reviewed. The standard fix is to invoke
the update via `bazel run` against a sibling `.update` target. Every
insta-using `rust_test` in this tree has one declared via the
`insta_update` macro in `//tools:insta.bzl`:

```bash
# Hermetic snapshot tests — no host prereqs.
bazel run //frankweiler/backend/core:fixture_db_snapshot_test.update
bazel run //frankweiler/backend/etl/providers/chatgpt:chatgpt_render.update
bazel run //frankweiler/backend/etl/providers/slack:slack_translate.update

# Live tests — need LATCHKEY_CURL on the host (same as cargo). Builds
# the shim once:
bazel build //frankweiler/backend/etl:latchkey_curl_shim
export LATCHKEY_CURL="$(pwd)/bazel-bin/frankweiler/backend/etl/latchkey_curl_shim"
bazel run //frankweiler/backend/etl/providers/anthropic:anthropic_live.update
```

The `manual_e2e_live_sync_golden` test is special: its config, source data,
and goldens live OUTSIDE this repo, found via `FRANKWEILER_MANUAL_E2E_DIR`.
Run it via the `run.sh` in that dir (`run.sh` to check, `run.sh --update` to
accept). See [`docs/dev/testing.md`](/docs/dev/testing.md).

The wrapper sets `INSTA_WORKSPACE_ROOT=$BUILD_WORKSPACE_DIRECTORY`,
which only exists under `bazel run` and resolves to the source tree
(not the sandbox), so insta writes — including brand-new `.snap`
files — land where `git status` will show them. Always review the
diff before committing.

When adding a new insta-using test, declare a sibling `.update`:

```python
load("//tools:insta.bzl", "insta_update")

rust_test(
    name = "my_render_test",
    data = [":tng_fixture"],
    env = {"MY_FIXTURE_DIR": "frankweiler/.../fixtures/my_api"},
    ...
)

insta_update(
    name = "my_render_test.update",
    test = ":my_render_test",
    test_args = ["--ignored"],  # only if the test is #[ignore]'d
    # `data` and `env` on rust_test DO NOT propagate through the
    # sibling sh_binary wrapper — mirror every fixture / env-var dep
    # here or `bazel run …update` will panic with "fixture not found".
    extra_data = [":tng_fixture"],
    extra_env = {"MY_FIXTURE_DIR": "frankweiler/.../fixtures/my_api"},
)
```

## Common commands

```bash
# Source of truth — run this before claiming tests pass
bazelisk test //...

# Rust-only inner loop (faster, narrower)
cargo test --manifest-path frankweiler/backend/Cargo.toml

# Rebuild the fixture ingest (dump.sql + qmd.tar)
bazelisk build //tests/fixtures:ingested_tng
```

## Provenance / "API wins"

Each parsed source carries an `"export"` / `"api"` tag. The merge step
in `frankweiler/backend/etl/providers/anthropic/src/translate/` applies
api-wins precedence: api-tagged rows beat export-tagged rows on the same
primary key, and api ingests own content blocks/attachments wholesale
per message (replacing any earlier export blocks for that message) so
trimmed blocks don't leave orphans. The union `grid_rows` table is the
only SQL artifact this produces; per-provider Dolt tables no longer
exist.

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

The Rust downloaders under `frankweiler/backend/etl/providers/*/src/extract/`
read the `sessionKey` cookie out of `latchkey curl -v` stderr and then
issue the actual requests via the `latchkey-curl-shim` so Cloudflare's
JA3 wall passes. If the cookie is missing or expired,
`latchkey auth set claude-ai` fixes it; if Cloudflare still 403s, the
IP/UA may be flagged — wait it out or swap networks.
