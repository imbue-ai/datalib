# Config schema redesign — flat `type:`, derived `managed`, combined Notion modes

## Overview

The user config (`~/.config/datalib/config.yaml`, plus the in-repo
`configs/thad_dev.yaml`, `ingest_configs/thad_dev.yaml`, and the fixture
generator in `tests/fixtures/build_ingested.py`) carries a handful of
fields that are redundant, misnamed, or awkward to live with. This plan
collapses them. Hard cutover — no aliases, no users to migrate.

Concretely, the shape goes from:

```yaml
root: ~/datalib.thad
sources:
  - name: claude-api
    provider: anthropic
    kind: export_dir
    path: ~/backups/claude_api
    provenance: api
    managed: true
    sync:
      kind: claude_web
      overlap: 50
  - name: notion-official
    provider: notion
    kind: notion_official_dir
    path: ~/backups/notion
    managed: true
    sync:
      kind: notion_official
      inbox: true            # XOR with subtree
```

to:

```yaml
data_root: ~/datalib.thad
sources:
  - name: claude-api
    type: claude_api          # collapses provider+kind+provenance
    # input_path defaults to ${data_root}/raw/${name}
    sync:                     # presence => managed
      overlap: 50             # no `kind:` — implied by `type`
  - name: notion-api
    type: notion_api          # renamed from notion_official
    sync:
      inbox:                  # nested sub-blocks; either/both can be set
        enabled: true
        types: ["mention"]
      subtrees:
        pages: ["id-1", "id-2"]
```

## Goals

- One discriminator field per source (`type:`); think of it as the name of
  a constructor and the rest of the source dict as its arguments.
- One discriminator field per sync block (none — implied by source `type`).
- `data_root` replaces `root`. `input_path` replaces `path`. `input_path`
  defaults to `${data_root}/raw/${name}` (which is already where the
  worker's downloader writes — see `src/ingest/run_source.py:141`).
- `managed` field is removed; managed-ness is `sync is not None`. Pause a
  source without losing its block via `enabled: false`. `enabled: false`
  makes the source invisible to both worker and ingester.
- The Notion downloader supports inbox **and** a list of subtree pages in
  a single invocation. Both are independently activatable. All downloaded
  pages land in one flat namespace under `input_path` — a subtree fetch
  that re-discovers a page already pulled from the inbox is a no-op write.
- Migrate every in-repo config and the schema in one PR. Delete
  `ingest_configs/` entirely; `configs/` is the new home.

## Non-goals

- No aliases / dual-name acceptance for old fields. Hard cutover.
- No new providers, no new downloader features beyond Notion's
  inbox+subtree combination.
- No change to the merge precedence semantics — `claude_api` rows still
  win over `claude_export` rows in `providers/anthropic/ingest.py`; only
  the field name carrying that signal changes.
- No change to `dolt:` block, `concurrency:`, or other top-level config
  keys.
- No change to the latchkey-based credential flow.

## External contract: the new YAML

### Top-level

```yaml
data_root: <path>              # was: root
dolt:                          # unchanged
  port: 3306
sources: [...]
```

### Per-source

```yaml
- name: <unique-string>        # unchanged
  type: <type-literal>         # NEW: collapses provider+kind+provenance
  input_path: <path>           # OPTIONAL; default ${data_root}/raw/${name}
  enabled: true                # unchanged; false hides from worker + ingester
  sync: {...}                  # OPTIONAL; presence => managed
```

`type:` values (closed set, Pydantic discriminator):

| `type` | Replaces (provider, kind, provenance) | Has downloader? |
| --- | --- | --- |
| `claude_export` | (anthropic, export_dir, export) | No (manual unzip) |
| `claude_api` | (anthropic, export_dir, api) | Yes (`claude_web`) |
| `chatgpt_api` | (openai, chatgpt_api_dir, api) | Yes (`chatgpt_web`) |
| `slack_api` | (slack, slack_api_dir, —) | Yes (`slack_web`) |
| `github_api` | (github, github_api_dir, —) | Yes (`github_web`) |
| `gitlab_api` | (gitlab, gitlab_api_dir, —) | Yes (`gitlab_web`) |
| `notion_api` | (notion, notion_official_dir, —) | Yes (`notion_official`) |

`enabled: false` is read by both `worker.runner` and
`ingest.ingest.run` — the source is skipped end-to-end. Already-ingested
rows remain in Dolt; nothing actively purges them, but they also won't be
refreshed and the source will not appear in any new run summary.

### Per-source `sync:` block

No `kind:` discriminator. The schema is selected by the source's `type:`.
Knobs unchanged except where noted:

- `claude_api.sync`: `overlap`, `refresh_window_days`
- `chatgpt_api.sync`: `max_pages`, `limit`, `sleep_between`, `refresh_window_days`
- `slack_api.sync`: `channels`, `since`, `all_channels`, `media`, `refresh_window_days`
- `github_api.sync`: `max_prs`, `refresh_window_days`
- `gitlab_api.sync`: `max_mrs`, `refresh_window_days`
- `notion_api.sync` (NEW shape):
  ```yaml
  sync:
    inbox:                    # OPTIONAL sub-block
      enabled: true
      types: ["mention", "comment"]   # was: inbox_types
      notification_page_size: 100
      max_notification_pages: 5
      space: "<workspace-id>"
    subtrees:                 # OPTIONAL sub-block
      pages: ["id-1", "id-2"]
      max_pages: 1000
    refresh_window_days: 7
  ```
  At least one of `inbox.enabled: true` or a non-empty `subtrees.pages`
  must be set, else the sync block is rejected at load time (replaces
  the old `_one_mode` XOR validator at `config.py:73-80`).

### Defaulting rules

- `input_path` omitted → `${data_root}/raw/${name}`. The downloader
  writes per-run timestamp subdirs underneath (existing behavior in
  `run_source.py:141`). The ingester scans the whole parent directory.
- `enabled` omitted → `true`.
- `sync` omitted → source is unmanaged (ingest-only).
- For `claude_export` specifically: `sync` MUST be absent (validator).
  There is no downloader, so a `sync:` block would be nonsense.

## Affected components

### Schema — `src/ingest/config.py`

- Replace `_SourceBase` + the seven per-provider source classes with
  seven `type`-discriminated source models. Drop `provider`, `kind`,
  `provenance`, `managed` from the YAML surface.
- Add a `provenance` `@property` on `ClaudeApiSource` returning `"api"`
  and on `ClaudeExportSource` returning `"export"` so existing merge code
  in `providers/anthropic/ingest.py` keeps working without restructure.
- Rename `Config.root` → `Config.data_root`. Update all `cfg.root` call
  sites (grep shows: `src/ingest/ingest.py`, `src/ingest/run_source.py`,
  `src/worker/runner.py`, anywhere the DoltServer is wired).
- Add `input_path` property: returns the explicit field if set, else
  `data_root / "raw" / name`. Call sites that read `src.path` switch to
  `src.input_path`.
- Replace `NotionOfficialSync` with the new sub-block shape. Add a
  `model_validator` that rejects `inbox.enabled=False` + empty
  `subtrees.pages`.
- Validator: reject `claude_export` with a `sync:` block.

### Sync→argv translator — `src/ingest/run_source.py`

- Drop the `KIND_TO_MODULE` dict's reliance on `sync.kind`. Replace with a
  dispatch keyed on the source `type` (the caller already has the source
  in hand via `resolve()`).
- `sync_to_argv` becomes per-source-`type` rather than per-sync-`kind`.
  Each branch reads its own (no-discriminator) sync schema.
- Notion branch: emit `--inbox` / `--inbox-types` / `--subtree-page <id>`
  (repeatable) etc. for whichever sub-blocks are present. Drop the
  single-`--subtree` flag.

### Notion downloader — `src/download/notion_official.py`

- Accept any combination of `--inbox` and zero-or-more `--subtree-page`
  args. Single auth setup. Single output dir. Pages are written by ID, so
  inbox-discovered pages and subtree-discovered pages collide harmlessly
  (later writer wins; both should produce identical content).
- Drop the existing "exactly one mode" CLI guard. Add a "at least one
  source flag (`--inbox` or `--subtree-page`) required" guard.

### Ingest — `src/ingest/ingest.py`

- Update the `isinstance` dispatch (lines 121-141) for the new source
  classes. The body of each branch is unchanged except `src.path` →
  `src.input_path` and `src.provenance` → the new property.
- `cfg.root` → `cfg.data_root` (one site, line 111).
- Skip sources where `enabled` is false (already partially via
  `enabled_sources` property; verify both worker and ingest use it).

### Worker — `src/worker/runner.py`

- `cfg.root` → `cfg.data_root` for the out_dir computation.
- `src.managed` references (worker uses this to decide whether to enqueue
  jobs for the source) → `src.sync is not None`.

### Configs in the repo

- `configs/thad_dev.yaml` — rewrite in new schema (this is the file the
  user is staging for `~/.config/datalib/config.yaml`).
- `ingest_configs/thad_dev.yaml` — delete the directory.
- `tests/fixtures/build_ingested.py` — rewrite the inline YAML at lines
  54-90 in the new schema. This is what generates the byte-stable
  `dump.sql` consumed by the e2e suite and dev_tng, so the change must
  preserve every value semantically.

### Docs/refs to touch

- README.md mentions of `root:` / `config.root` (grep).
- `frankweiler/dev_tng.sh` writes a config.yaml — uses `root:`; switch to
  `data_root:`.
- `frankweiler/ui/tests/e2e/prepare-fixture.cjs` — same.
- `AGENTS.md` and any other docs that show the YAML shape.

## Migration / verification strategy

Hard cutover means breakage is loud. The mitigations are:

1. Run `bazelisk test //...` after each commit. The fixture generator
   feeds `dump.sql`, which feeds e2e_test and dev_tng — schema drift
   breaks them.
2. Hand-verify `configs/thad_dev.yaml` loads via `load_config()` (the
   smoke test we used last commit: `uv run python -c
   "from src.ingest.config import load_config; load_config('configs/thad_dev.yaml')"`).
3. Hand-verify the Notion downloader's combined-mode CLI by running
   `python -m download.notion_official --inbox --subtree-page <id> --out-dir /tmp/notion-test --dry-run` (add `--dry-run` if not present).

## Task list (ordered, each = one commit)

1. **Schema rewrite** in `src/ingest/config.py`: new `type:`-discriminated
   source classes, `data_root` rename, `input_path` property with default,
   new Notion sync sub-block shape, removed `managed` field. Update
   `provenance` to a derived `@property` on the two Anthropic source
   classes. Add validator rejecting `claude_export` + `sync`. Add
   validator requiring at least one of inbox/subtrees on `notion_api`.
2. **Call-site sweep**: `cfg.root` → `cfg.data_root`; `src.path` →
   `src.input_path`; `src.provider`/`src.kind` references → use
   `isinstance` or `type(src).__name__` as appropriate. Touches
   `src/ingest/ingest.py`, `src/ingest/run_source.py`,
   `src/worker/runner.py`, plus any logging/summary code that printed
   `provider`/`kind`. After this commit, the test suite must build (it
   will fail on YAML inputs until step 4).
3. **Notion downloader combined mode**: in `src/download/notion_official.py`,
   accept `--inbox` together with one-or-more `--subtree-page <id>`;
   drop the XOR guard; flatten output to a single directory keyed by
   page ID. Update its unit tests if any exist.
4. **`sync_to_argv` rewrite** in `src/ingest/run_source.py`: dispatch on
   source `type` (not sync.kind), emit the new Notion flag set, build
   `out_dir` from `data_root`. Update `resolve()` to use `input_path`
   for the parent of the per-run timestamp dir, so user overrides of
   `input_path` flow through to where downloads land.
5. **Fixture generator**: rewrite the YAML template in
   `tests/fixtures/build_ingested.py:54-90` in the new schema. The
   resulting `dump.sql` and `qmd.tar` should be byte-identical to
   pre-change (so the e2e test cache stays warm — we're not changing
   data, only the config that describes it).
6. **In-repo configs**: rewrite `configs/thad_dev.yaml` (this is the user's
   staging file — already in the working tree with FIXME comments) and
   `git rm -r ingest_configs/`. Verify with the `load_config` smoke test.
7. **dev/e2e harness configs**: update `frankweiler/dev_tng.sh` and
   `frankweiler/ui/tests/e2e/prepare-fixture.cjs` to emit `data_root:`
   instead of `root:`. Run `bazelisk test //frankweiler/ui:e2e_test` to
   confirm the harness still works end-to-end.
8. **Docs**: grep README.md, AGENTS.md, blueprint/*.md, src/download/*.md
   for `root:`, `path:`, `kind:`, `provenance:`, `managed:` in config
   examples and update them. Drop any `notion_official` references to the
   old single-mode XOR.
9. **Full bazel test pass + commit + push**. Hand-verify `configs/thad_dev.yaml`
   loads, copy it to `~/.config/datalib/config.yaml`, run one
   real-world sync (e.g. `python -m ingest.run_source slack-api --dry-run`)
   to confirm argv generation is sane.

## Risks and open questions

- **Anthropic merge precedence is the one piece of bespoke logic that
  doesn't fall out of the type-as-constructor model.** It survives via
  the `provenance` `@property`. If we ever want a third Anthropic shape
  (export+api+something), the property approach generalizes; a switch
  inside `merge_anthropic` would too. Either is fine, no decision needed
  now.
- **`input_path` outside `data_root`** (e.g. `~/backups/claude` for the
  existing claude-export source) — supported, treated as
  read-only-from-the-ingester's-perspective. The worker would never
  write there because that source has no `sync:` block.
- **Step-2 atomic-commit feasibility**: renaming `root` → `data_root`
  while step 1 already redefined the model means commit 2 only compiles
  because commit 1 already shipped the new field. If precommit checks
  block step 1 alone (because no callers exist for the new fields yet),
  collapse steps 1+2 into one commit. Verify on first attempt.
- **Notion subtree fetch + inbox in one process** — the existing
  downloader code is structured around one entry point. The "single
  invocation, two modes" requirement may require modest refactor inside
  `notion_official.py` (split the inbox-walk and subtree-walk into
  helpers, call both from `main`). Worth a small spike before committing
  step 3.
