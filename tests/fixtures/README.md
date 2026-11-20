# The TNG corpus

Datalib's test data is a made-up mirror of the crew of the USS
Enterprise-D, from *Star Trek: The Next Generation*. Every source
Datalib reads has a fixture in that world: Picard's email and Codex
sessions, Data's ChatGPT account, the ship's air-quality monitors. Together they are the **TNG corpus**.

It exists so that:

1. A fresh clone can run every source end to end without anyone's real
   data. Nothing from a real mirror goes into the repo
   ([`AGENTS.md`](../../AGENTS.md) §"Real data stays out of the repo"),
   so a test, a screenshot or a demo uses this instead.
2. Tests have known rows, threads and attachments to assert against.
3. Anything drawn from it is plainly fictional.

Each fixture lives beside the provider it tests, under
`datalib/backend/etl/providers/<provider>/tests/fixtures/`. This
directory holds what turns them into one synced library: the pipeline
driver, the generators, and the tests over the result.

## Start here

| to… | run |
|---|---|
| see the corpus in the app | `bazelisk run //datalib:dev_tng` ([`first_time_dev.md`](../../docs/dev/first_time_dev.md) §"Quickest first run") |
| build the synced corpus | `bazelisk build //tests/fixtures:ingested_tng` |
| sync it from scratch and check it | `bazelisk test //tests/fixtures:ingested_tng_test` |

`dev_tng` builds a data root from the corpus in a temporary directory
and opens the app on it. Browsing and search work. Syncing a source
does not: the root holds the rendered documents but not the raw stores
they came from.

## Conventions

- **Picard holds the account** in most sources. His email address is
  `picard@enterprise.starfleet`, his Claude Code projects are under
  `/Users/picard/`, and he is usually "me" in a chat. Some sources
  belong to another officer (Data's ChatGPT account) or to the ship
  (YoLink's sensors, the AirVisual monitors).
- **"Now" is `2369-04-15T00:00:00+00:00`.** The pipeline pins its
  clock there, so anything measured from now (a "since" window, a
  stamp the pipeline writes) lands on the same date every build.
- **Ids say they are fake.** Hand-written UUIDs follow
  `XXXXXXXX-1701-4d00-8000-…` (1701 is the ship's registry number),
  and ChatGPT's user id is `user-FAKE0DATAANDROID0POSITRONIC1`.
- **Keys are public constants.** The Signal backup passphrase and the
  WhatsApp root key are 64 zeros. They are not secrets; a fixed key
  lets the real decryption code run on made-up backups.
- **Shapes are real; content is not.** A fixture copies the keys,
  tables and quirks of a real export or API answer, and fills them with
  TNG content. The header of each generator, or the provider's
  `INGEST.md`, says what real data its shapes were read from.
- **Some images keep their own licence.** The photos on the contact
  cards are actors' photos from Wikimedia Commons, credited in
  [`carddav_tng/README.md`](../../datalib/backend/etl/providers/contacts/tests/fixtures/carddav_tng/README.md).

## How a fixture reaches the index

[`run_sync_pipeline.py`](run_sync_pipeline.py) turns every fixture into
a source in one data root, writes a config with a download step and a
render step for each, and syncs it. A fixture gets in one of four ways:

- **Recorded API answers.** For a source that talks to a web service,
  `datalib-step synthesize <type>` reads the fixture and writes the
  HTTP exchanges the download would make as playback tapes. The
  download then runs its real code against those tapes instead of the
  network. Garmin's and Google Drive's fixtures are a JSON spec the
  synthesizer expands; the others are the answers themselves.
- **Files read where they lie.** For a source that reads an export or
  a local folder, the download step reads the fixture directory
  directly, as it would read a person's own. The Google Takeout,
  LinkedIn and Facebook exports are copied into the workspace first,
  because a sandbox's inputs are links and those readers follow none.
- **A spec expanded into a backup.** Signal's and WhatsApp's fixtures
  are a JSON spec; `signal-make-fixture` and `whatsapp-make-fixture`
  turn each into an encrypted backup, which the download then decrypts.
  Beeper's is SQL and media, which
  [`make_beeper_texts.py`](make_beeper_texts.py) lays out as a Beeper
  Texts data directory.
- **A store written for it.** YoLink fetches its data with a plain
  `curl`, which the playback tapes cannot stand in for, so
  `yolink-make-fixture` writes its raw store from a spec and the
  pipeline runs only its render step (`PRESEEDED_RAW` in the driver).

The genrule runs every sync through `datalib-http`, driven over its API
the way the web client drives it ([`sync_drivers.py`](sync_drivers.py)).
`:ingested_tng_test` runs the same driver through `datalib-dag`, so
both are covered over the same corpus.

## The fixtures

`P` is `datalib/backend/etl/providers`. Every fixture in the first
table is in `:ingested_tng`.

| source type | fixture | how it gets in |
|---|---|---|
| `claude` | `P/claude/tests/fixtures/claude_export/` | recorded API answers |
| `chatgpt` | `P/chatgpt/tests/fixtures/chatgpt_api/` | recorded API answers |
| `slack` | `P/slack/tests/fixtures/slack_api/`, then `slack_api_v2/` | recorded API answers; `v2` is the workspace one sync later |
| `github` | `P/github/tests/fixtures/github_api/` | recorded API answers |
| `gitlab` | `P/gitlab/tests/fixtures/gitlab_api/` | recorded API answers |
| `notion` | `P/notion/tests/fixtures/notion_web/` | recorded API answers |
| `garmin` | `P/garmin/tests/fixtures/garmin_tng/tng.json` | spec → recorded API answers |
| `google_drive` | `P/google_drive/tests/fixtures/google_drive_tng/tng.json` | spec → recorded API answers |
| `contacts` | `P/contacts/tests/fixtures/carddav_tng/`, then `carddav_tng_v2/` | files; `v2` is the address books one sync later |
| `email` | `P/email/tests/fixtures/mbox/star_trek.mbox` | files (a Google Takeout mbox) |
| `google_takeout` | `P/google_takeout/tests/fixtures/Takeout/` | files |
| `linkedin` | `P/linkedin/tests/fixtures/linkedin_tng/` | files |
| `sms_backup_restore` | `P/sms_backup_restore/tests/fixtures/sms_backup_restore_tng/` | files |
| `facebook` | `P/facebook/tests/fixtures/facebook_tng/` | files |
| `pdf` | `P/pdf/tests/fixtures/pdf_tng/` | files, written by [`make_pdf_fixtures.py`](make_pdf_fixtures.py) |
| `claude_code` | `P/claude_code/tests/fixtures/claude_code_tng/` | files, written by [`make_claude_code_fixtures.py`](make_claude_code_fixtures.py) |
| `codex` | `P/codex/tests/fixtures/codex_tng/` | files, written by [`make_codex_fixtures.py`](make_codex_fixtures.py) |
| `calendar` | `P/calendar/tests/fixtures/calendar_tng/` | files (two `.ics` calendars) |
| `airvisual` | `P/airvisual/tests/fixtures/airvisual_tng/` | files (two monitors' data folders) |
| `signal` | `P/signal/tests/fixtures/signal_tng/tng.json` | spec → encrypted backup |
| `whatsapp` | `P/whatsapp/tests/fixtures/whatsapp_tng/tng.json` | spec → encrypted backup |
| `beeper` | `P/beeper/tests/fixtures/beeper_tng/` | SQL and media → a Beeper Texts directory |
| `yolink` | `P/yolink/tests/fixtures/yolink_tng/tng.json` | spec → raw store; render only |

These TNG fixtures are used by their own provider's tests and are not
in `:ingested_tng`:

| source type | fixture | what it is |
|---|---|---|
| `chatgpt` | `P/chatgpt/tests/fixtures/chatgpt_export/` | ChatGPT's data export, split into parts as the real one is |
| `calendar` | `P/calendar/tests/fixtures/calendar_split_tng/` | one calendar before and after a recurring series is split |
| `media` | `P/media/tests/fixtures/media_tng/` | photos, music and video, written by [`make_media_fixtures.py`](make_media_fixtures.py) |
| `file_inventory` | `P/file_inventory/tests/fixtures/file_inventory_tng/` | a folder to list; `materialize_tng_root` also drops it into the root as `file_inventory_scan/` |
| `email` | `//datalib/backend/etl/providers/email:tng_apple_mail` | an Apple Mail folder, built by [`make_apple_mail_folder.py`](make_apple_mail_folder.py) |
| `apple_messages` | `//datalib/backend/etl/providers/apple_messages:tng_chat_db` | a Messages `chat.db`, built by [`make_apple_messages_db.py`](make_apple_messages_db.py) |
| `apple_photos` | `//datalib/backend/etl/providers/apple_photos:tng_library` | a Photos library, built by [`make_apple_photos_library.py`](make_apple_photos_library.py) |
| `lightroom` | `//datalib/backend/etl/sqlite_mirror:tng_catalog` | a Lightroom catalog, built by [`make_lightroom_catalog.py`](make_lightroom_catalog.py); also synced on its own as `:ingested_lightroom` (below) |

One fixture is not TNG: `P/perseus/tests/fixtures/perseus_tiny/` is a
page of Thucydides in Greek and English, for the bilingual view
(`bazelisk run //datalib:dev_perseus`).

**Two kinds of generator.** The `make_*.py` scripts named "written by"
above were run by hand and their output is checked in; rerun one with
`uv run python tests/fixtures/<script>` after changing it. Those named
"built by" run as Bazel genrules on every build and nothing they make is
checked in. Both live here, not beside their provider, because
`tests/fixtures` is one of the directories `//:lint_repo` lints as
Python (`PYTHON_LINT_ROOTS` in `scripts/lint_repo.py`). The header of
each one lists the cases its output exists to cover.

## Sources synced on their own

`:ingested_tng` is one action over every source, so a change to any one
of them syncs them all again. A source can instead be synced by itself
with `tng_source` ([`tng_source.bzl`](tng_source.bzl), driven by
[`sync_one_source.py`](sync_one_source.py)): its ingest, its render and
a grid index over it alone, through `datalib-dag`, written to
`<name>/index.sqlite`. The build fails if the source leaves no row in
it. Such a source is not in `:ingested_tng`, and the tests that check
it name its index themselves. One source is synced this way:

| target | source | fixture |
|---|---|---|
| `:ingested_lightroom` | `tng_lightroom` (`lightroom`) | `//datalib/backend/etl/sqlite_mirror:tng_catalog` |

## What the build makes

`bazelisk build //tests/fixtures:ingested_tng` writes, under
`bazel-bin/tests/fixtures/ingested/`:

| file | what it is |
|---|---|
| `backend_index.sqlite` | the grid index, a plain SQLite file |
| `rendered.tar` | every source's render tree: markdown, render stores, blobs |
| `rendered_md.tar` | the markdown alone, for the UI's render preview |
| `time_series.tar` | the time-series stores and their index |
| `config_body.toml` | every source as a render-only group, for `materialize_tng_root` |
| `sql_scans.jsonl` | every statement the sync ran that walked a whole table or index, for `sql_scans_test` |

Three more genrules build the search index over it, in steps that Bazel
can cache apart, because embedding is the slow one:
`:ingested_tng_search_index` (`search_index.sqlite`,
`search_prompts.tsv`), `:ingested_tng_vectors` (`vectors.tsv`) and
`:ingested_tng_inference_cache` (`inference_cache.sqlite`).
[`materialize_tng_root.sh`](materialize_tng_root.sh) puts all of it
together into a data root, and is the one script both `dev_tng` and the
Playwright suite use.

**Reading what it made.** `backend_index.sqlite` opens in any SQLite.
The render stores in `rendered.tar` are doltlite files, which stock
`sqlite3` cannot open; turn one into a plain SQLite file with
`datalib-doltlite -readonly <store>.doltlite_db .dump | sqlite3 out.sqlite`
([`doltlite.md`](../../docs/dev/doltlite.md#getting-the-data-out-export-to-plain-sqlite)).

**What is stable from build to build.** The rows are; the files are
not. Dump `grid_rows`, `markdowns` and `edges` from two clean builds and
they match byte for byte. The files differ: the index gives itself a
random id (`store_generation`) so a reader can tell a rebuilt index
from the one it read, and doltlite stamps two of its own commits with
the wall clock, so every commit hash after them moves
([`doltlite.md`](../../docs/dev/doltlite.md#plain-sqlite-files-and-sqlite-compatibility)).
The rendered documents are stable too, except the "Store" section of
YoLink's page, which reports the store's commit log. Test the rows, not
the files. To check stability yourself, build into
two fresh roots more than a second apart: two builds in the same second
can agree by luck.

## What checks it

| target | what it asserts |
|---|---|
| `:ingested_tng` itself | refuses to build if a source synced without error but left no rows in the index ([`check_ingested.py`](check_ingested.py)); `:check_ingested_test` proves that check fails |
| `:ingested_tng_test` | row counts and per-source coverage, over three syncs of one root: fresh, unchanged, and after `--reset` |
| `:render_contract_test` | for every table of every source's raw store: change it, render incrementally and from scratch, and get the same result |
| `//datalib/backend/unified_grid_and_search_index:unified_grid_and_search_index_tests` | `fixture_db_snapshot`, an insta snapshot of the index's rows; `fixture_terms_snapshot`, what each row answers to (`from:`, `to:`, `with:`, `label:`, `pictured:`, …), one snapshot per source, and the handles' names; `fixture_tree_snapshot`, each source's tree of containers and rows and what each row is also filed under, after checking that every parent is a row and `read_uuid`/`place_uuid` name the nearest container above. All three over `:ingested_tng`, the last two over `:ingested_lightroom` too |
| `//datalib/backend/applets:applet_unittests` | `search_finds_itself_tests`: every row is found by each key's value, each search term, each pasted id and its rarest word; over `:ingested_lightroom`, by each key, term and pasted id, its caption as a phrase and its own H3 cell |
| `//datalib/backend/datalib_step:step_tests` | `index_text`, the text search draws from each fixture document, as one snapshot per source |
| each provider's own tests | its parse and render snapshots, over its own fixture |
| `//datalib/ui:e2e_test` | the Playwright suite, against a root from `materialize_tng_root` |
| `:tng_fuzz_test` (manual) | random clicks and killed steps, then a full sync compared with a sync from nothing |

Snapshots are updated through `.update` targets, such as
`//datalib/backend/unified_grid_and_search_index:unified_grid_and_search_index_tests.update`
and `//datalib/backend/datalib_step:index_text.update`
([`testing.md`](../../docs/dev/testing.md)).

## Changing the corpus

**Changing a fixture.** Edit it beside its provider, or edit its
generator and rerun it. Keep it fictional and in the TNG world. Then run
the `.update` targets above and read the snapshot diff: a change to
`index_text` is a change to what search indexes.

**Adding a source to `:ingested_tng`.** In order:

1. A `tng_fixture` filegroup in the provider's `BUILD.bazel`, added to
   `:source_json` here.
2. The fixture's path appended to the `:ingested_tng` genrule's command
   **and** to `_TNG_FIXTURE_ARGS`. Append; don't renumber. The driver
   reads its arguments by position, and those two lists must agree.
3. An entry in the driver's `sources` table, and a line for the new
   argument in its docstring.

`check_ingested.py` fails the build until the new source has rows in
the index, so a source that syncs but indexes nothing cannot slip in.
