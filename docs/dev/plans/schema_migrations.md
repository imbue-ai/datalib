# Schema changes after there are users: an audit, and a plan

**Status: audit and proposal (2026-09-21), nothing in §3 built.** §1
and §2 describe what the tree does today and were checked against it
at `a5f04141`, by reading the code, not the prose. Where this doc and
the tree disagree, the tree wins.

## 0. Why now

AGENTS.md says breaking changes are fine because there are no real
users yet. That stops being true soon, and the question this doc
answers is what has to be in place before it does: which of the
mechanisms we already have for surviving a schema change are sound,
which are only sound under an assumption that is about to break, and
what is missing altogether.

"Schema" here means every shape a running datalib reads back from a
data root and would misread if an older or newer build had written it:
the tables in every doltlite store, the plain-SQLite run store, the
JSON files, the config, the uuid recipes, and the strings an enum is
stored as. The user's own data is the raw stores (`<group>/ingest/`),
and everything else is derived from them — but "derived" is only a
comfort when the derivation can be re-run, and §1.2 says where it
cannot.

The specific question asked — should every doltlite file carry a
schema version number — is answered in §2.4: yes, but a number alone is
the weakest of the three things a store should carry, and the one we
have least need of.

## 1. What the tree does today

### 1.1 One store at a time

| Store | Schema is | Change detected by | What happens on a mismatch | Version recorded in the file |
|---|---|---|---|---|
| Raw entity store `<group>/ingest/entities.doltlite_db` | a provider's `schema_raw.rs` (`PortableTable` derive → DDL) plus `SHARED_DDL` | `doltlite_raw::open` → `reconcile_table_schema`, per table, **column names only** | missing columns: `ALTER TABLE ADD COLUMN`; anything else: `DROP TABLE` + recreate, then every store-wide cursor is cleared so the next run walks from the start | none |
| Blob CAS `<group>/ingest/blobs.doltlite_db` | `CAS_OBJECTS_DDL` + per-provider edge tables | same reconcile | same; a recreated `cas_objects` is refilled by re-fetch | none |
| SQLite-mirror stores (lightroom, apple_photos, whatsapp, apple_messages) | *upstream's* schema, rebuilt from `PRAGMA table_xinfo` every run | not applicable: every run drops every mirror table and refills from the source file, doltlite dedups | upstream drift is absorbed by construction | none (but the source file's own version, where it has one, is in the mirror) |
| Render store `<group>/render_markdown/indexed_markdown.doltlite_db` | `indexed_markdown::store_ddl()` — `grid_rows`, `markdowns`, `edges`, `problems`, `measurements`, `render_cursor`, `render_inputs` | the **same** per-table reconcile as the raw store (`open_derived`) | same as raw — but see §1.2: the render cursor is **not** among the cursors it clears | `markdowns.renderer_version` per document, `"rust-v1.<RENDER_VERSION>"` |
| Grid index `unified_index/grid_index/db.doltlite_db` | `grid_index::index_ddl()` | `reconcile_index_schema`, column names, all tables together | drop **every** index table and rebuild from the render stores; `source_cursors` goes with them, deliberately | none |
| qmd index `unified_index/qmd_index/` | qmd's own (plain SQLite) | qmd's; datalib only reconciles the collection list | a collection no group claims is retired; the one migration so far (`mirror` → per-source) is hand-coded in `qmd_index.rs` | qmd's, plus `DEFAULT_QMD_VERSION` pinned in `datalib_runtime` |
| Run store `system/runs.sqlite` | `app_schema::runs` | `PRAGMA user_version` against `datalib_runs::SCHEMA_VERSION` (7; it has moved in six commits since 2026-09-11) | **delete the file and remake it** | `user_version` |
| Feedback `system/feedback.doltlite_db` | `app_schema::feedback` | `CREATE TABLE IF NOT EXISTS` — i.e. not detected — plus one hand-written probe (`app_store_migrate`: "is `created_at` still there?") | the one known rename is migrated in place and dolt-committed; any other change goes unnoticed until a query names a column that is not there | none |
| Jobs `system/jobs.doltlite_db`, usage `system/usage.doltlite_db` | `app_schema::sync_jobs`, `disk_usage` | same as feedback | same | none |
| `system/dag_state.json` | `dag::state::DagState` (serde) | every field `#[serde(default)]` | an unknown field is ignored; a missing one reads as "no record", which re-runs the step | none |
| `config.toml` | `dag::config` | the loader recognises one retired shape by its keys | a diagnostic naming `datalib-migrate-config`, which holds exactly one rewrite at a time | none |
| uuid recipes (`docs/dev/entity_ids.md`) | code, per provider | not detected: a changed recipe just mints different uuids | every document re-keys on the next `RENDER_VERSION` bump; anything that stored the old uuid (feedback's `target_uuids`, a bookmark, an agent's notes) now points at nothing | `upstream_scope` beside the uuid, on some providers |
| Enum spellings in `VARCHAR` columns | `strum` enums, `parse → Option` | at read: `RunState::parse`, `Severity::parse`, `problems::from_row` | a spelling this build lacks is `None` or an error, by the caller's choice — never a guess | the spelling is the version |
| UI `localStorage` | three keys (dev mode, a handoff flag, expanded groups) | none | a stale value is a cosmetic default | none |

Things the table does not show that also count as schema:

- **`payload` contents.** Upstream's shape, stored verbatim as JSONB.
  Upstream drift is render's problem by design and is not what this
  doc is about. But *our* shaping of a payload — `VolatilePath`s, the
  sort-before-store rule, `payload_scheme` — is schema: change it and
  every row's content hash moves, `dolt_diff` reports every row
  changed, and the next render is a full one. That is correct and
  slow, and nothing says so at the time.
- **Render params.** `LAYOUT_VERSION` (chat-common) and each
  processor's `render_params` are folded into the render cursor, so a
  change re-renders the source. This is the best-behaved version
  mechanism in the tree: it is compared, it is automatic, and a bump
  cannot be forgotten because it is *the* way to say "the output
  changed".
- **The doltlite file format.** Pinned at v0.50.3, the release whose
  headline is storage-format stability. Nothing records which doltlite
  wrote a file and nothing checks.
- **The HTTP API and the TypeScript unions in `api.ts`.** Hand-kept
  mirrors of the Rust enums. Not a data-root concern, but the same
  class of thing: a spelling added on one side and not the other.

### 1.2 Where the current design is unsound, in order of harm

Each of these was verified against the code; the function names are
where to look.

**(a) Drop-and-recreate rests on "raw rows are re-fetched", and that is
false for the stores that matter most.** `etl/README.md` §"Schema
self-healing" says dropping a raw table is safe "specifically because
raw-store rows are a cache of upstream, re-fetched on the next sync".
For an API-backed source that is true. For a one-shot import — a
Signal backup, a Facebook or Google Takeout export, a WhatsApp
`msgstore` from a phone since replaced, an mbox — there is no upstream
to fetch from, and the raw store *is* the only copy. For an
API-backed source with a retention window (Slack free tier, a mailbox
with old mail purged) the same is true of everything older than the
window. Doltlite keeps the dropped rows in history, so the bytes are
not gone; but nothing in the tree reads them back, and the person who
ran the sync sees a source that went empty and a log line at `warn`.
`data_architecture_ingestion_practices.md` §"Schema evolution" names
this hazard and says "we aren't there yet". We are still not there,
and it is the one that loses data.

**(b) The reconcile compares column names and nothing else.**
`reconcile_table_schema` (`doltlite_raw.rs`) diffs `declared_names`
against `actual_names`. A changed primary key, a changed type or
affinity, a `NOT NULL` added or removed, a generated column whose
expression changed — every one of these has the same column names on
both sides and reads as `Kept`. The store then holds rows written
under the old rule while every writer follows the new one. For a PK
change the upsert stops meaning "same upstream row"; for an
expression change the index over the generated column lies.

**(c) A renamed table is an orphan plus an empty twin, and no cursor
is cleared.** The reconcile walks the *declared* DDL: a table that is
in the file and no longer in the DDL is never visited, and the new
name has `actual.is_empty()` and is created fresh and reported
`Kept`. Since nothing was `Recreated`, `forget_cursors_after_recreate`
does not run, and the next sync resumes past everything — the new
table stays empty until upstream changes, with nothing saying why.
This is exactly the silent shape #05db5c6f fixed for the recreate
path, still open for the rename path.

**(d) The render store shares the raw store's reconcile but not its
cursor list.** `IndexedMarkdownStore::open` goes through
`open_derived(&path, &store_ddl())`, so a non-additive change to
`grid_rows`, `markdowns` or `edges` DDL drops and recreates that table
in every render store. `CURSOR_TABLES` names `sync_scope_state`,
`sync_scope_config` and `ingested_files` — none of which a render
store has — so `render_cursor` survives, and the next render is a
delta from the stored raw commit. `render_versions()` then returns an
empty set, `tree_is_from_an_older_renderer` returns `false` on an
empty set, and the plan is `FromCursor`. Result: a render store that
holds only the documents whose inputs changed since the last run, and
a grid index rebuilt faithfully from it. Recoverable by bumping every
provider's `RENDER_VERSION` or deleting the stores, if someone works
out what happened.

The additive case is quieter and more likely: a new `grid_rows`
column arrives by `ADD COLUMN`, every existing row has `NULL` in it,
and only a `RENDER_VERSION` bump revisits them. `docs/dev/grid_rows.md`
§"Adding a column" does not say to bump anything — step 5 is
"re-bake the fixture", which is the step that hides it, because the
fixture is always baked fresh.

**(e) `grid_index::RENDERER_VERSION` is stored and never compared.**
Its comment says to bump it "when the rendered `.md` layout changes
for every provider at once: every document's version then differs
from its store's". The stored value is `"rust-v1.<n>"`, and both
readers (`render_versions`, `documents_matching`) take
`rsplit('.').next()` — the `<n>`. Bumping `rust-v1` to `rust-v2`
changes nothing. `LAYOUT_VERSION` in the render params is what
actually does this job for the chat providers.

**(f) There is no downgrade guard anywhere.** Nothing records which
datalib wrote a root, and nothing refuses a root written by a newer
one. An older build opening a newer root does, in order: the raw
reconcile sees the newer columns as "extra" and **drops and
recreates** those tables (losing the columns and, per (a), possibly
the rows); the run store sees a `user_version` it does not know and
deletes the file; `dag_state.json` drops the fields it does not know;
the app stores keep the newer shape and fail on the first query that
names a renamed column. This is not hypothetical: the Minds template
runs a *pinned* datalib against a root the desktop app may have
written with a newer one.

**(g) Nothing records which build wrote a store.** No table, no
pragma, no sidecar. The only version stamps in any store are
`feedback.app_version` (per row) and `markdowns.renderer_version`
(per document). When a migration has to reason about what it is
looking at, all it has is the column list.

**(h) The app-store migration is a one-off, not a mechanism.**
`app_store_migrate` is correct and tested for the one rename it
covers, keyed on "is the old column still there". A second rename
needs a second probe, a third a third, and nothing orders them or
says which have run. Fine at n=1; it is the shape that grows into
the pile of `if column_exists(...)` every ORM's migration table
exists to avoid.

**(i) A uuid recipe change is a schema change with no version
anywhere.** The recipes are code; changing one re-keys every
document the next time it renders. `docs/dev/entity_ids.md` documents
the re-key as a `RENDER_VERSION` bump, which handles the render and
index side. What it cannot handle is anything that *stored* a uuid
expecting it to stay: feedback's `target_uuids` and `row_uuids`, and
any agent or person who bookmarked `m-<uuid>`. `upstream_scope`
(stamped beside the uuid on some providers) is the right start — the
recipe input is kept, so the old id can be regenerated — but it is
per-provider and nothing reads it back.

### 1.3 What is sound and should stay

- **The schema is the struct.** `PortableTable` derives the DDL, so a
  field change *is* a DDL change, and the reconcile sees it at the
  next open without anyone bumping anything. The `schema_inventory`
  golden makes every table or column change a reviewed diff. Keep
  both; build on them rather than beside them.
- **Compare content, not counters, wherever a rebuild is cheap.** The
  grid index rebuilds wholesale on any drift; the render cursor
  carries the params and re-renders on any change; the run store is
  remade. For derived data whose regeneration is a local scan, a
  version number would only add a way to forget the bump.
- **Two passes: tables, reconcile, indexes.** The ordering in
  `open_inner` is load-bearing (an index over a new column fails
  against an old store), and the drop path costs no index. Keep it.
- **Enums parse to `Option`, never a guess.** A spelling from a newer
  build is refused rather than misread. This is the downgrade guard
  in miniature, and the model for the one in §3.
- **A recreated table forgets the store's cursors** (#05db5c6f). The
  right instinct; §1.2(c) and (d) are the two places it does not
  reach.
- **Doltlite history.** Every drop, recreate and migration on a
  committed store leaves the previous shape readable at the previous
  commit through `dolt_at_<table>(<hash>)`. This is the tool that
  makes (a) survivable and is what a real migration should be built
  on: read the old shape at `HEAD`, write the new one, commit.
- **`sync_scope_config` beside the cursor.** A cursor is only valid
  under the config that set it, the store records that config, and
  `lint_repo.py` check 8 refuses a provider that keeps a cursor
  without it. The same rule, with "schema" for "config", is what §3.2
  asks for.

## 2. What we want

### 2.1 Stores by what a mistake costs

The right policy differs by store, and the difference is not "raw vs
derived" — it is whether the bytes can be produced again.

| Class | Stores | If we get it wrong |
|---|---|---|
| **Irreplaceable** | feedback; raw stores of an import-shaped source (signal, facebook, google_takeout, mbox email, sms_backup_restore, an archived whatsapp/apple_messages copy, pdf and media trees the user has since moved); the older-than-the-window part of any windowed API source | the user's data is gone, or reachable only through doltlite history by someone who knows to look |
| **Expensive** | raw stores of a live API source (slack, gmail, github, claude, chatgpt, notion, garmin, …) | a re-download: hours, API quota, a rate-limit ban |
| **Rebuildable** | every render store, the grid index, the qmd index, the run store, jobs, usage, `dag_state.json` | a local scan or an embed pass; minutes to an hour |

A source's class is not a property of its provider type — the same
`whatsapp` provider reads a live phone backup or a copy from a phone
since reset — so it has to be declared per source (§3.5) or assumed
conservatively.

### 2.2 The rules

1. **An additive change costs nothing and asks nothing.** A new
   column, a new table, a new optional field. Today this is nearly
   true; §1.2(d) is the gap.
2. **A non-additive change to any raw store is a migration, written
   down, ordered, and run under a name.** Never a drop-and-recreate.
   If no migration is written, the open **fails loudly** and says
   which table and what changed. `--reset-and-redownload` remains the
   escape hatch and remains a thing a person types.

   This is strict on purpose, and it is strict for the Expensive class
   too, not only the Irreplaceable one — decided 2026-09-21. The
   lenient alternative (rebuild a refetchable store automatically,
   with a warning) was weighed and turned down: its failure mode is
   unrecoverable data on a user's machine when the class was guessed
   wrong, and the strict rule's failure mode is a developer typing one
   flag. What a developer iterating on a live-API provider gives up is
   the silent re-download the tree does today; what they get is a
   message naming the table and the change, and the same re-download
   after one flag. Additive changes are untouched either way, and
   most iteration is additive.
3. **A non-additive change to a rebuildable store may rebuild — and
   must rebuild all of it.** The whole store, every cursor with it.
   (d) is a store that rebuilt half.
4. **Every store says who wrote it,** so an older build can refuse and
   a migration can know what it is looking at.
5. **An older build refuses an irreplaceable or expensive store a
   newer build wrote, and rebuilds a rebuildable one.** A refusal is
   `app_ready: false` with a sentence, not a stack trace.
6. **A bump nobody has to remember is better than a bump.** Where the
   fact can be derived — a hash of the DDL, the params in the cursor
   — derive it. Reserve hand-bumped numbers for the one thing a hash
   cannot do: order the migrations.
7. **What a schema change breaks goes in the commit message and in
   the release notes.** AGENTS.md already says the first; the second
   starts mattering the day there is someone to read it.

### 2.3 Non-goals

- Migrating a root written before v0.35 (the current release line).
  There is nobody with one. The plan is for what comes *after* this
  lands, not for the roots that exist today.
- Multi-version read compatibility ("a build reads any older store
  without migrating it"). Migrate on open, once, forward only.
- Automatic migration of `payload` contents when upstream changes
  shape. That is render's job and stays it.

### 2.4 The question asked: a version number in every doltlite file?

**Yes, but it is the third-most-useful of three facts to record, and
the one whose absence has hurt least.** What a store should carry, in
order of use:

1. **Which build wrote it** — the datalib version and git hash. This
   is what a downgrade guard reads, what a bug report needs, and what
   a migration checks first. Nothing carries it today (§1.2(g)).
2. **What shape it is in** — a hash of the DDL list the owner opened
   it with. This is what detection needs, and it cannot be forgotten:
   it is derived. The reconcile already computes the equivalent by
   introspection; storing the hash lets a *reader* — which cannot
   reconcile — know before its first query whether the columns it
   will name are there, and lets a downgrade refuse before touching
   anything.
3. **Where it is on the migration ladder** — an integer. This is what
   *ordering* needs: "migrations 4 and 5 have run, 6 has not". It is
   the only one of the three that has to be bumped by hand, and the
   only one that is meaningless without a list of migrations beside
   it. It is worth having precisely when there is such a list, and
   not before.

A number alone — the `user_version` pattern the run store uses — is
right for the run store, where the only action is "remake", and
wrong for a raw store, where the number would be bumped for every
struct change (or, more likely, not bumped, and the reconcile would
still be the thing that noticed). Store the number, but make the
hash the thing that detects and the version string the thing that
guards.

## 3. The plan

Five PRs, roughly in dependency order. Each is small enough to review
on its own and lands something that is useful alone.

### 3.1 PR 1 — `_datalib_meta`: every store says who wrote it

One table in every doltlite store and the run store, written by the
owner on every open, read by anyone:

```sql
CREATE TABLE IF NOT EXISTS _datalib_meta (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL,
    written_at_utc TEXT NOT NULL,
    tz_offset  TEXT NULL
)
-- keys: datalib_version, git_hash, doltlite_version,
--       schema_hash, schema_version, store_kind
```

- `schema_hash` is blake3 over the DDL list the owner passed to
  `open`, in order, after the reconcile ran. `store_kind` is
  `raw | blobs | render | grid_index | feedback | jobs | usage | runs`.
  `schema_version` is the ladder position from §3.3, `0` until a
  store has a ladder.
- Written inside the schema commit `open` already makes, so it costs
  no extra commit and rides in history: `dolt_log` on any store
  becomes "which build wrote each commit". Excluded from
  `SHARED_TABLES`' mirror/diff logic the same way the bookkeeping
  tables are.
- `schema_inventory` learns the table once; the golden moves once.
- Readers get `datalib_etl::meta::read(pool) -> Option<Meta>`, which
  is `None` on a store from before this PR.

Nothing acts on it yet. This PR is the fact; the next ones are the
uses.

### 3.2 PR 2 — the reconcile compares shape, refuses by class, and reaches the render cursor

Three changes to `doltlite_raw`:

- **Compare the full column shape.** `ColumnInfo` already carries
  `decl_type`, `not_null`, `default` and `generated`; the comparison
  uses only `name`. Compare all of them plus the primary key
  (`pk` from `table_xinfo`). A difference in anything but "a column
  is missing" is a *non-additive* change. Closes §1.2(b).
- **Notice orphans.** After walking the declared DDL, list
  `sqlite_master` tables that are not declared, not `_bookkeeping`,
  not shared, not `_datalib_meta`, not a doltlite system table. An
  orphan is logged and reported through `problems`
  (`Stage::Fetch`, warning, one row per table), and it counts as a
  non-additive change for the cursor rule. Closes §1.2(c).
- **Non-additive means: refuse, unless the caller said rebuild.**
  `open` grows a policy argument:

  ```rust
  pub enum OnSchemaBreak {
      /// Drop, recreate, forget every cursor. Rebuildable stores.
      Rebuild,
      /// Fail the open with a message naming the table and the change.
      Refuse,
  }
  ```

  `open_derived` (render, index) passes `Rebuild` and adds
  `render_cursor` to the tables a rebuild forgets — closing §1.2(d)
  — or, simpler and stricter, treats a render store the way the grid
  index treats itself: any drift drops every table. The raw-store
  `open` passes `Refuse` — for every raw store, whatever its class —
  unless `DATALIB_DAG_RESET_AND_REDOWNLOAD` (the env the runner sets
  for `--reset-and-redownload`) is on. The message says: the store,
  the table, what differs, and the two ways out — a migration (§3.3)
  or a reset of that source. Closes §1.2(a) for the non-additive case;
  the additive case was already safe.

The test that earns this PR is the one AGENTS.md asks for on every
silent-no-op fix: bake a store under the old DDL, open it under a
renamed column with `Refuse`, and assert the open fails *and the
table still has its rows*. Then the same with a renamed table and an
orphan report.

### 3.3 PR 3 — a migration ladder for raw and app stores

Where a non-additive change is wanted rather than refused, it is
written as a step on a ladder:

```rust
pub struct Migration {
    /// Position on the ladder. Dense, starting at 1, per store kind
    /// (per provider for raw stores).
    pub version: u32,
    /// One line, becomes the dolt commit message.
    pub name: &'static str,
    /// Runs inside one transaction, before the reconcile, against a
    /// store at `version - 1`. May read the old shape at HEAD
    /// through `dolt_at_<table>` and write the new one.
    pub apply: fn(&mut Transaction<'_, Sqlite>) -> BoxFuture<'_, Result<()>>,
}
```

- A provider's `schema_raw.rs` gains `pub const SCHEMA_VERSION: u32`
  and `pub const MIGRATIONS: &[Migration]`; `AppStore` gains the same
  per file. `open` reads `_datalib_meta.schema_version`, runs every
  migration above it in order, each in its own dolt commit named
  `migrate <store> v<n>: <name>`, writes the new version, and only
  then reconciles — which should now find nothing to do, and says so
  loudly if it does (a migration that left the shape wrong is a bug
  in the migration, not a reason to drop the table).
- `app_store_migrate`'s stamp rename becomes migration 1 of each of
  the three app stores, keyed on the ladder instead of on
  `column_names`. The probe stays as a one-time bootstrap: a store
  with no `_datalib_meta` and the old column is at version 0.
- A migration may declare `resets_cursors: true` when it changes what
  a cursor means (a PK change does; a column rename does not).
- A test per migration is the rule, and the test is the same shape
  every time: build the `version - 1` store by hand, run the ladder,
  assert the rows. `app_store.rs`'s
  `a_store_from_before_the_utc_columns_is_migrated_on_open` is the
  template.

`lint_repo.py` gets a check: a change to a `schema_raw.rs` or
`app_schema` struct that removes or renames a field must touch the
same crate's `MIGRATIONS` or `SCHEMA_VERSION`. Crude, like check 8,
and for the same reason: the failure mode is forgetting.

### 3.4 PR 4 — the downgrade guard, and a root-level check

- `open` (raw) and `AppStore::open` compare `_datalib_meta.datalib_version`
  to their own. Newer by a *minor* or more: refuse with "this store
  was written by datalib X; you are running Y". Same minor, newer
  patch: allow (patch releases do not move schemas — make that a
  release rule and write it in `release_steps.md`). `schema_version`
  above our ladder's top: refuse regardless.
- Rebuildable stores keep rebuilding on a newer schema; that is
  correct and already what they do. The run store's "delete and
  remake" stays.
- `datalib-http` runs the check across the root at startup and on
  every config reload, the way it already produces `app_ready:
  false` for a config it cannot serve. A refused store becomes a
  Manage-row problem, not a 500 on first query. The Minds case
  (§1.2(f)) becomes a sentence on screen.
- `datalib-dag --check` reports it too, so an agent at a shell sees
  it before a sync does.

### 3.5 PR 5 — a source says whether it can be fetched again

The class in §2.1 is per source. Two ways to get it:

- **Derive it from the ingest method's `Reach`** (`source_common`):
  `Local` is import-shaped, `Origin` is not. This is right often
  enough to be the default and wrong for exactly the cases that
  matter (a live phone backup dir is `Local`; a windowed API is
  `Origin`).
- **Let the config say it.** A `[[groups]]` key, `refetchable = true |
  false`, defaulting from `Reach`. The wizard sets it from a
  one-line question ("Is this export still available upstream?") for
  file-backed sources.

The flag does not change *whether* a raw store refuses — §2.2 rule 2
says every one does. It changes what the refusal **offers** and what
the guard **allows**:

- The refusal message, the Manage row and `datalib-dag --check` offer
  "reset this source" as the way out only when `refetchable = true`;
  for `false` they name the migration path and the doltlite-history
  recovery recipe, and the reset stays a flag a person has to find.
- The downgrade guard (§3.4) may let a *rebuild* through for a
  refetchable store when the person has confirmed a reset; for a
  non-refetchable one it never does.

Until this PR lands every raw store is treated as `refetchable =
false`, which is the safe side and, for the refusal itself, the same
side.

### 3.6 Alongside, not gated

- **Delete `grid_index::RENDERER_VERSION`** or make the readers compare
  the whole string. Deleting is right: `LAYOUT_VERSION` and
  `render_params` already do the job, and a constant whose comment
  describes a behaviour it does not have is worse than none. (§1.2(e))
- **Fold the render store's `schema_hash` into the render cursor's
  params.** Then a change to `grid_rows` DDL re-renders every source
  the way a `LAYOUT_VERSION` bump does, with nobody remembering
  anything, and `grid_rows.md`'s checklist needs no new step. The
  same trick — a hash of the DDL in the thing that decides "did the
  inputs change" — is what §2.2 rule 6 means.
- **The add-a-column checklist** in `grid_rows.md` says, until the
  fold above lands: bump `RENDER_VERSION` in every provider that
  populates the column, or the column is `NULL` on every existing row.
- **A "written by the previous release" test in CI.** The fixture is
  always baked by the tree under test, which is exactly why (d) was
  never seen. Add a job that takes the previous release tarball
  (release.yml publishes them), bakes the TNG root with it, then runs
  the current build's `datalib-dag --check` and one sync against that
  root, and asserts: no `Refuse`, no unplanned `Rebuild` of a raw
  table, row counts preserved. This is the test that catches the class
  of bug this doc is about, and none of the others do.
- **Release notes carry a "data root" section**: which stores
  migrate, which rebuild, whether a reset is needed. The
  `release` skill drafts it from the commits that touched
  `MIGRATIONS`, `SCHEMA_VERSION`, `RENDER_VERSION`, `LAYOUT_VERSION`
  or `datalib_runs::SCHEMA_VERSION`.
- **uuid recipes (§1.2(i)).** Stamp `upstream_scope` on every provider
  that mints, not some; and when a recipe changes, the migration for
  the *feedback* store rewrites `target_uuids` from the old recipe to
  the new using the stamped inputs. That is the only store where a
  stale uuid is a lost user action rather than a stale cache. Out of
  scope to build now; in scope to stop making it worse.

## 4. Open questions

- **Does the ladder belong to the provider or to the framework?** A
  change to `SHARED_DDL` (`sync_runs`, `problems`, the bookkeeping
  shape) touches every raw store. Two ladders per store — one for
  the shared tables, one for the provider's — or one ladder with the
  framework's steps interleaved. Two is simpler to reason about; the
  meta table holds two integers.
- **Should a migration be reversible?** Doltlite says no: the
  previous shape is at the previous commit, and a downgrade is a
  refusal, not a reverse migration. Revisit only if a downgrade path
  is ever wanted.
- **The mirror stores.** They rebuild from the source file every run
  and carry upstream's schema; the framework's meta table applies,
  the ladder does not. A mirror of a file the user has since deleted
  is irreplaceable, and the engine already protects it: the source
  is snapshotted read-only (`vacuum_into`, `create_if_missing(false)`)
  before any mirror table is dropped, so a missing source fails the
  run and leaves the mirror as it was. Worth a test that says so.
- **How much of this does the supervisor plan absorb?**
  `plans/supervisor.md` makes sinks first-class with one writer at a
  time. A sink's open is the natural home for §3.2–3.4; if that plan
  lands first, this one's PRs become its sink contract rather than
  changes to `doltlite_raw::open`.
