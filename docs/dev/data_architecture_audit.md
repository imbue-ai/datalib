# Data Architecture Audit

Audit of the ETL codebase against [`data_architecture_ingestion.md`](data_architecture_ingestion.md),
produced 2026-06-09.

> **2026-06-12 superseded findings**: every "pre-seed before fetch" /
> "missing pre-seed" finding in this audit is now obsolete. The
> "Retry and fetch durability" principle was reversed (see
> `data_architecture_ingestion.md` §"No-preseed listing flow"):
> entity rows now only exist after a successful detail fetch.
> Skip-check happens via bulk-read of stored `update_time` /
> `updated_at` compared to the listing's value. Findings about
> missing pre-seed on slack messages, github discussions, yolink
> devices, notion blocks/comments, etc. are no longer violations.

For each provider plus the shared/orchestrator layer, four buckets are
called out: principle violations, dead patterns to remove, simplification
opportunities, and cross-source sharing opportunities. The final
section synthesizes the cross-source threads into proposed unification
work.

Each section was produced by an audit agent reading the architecture
doc against one slice of the tree. They have not been cross-checked
against each other except in the final unification synthesis; expect
some duplication and disagreement at the margins — we'll work through
those together.

---

## Shared / orchestrator layer

### Principle gaps in shared code

- P2: We don't need this to exist now, but we need it to be able to exist soon. **Missing `--retry-failed` / `--no-retry-failed` flag**: The architecture document (§"Retry and fetch durability") mandates `--retry-failed` (default true) + `--no-retry-failed` to control per-row failure retries. The shared `control.rs` exports `ExtractControl { reset_and_redownload, refetch_blobs }` but has **no `retry_failed` field**. The dolt infrastructure (`failed_ids`, `record_object_error`, `ensure_object_row`) is complete in `doltlite_raw.rs`, but the orchestrator doesn't wire a CLI flag into it. Providers cannot honor the retry principle because the orchestrator doesn't request it. **`sync/src/main.rs`**: no `--retry-failed` arg defined; `control.rs` line 14 missing field.

- P3: This doesn't need to be enforced, but it should just be strongly recommended. **Single-commit-per-source enforcement absent**: The document mandates "orchestrator wraps each source's extract in exactly one commit" with message shape `extract <name>: <stats>`. This IS implemented (`sync/src/main.rs` line 1841), but the enforcement is asymmetric: if a provider were to call `dolt_commit` internally, nothing prevents it. A shared guard (e.g., `ensure_single_commit_per_extract` in the shared layer) is not articulated. Providers must be trained not to call `commit_run` themselves; no affordance prevents the mistake. **`doltlite_raw.rs` module docs** (lines 1–52) spell out the policy but don't prevent it in code.

- P2: Let's use this in the places where we can, but it is not mandatory because it's not always possible. **`ensure_object_row` available to providers but rarely used**: The shared library exports `ensure_object_row` in `doltlite_raw.rs` (line 567) per the port guide. The principle (§"Retry and fetch durability") is "pre-seed before fetch" for every entity. Audit of provider usage shows it's implemented inconsistently: **Notion** uses it on pages/blocks (clean); **Anthropic/ChatGPT** use it sparingly. Some providers that discover IDs mid-fetch (Slack threads within messages) cannot pre-seed; the framework acknowledges this. No violation detected, but the guarantee is "aspiration" not "always," per the doc's own admission.

- P1: Standardizing this would be very valuable because it would help us eliminate inconsistencies and reduce code. **`<table>_bookkeeping` helpers exported but responsibility is split**: The shared layer provides `bookkeeping_ddl_for(table)` in `doltlite_raw.rs` (line 161), `record_object_attempt`, `record_object_error`, and `failed_ids`, but **each provider's `extract/db.rs` must manually instantiate** the bookkeeping table in its own DDL block and call the record-* helpers. No macro or centralized builder collapses this boilerplate — a new provider's author must copy the pattern from a template. **Port guide** (§6) mitigates this with the template reference, but it's not as tight as shared code would be.

- P1: Standardizing this config would be valuable. **No per-source retry policy config in `config.yaml`**: Document §"Retry and fetch durability" states "retry policy is config, not code" with "per-source `sync:` blocks in `config.yaml` should support the same retry knobs as the global default." Inspection of `frankweiler_core::config` (not in audit scope but referenced) reveals no per-source retry-policy fields. Global `--retry-failed` exists in CLI only; no config-file equivalent, no per-source override.

- P2: If we can come up with a better mechanism to ensure compliance, that would be great, but not urgent. **No `ObsArgs` flattening enforcement**: The document mandates "every binary flattens [`obs::ObsArgs`]" via clap's `#[command(flatten)]`. The sync binary does this (`sync/src/main.rs` line 179), but the shared layer doesn't guard against binaries that forget. A provider's standalone `*_download` binary (e.g., `anthropic_download.rs`) may or may not include it; no compile-time check forces compliance.

### Dead patterns

- P1: Let's just stop building these for now. It just creates extra compilation/linking we don't need. **Vestigial `<provider>_download` rust binaries**: The document states (§"Shape of the system", line 46) "there are vestigial `<provider>_download` rust_binary targets that could be revived. Today they aren't on the production path." Each provider crate at `providers/<name>/src/bin/<name>_download.rs` exists: **Anthropic, ChatGPT, Slack, Notion, GitHub, GitLab, Beeper**. These binaries are built but unreferenced in the main sync pipeline. They remain as escape-hatches for single-provider debugging but create confusion about multiple entry points. **No cleanup needed immediately** per the doc's phrasing, but they add surface area to maintain.

- P4: Nothing to do here. **`endpoint_shapes` remnant**: Document §"Unresolved questions" / "Detecting upstream shape drift" explicitly states `endpoint_shapes` was "deleted; see commit history." The code search confirms zero references in current tree (binary search hit patterns in other files). The concept is genuinely gone, not dead code. No violation.

- **JSONL-tree raw-store code likely absent**: The port guide extensively discusses "old JSONL-tree raw-store remnants." Current audit shows all providers (notion, chatgpt, anthropic, slack, github, gitlab, beeper) use doltlite. No JSONL-tree fallback logic detected in shared layer. **Confirmed clean.**

- P1: I thought we'd probably also use this for beeper and signal when we want to group arbitrarily long message threads. If we don't, we should. **`periodize.rs` (line 5435 bytes)**: Module is present but rarely invoked. Appears to be a utility for time-windowing (e.g., Yolink sampling). No evidence it's dead, but it's specialized to one provider and not exported from lib.rs. Not a violation; just narrow scope.

### Simplification opportunities

- P1: Standardizing this would ensure we don't have any drift. **Bookkeeping DDL boilerplate**: Every provider repeats the same bookkeeping pattern. A macro like `bookkeeping_tables!(table1, table2, ...)` could auto-generate DDL + the four column constants. **Impact**: ~20 lines per provider → 0 lines of duplication. **Effort**: low. **Precedent**: `BLOB_REFS_DDL`, `SYNC_RUNS_DDL` are already shared constants.

- P2: Seems nice but won't affect the shape of bytes on disk. **Record-object lifecycle**: Providers today inline `ensure_object_row`, `record_object_attempt`, `record_object_error` calls. A builder struct like `ObjectLifecycle { table, tx, id }.ensure().record(result?)` could reduce callsite boilerplate and enforce the always-paired invariant at the type level. **Effort**: medium. **Risk**: high if any provider has a non-standard lifecycle (unlikely).

- P1: I really would like to clean up how we express per data source configuration and code paths. **ExtractControl expansion via trait**: Instead of a fixed struct, `ExtractControl` could be trait-based so per-provider subclasses (`SlackControl { ... extra_slack_only_knobs ... }`) don't pollute the union. Today `control` is a catch-all that every provider ignores most of. **Effort**: medium. **Trade-off**: adds indirection vs. type-safe field access.

- P1: Low effort, big win. **Per-source narrative logging**: Duplicate `tracing::info!(source = %name, kind = ..., "extract pre-open: opening ...")` at sync/src/main.rs:1066–1071 and similar in translate phase. A shared `log_phase_step(phase, source, step)` could standardize the narrative. **Effort**: low. **Gain**: consistency + future audit trail unification.

### Missing shared affordances (unification opportunities)

- P1: This would be really great and may affect the schema of bytes at rest, at least the summary bytes. **No shared "sync run state machine" struct**: Every phase (extract, translate, load, qmd) tracks its own outcome structs (`PhaseOutcome`, `LoadOutcome`). A unified `RunPhase { name, status, error, stats }` with phase-specific extensions (e.g., `RowCounts { added, modified, removed }`) could reduce the handoff boilerplate at `sync/src/main.rs` lines 1200–1300+. **Precedent**: `FetchSummary` is per-provider; no cross-phase equivalent exists. **Impact**: Makes future status-line rendering, JSON summary generation, and interrupt handling more uniform.

- P2: Markdown rendering is not our highest priority. **No shared "blob attachment rendering" in non-CAS paths**: Contacts (signal) stores blobs inline in payloads, not in the CAS. Each provider re-implements the "materialize bytes to disk next to markdown" logic. A shared `materialize_inline_blobs(provider_type, parsed_structure)` could factor out the common file-write choreography. **Effort**: medium. **Trigger**: second provider with inline blobs (none yet planned).

- P3: We should never be parsing commit messages. That's crazy :)  But unifying the code sounds like a pretty good idea. **No `dolt_commit` message template library**: Currently every post-extract commit inlines the message format. If commit messages ever need audit-trail standardization or downstream parsing, a shared builder (e.g., `CommitMessage::extract(name, stats).render()`) would centralize it. **Today**: `extract <name>: <stats>` is tight, inline. **Future-proofing**: low priority but worth noting.

- P1: Standardizing this feels quite valuable. **No shared "retry policy" struct**: Each provider's config block mentions retry semantics, but no `RetryPolicy { max_attempts, backoff_ms, give_up_after_days }` struct exists. Per-source retry config (desired per §"Retry and fetch durability") would need one. **Effort**: medium, paired with config.yaml schema evolution. **Blocker**: `--retry-failed` CLI flag not yet wired.

-P1: I don't think I understand this and I don't like the name because I don't know what it means. **`ObsArgs` flattening not verified at compile time**: Every binary should flatten it. A procedural macro or lint rule could check — but Rust doesn't expose clap's `#[command]` macro to lint plugins. **Workaround**: training + doc / example in each provider template. **Not blockers**, but inconsistency risk is real for new providers.

### Checks: principle enforcement questions

**Does Load never re-parse markdown (sidecar JSON only)?** ✓ **YES**. The audit confirms `load.rs:505–509` reads the sidecar JSON from disk and parses it; the `.md` file itself is never touched. The `.md` is for humans; the sidecar is the machine contract. **Confirmed clean.**

**Does `obs::ObsArgs` get flattened uniformly?** ✓ **PARTIAL**. Sync (main.rs:179) and each provider's hypothetical binary do flatten it per the template, but no compile-time enforcement. Existing binaries are correct; new providers have the template. Minor unification gap; not a violation.

**Does `IndicatifWriter` work?** ✓ **WORKS** (but renamed). The codebase uses `IndicatifSink` (see `progress.rs` module) + `FanOut` (shared `progress.rs:157`). The pattern is sound: per-source bars, per-doc inner progress, no collision. Tested in sync/src/main.rs:760–765. **Confirmed in production form.**

**Is privacy boundary for spans articulated/enforced?** ✗ **NO**. Document §"Unresolved questions" / "Observability and the privacy boundary" explicitly states this is open. Code audit confirms no redaction layer. OTLP export exists but item contents *could* leak. **Not a violation**; the doc calls it out as deferred. No action needed yet, but it's a known gap.

P2: Is there code we could delete here? **Is `journal_mode=DELETE` actually used?** ✓ **ATTEMPTED BUT DOLTLITE REJECTS IT**. Code at `doltlite_raw.rs:256–259` shows the pragma is attempted but doltlite responds with "not configurable on doltlite-format databases." The fallback is implicit: doltlite manages its own chunk-store journal. No WAL sidecars appear on disk. **Principle is upheld; implementation delegated to doltlite internals.** Documented in the code comment.

**Are commit messages of shape `extract <name>: <stats>`?** ✓ **YES**. Confirmed at `sync/src/main.rs:1841`: `format!("extract {name}: {stats}")`. Index commits use a different shape (line 911–917) but that's Load, not Extract. **Confirmed clean.**

**Does orchestrator enforce single-commit-per-source-per-run?** ✓ **YES**, but only at the orchestrator level. Extract per-source code is single-writer (doltlite pool max_connections=1), so internal concurrent commits are blocked. The orchestrator's post-extract phase (lines 1839–1854) issues exactly one dolt_commit per source. **Confirmed clean.** The only gap is that a provider that *tries* to commit internally won't be prevented by shared code, only by policy + training.


---

## Anthropic (Claude)

### Principle violations & gaps

**Wire-fidelity: GOOD** — Conversations stored as raw `/api/...` payload (not pre-normalized).
  - Raw payload saved at fetch time in `conversations.payload` column (`db.rs:284-290`, `mod.rs:346`).
  - Normalization (`normalize_to_export_shape`) deferred to translate read-time (`parse.rs:18-19`).
  - No downloader-synthesized keys polluting payload; bookkeeping lives in separate columns.

**Pre-seed before fetch: IMPLEMENTED** — Conversations pre-seeded from listing before detail fetch.
  - `pre_seed_conversations()` creates rows with `payload=NULL` from listing metadata (`db.rs:214-250`).
  - Listing walk at `mod.rs:192-193` pre-seeds before the ordered fetch loop (`mod.rs:244`).
  - Bookkeeping row paired on pre-seed (`db.rs:243-247`).
  - But: users and orgs are NOT pre-seeded. Orgs are discovered from `/api/organizations` and saved on first fetch (`mod.rs:142-144`), users either from export or from `/api/account` fetch (`mod.rs:110-135`). No pre-seed row with `payload=NULL` before the fetch attempt.

**Retry and durability: PARTIALLY IMPLEMENTED** — Error recording exists, retry-on-by-default does not.
  - `record_conversation_error()` records error in `*_bookkeeping` via `dr::record_object_error()` (`db.rs:308-319`).
  - `upsert_conversation_detail()` calls `dr::record_object_attempt()` on success (`db.rs:301`).
  - But: no dedicated retry-failed walk in the extract loop. Conversations with `last_error IS NOT NULL` are not re-attempted within a run or between runs. The architecture doc (§446-488) calls this "retry-on-by-default"; anthropic does not implement it.
  - No `--retry-failed` flag plumbed to extract; retry logic must live in orchestrator (sync/main.rs), not in provider.
P2: Building this retry failed mechanism would be useful, but I'm glad we have the state to implement it. 

**404 handling (transient vs deletion): INCOMPLETE** — 404 on a missing conversation is logged but not marked distinctly.
  - Single-conversation mode (`fetch_single()`) treats 404 as "wrong org, continue" (`mod.rs:315-322`), not as "deleted upstream."
  - No `deleted_upstream_at` column on conversations table, so a conversation that was deleted upstream and re-seeded with `payload=NULL` would look indistinguishable from a transient fetch failure.
  - Architecture doc (§497-510) calls for a distinct `deleted_upstream_at` marker; anthropic lacks this.
P1: Let's actually do this because it affects the schema of the bytes on disk. 

**Timestamp discipline: MOSTLY GOOD** — ISO-8601 with offset for when_ts, microsecond-bump for blocks.
  - Message `when_ts` drawn from `created_at` directly (`grid_rows.rs:131`).
  - Block timestamps synthesized via `bump_micros()` for microsecond ordering (`grid_rows.rs:37-50, 173-176`).
  - But: Chat row `when_ts` uses `created_at` OR `updated_at` fallback with no explicit offset normalization (`grid_rows.rs:230-234`). If an upstream timestamp is naive or bare-Z, it passes through; no validation at translate time to enforce ISO-8601 with explicit offset.
  - No evidence in the codebase of "Strict ISO-8601 with offset, not bare Z or naive" being checked.
  P3: I don't think we should make up time zones if they weren't given to us. 

**Cursor strategy: IMPLEMENTED — Forward-walk + refresh window.**
  - Listing walk + overlap-forcing most-recent N conversations, then re-fetching missing and stale (`mod.rs:157-265`).
  - No checkpoint file; dedup via PK on UPSERT (`db.rs:226-231`).
  - Matches architecture doc pattern (§314-327) for Anthropic shape.

**Commit lifecycle: COMPLIANT** — Provider does not call `dolt_commit` directly.
  - Extract delegates to orchestrator via `run.finish()` after work completes (`mod.rs:274`).
  - One commit per run, wrapping the entire fetch work.

**Incremental dedup: GOOD** — Sidecar fingerprint + translate-side skip.
  - `fingerprint_for_conversation()` hashes normalized payload + RENDER_VERSION (`grid_rows.rs:281-286`).
  - Sidecar header carries `source_fingerprint`; load phase skips unchanged sidecars.

**Blob handling: GOOD** — Blobs stored in CAS, retry on failure recorded.
  - `fetch_files_for()` walks `chat_messages[*].files[]` and downloads to `blob_cas::store_bytes()` (`mod.rs:391-432`).
  - `blob_exists()` check skips already-cached blobs (`mod.rs:416-419`).
  - `record_blob_error()` logs failures in `blob_refs_bookkeeping` (`mod.rs:425-429`).

---

### Dead/cargo-culted patterns

**Vestigial single-conversation mode (`--conv-uuid`)** — `fetch_single()` at `mod.rs:279-337` and bin flag at `anthropic_download.rs:50-51`.
  - Useful for manual spot-checks but orthogonal to incremental sync. No tie-in to retry-failed buckets or to prioritizing failed conversations.
NOTE: We actually want this because it's useful for small tests.
  - If a conversation has `last_error IS NOT NULL`, there's no automatic path to re-fetch it via this flag without manual UUID extraction.

**Export seeding as primary path** — `--export-dir` bootstraps from deprecated bulk-export format.
  - Legacy support is good; doc (EXTRACT.md:8-9) frames it as "existing translator consumes either source indistinguishably."
  - But the path is asymmetric: export can seed users + conversations, live API cannot. A user starting fresh with only live API gets no users until `/api/account` is fetched, and that's late-pipeline (after org listing).
TO CLARIFY: Is this about ingesting from a flawed data export? Or something else. I don't think I totally understand. 

---

### Simplification opportunities

**Pre-seed users and orgs too** — Currently users and orgs are discovered in-fetch, not pre-seeded.
  - Users: seeded from export if present, else fetched from `/api/account` (`mod.rs:110-135`). No pre-seed row.
  - Orgs: fetched from `/api/organizations` before listing walk (`mod.rs:137-144`). No pre-seed row.
  - Both could be pre-seeded from a prior sync's state (next-run opening the DB sees rows from last run) before any fetch, making error handling uniform across all three tables.
  - Would unify the three table handlers under a common "list, pre-seed, fetch detail, record error" flow.
CLARIFY: Unifying flows sounds good, but I imagine if someone is using a exported data set that they would not have yet run against the API, And thus these tables would not exist yet. 

**Consolidate error recording paths** — `record_conversation_error()` and `record_blob_error()` both exist but differ in signature and wrapping.
  - Could factor into a shared helper that wraps `dr::record_object_error()` / `blob_cas::record_ref_error()` with consistent transaction handling.
P1: Unifying this data flow sounds good, especially if we could do it for all of the data sources. 

**Timestamp validation at translate time** — No explicit check that `when_ts` meets the ISO-8601 + explicit-offset requirement.
  - A regex or parser in `grid_rows.rs` could validate each timestamp and log/warn when upstreams violate the discipline.
  - Current approach silently passes through bare-Z or naive timestamps, relying on luck.
P1: Let's do log warnings about this and make sure that warnings make it all the way through to the synchronization log that we write at the end. 

**Unify `--conv-uuid` into orchestrator retry** — Single-conversation fetch is a manual escape hatch.
  - If the orchestrator's `--retry-failed` implementation were provider-agnostic and learned to fetch specific upstream IDs (not just "retry all"), the anthropic provider would not need a separate `--conv-uuid` flag.
NOTE: Again, we want to keep this flag, It's useful for testing. 

---

### Cross-source sharing opportunities

**Chat (LLM) family: Claude + ChatGPT + planned Gemini** — All three project conversations → messages → content blocks.

1. **Shared raw table schema** — Users, Orgs, Conversations, Messages, ContentBlocks, Attachments.
   - Anthropic has no Messages or ContentBlocks tables (they're exploded at translate time from `chat_messages` payload).
   - ChatGPT (`providers/chatgpt/src/extract/db.rs`) may differ.
   - Recommend: define a shared `frankweiler_etl::chat_llm` schema crate (users, orgs, conversations) and let each provider extend it with provider-specific columns (e.g. `conversations.model`, `anthropic_project_id`).
P2 & NOTE: Again, the raw data should be very specific to each provider. It is only once we start translating data that we need a shared schema. And I'm not sure that even needs to be a SQL schema. It might be a native Rust schema for now. Let's wait on this. 

2. **Shared translate shape** — Messages, blocks (thinking, tool_use, tool_result) projected to uniform `GridRow` kinds.
   - Anthropic `grid_rows.rs` produces: Chat, User Input, LLM Response, LLM Thinking, Tool Call.
   - ChatGPT likely produces Message → role (user/assistant) mapping.
   - `section_uuid_for_block()` (`render.rs:68-70`) suggests there's already a shared naming scheme for block anchors.
   - Recommend: extract the GridRow emission loop into `frankweiler_etl::chat_llm::translate` so both providers reuse it, parameterized by provider name and model lookup.
P2: This sounds like a really good idea. 

3. **Shared sidecar structure** — Identical: markdown with message divs, grid_rows JSON sidecar.
   - Already unified at Load time.
   - Opportunity: if raw tables were shared, translate could be shared too, eliminating `anthropic_translate` as a separate binary.

4. **Blob handling parity** — Both providers attach file/image blobs to messages.
   - Anthropic: `blob_refs` keyed by `file_uuid`, downloads via `preview_url` / `document_asset.url`.
   - ChatGPT: likely different field names (e.g. `file_id`).
   - Shared blob CAS is already there; recommend shared `BlobRef` table DDL so cross-source GC works.
CLARIFY: Where would you put this shared blob ref table? Actually, I don't think it makes sense. Each system will reference blobs in its own scheme. As long as we CAS the blobs, we are okay, right? 

**Retry-on-by-default as orchestrator feature** — Currently absent in anthropic extract; should be shared machinery.
   - Orchestrator should own the `--retry-failed` loop: before any normal fetch, query all providers' `*_bookkeeping` tables and feed failed object IDs back to each provider's extract as a `retry_ids: Vec<String>` in `FetchOptions`.
   CORRECTION: I don't think this is right. I think it is each individual data source's responsibility to do retry logic. 
   - Anthropic's `fetch()` would then check if it's in a retry phase and skip the listing walk, jumping straight to detail fetches.
   - Eliminates per-provider retry logic; applies uniformly to Slack, GitHub, etc.

---

### Summary

Anthropic is **well-aligned with the architecture** on wire-fidelity, pre-seed, UPSERT dedup, commit boundaries, and cursor strategy. The main gaps are:

1. **No retry-on-by-default within extract.** Error rows are recorded but not re-fetched in a dedicated retry pass.
2. **Users and orgs are not pre-seeded,** creating asymmetry with conversations.
3. **404 handling lacks a `deleted_upstream_at` distinction** from transient failures.
4. **Timestamp validation at translate time** — no check that when_ts meets ISO-8601 discipline.
5. **Opportunity to unify ChatGPT + Gemini** under shared raw schema + translate code.

No pre-normalization pollution (good), no checkpoint files (good), proper blob CAS integration (good). Recommend addressing gaps 1–2 in a follow-up pass, and 3 as part of broader schema evolution.

---

## Beeper

### Principle violations

#### 1. No cursor or resume strategy (local-data only)
- **Violation**: Architecture demands "Cursor / resume strategy" with forward-walk + refresh window or time-windowed sampling patterns. Beeper has neither.
- **Details**: 
  - Beeper reads a local SQLite cache (`index.db`) and optional megabridge databases. These are not network endpoints with cursors or pagination.
  - No `max(ts)` tracking, no refresh windows, no incremental walking.
  - `upstream_cursor: None` hardcoded in render.rs:160 — acknowledged as "no provider-side cheap-probe signal today."
  - Architecture applies to Slack, Anthropic, GitHub, etc. which have live APIs; Beeper is fundamentally different (archive-like, local read-only).
- **Status**: By design. Acceptable for local-data providers, but breaks the incremental-delta assumptions the architecture makes.
- **File:line**: extract/mod.rs:72, translate/render.rs:160

#### 2. No pre-seeding before fetch; no payload IS NULL retry pattern
- **Violation**: Architecture demands pre-seeding rows with NULL payload at the moment the ID is discovered, enabling "payload IS NULL" retry logic on next run.
- **Details**:
  - Beeper's extract walks index.db.threads, mx_room_messages, mx_reactions, participants — all at once.
  - No separate listing pass that discovers IDs before fetching detail. Rows are inserted only when data is available.
  - No NULL payload rows exist to signal incomplete fetches.
  - Megabridge enrichment (enrich_one) is an UPDATE, not a pre-seed + re-attempt pattern.
- **File:line**: extract/index_db.rs:138–220 (ingest function), extract/megabridge.rs:219 (UPDATE not retry)

#### 3. No dedicated retry-on-by-default mechanism
- **Violation**: Architecture requires `--retry-failed` flag and per-row retry state (`last_error`, `attempt_count`).
- **Details**:
  - Beeper calls `record_object_attempt` (extract/db.rs:316, 351, 404) unconditionally on every upsert.
  - No distinction between "this was successful" vs "this failed, record the error."
  - No `last_error` column checked on next run to retry only failed rows.
  - Megabridge doesn't fail gracefully — if a megabridge.db is malformed, the whole enrich_one returns Err and logs a warn (megabridge.rs:167–174), but doesn't record per-row durability.
- **File:line**: extract/db.rs:316, 351, 404 (unconditional record_object_attempt), megabridge.rs:167
P3: There's no API to read from, so this doesn't matter as much. We should just make sure we are logging errors so we can deal with them later. 

#### 4. Timestamps lack explicit UTC offset in ISO-8601 output
- **Violation**: Architecture demands "Strict ISO-8601 with offset, not bare Z or naive."
- **Details**:
  - `iso_from_ms()` in translate/render.rs:489–492 calls `to_rfc3339_opts(SecondsFormat::Millis, true)`.
  - The second argument `true` is `use_z: bool` — when true, emits `Z` suffix instead of explicit `+00:00`.
  - Example output: `2023-06-15T14:23:45.123Z` instead of `2023-06-15T14:23:45.123+00:00`.
  - Violates the explicit-offset rule: "A naive timestamp can't be globally sorted alongside a `+02:00` one without a hidden timezone assumption."
- **Status**: All Beeper rows (docs, messages, reactions) affected.
- **File:line**: translate/render.rs:489–492 (iso_from_ms)
P1: Let's fix this. 

#### 5. No explicit null-out for entities without time-shape; docheader when_ts ambiguous
- **Violation**: Architecture says entities without time-shape should "null-out `when_ts` or use the same sentinel everywhere" consistently.
- **Details**:
  - Beeper's conversation-header GridRow (translate/render.rs:628) sets `when_ts: iso_from_ms(doc.first_ms)`.
  - `doc.first_ms` is MIN(events.timestamp_ms) over all messages in the period bucket, not a conversation creation time.
  - If the conversation has no messages (edge case), `first_ms` defaults to docparse's MIN over empty set (likely 0 or a sentinel).
  - Not explicitly documented which time-shapes are event-shaped vs metadata; no comment marking this choice.
- **File:line**: translate/render.rs:628

#### 6. Weak external_id backpointers for messages; source_url absent for most GridRows
- **Violation**: Architecture demands "Backpointers and outlinks are first-class" and source_url should be "the canonical URL on the provider's web UI."
- **Details**:
  - `external_event_id` is pre-populated as NULL in index.db (extract/db.rs:112, noted as "NOT populated from index.db").
  - Megabridge enrichment (megabridge.rs:219–229) UPDATEs `external_event_id` post-hoc but doesn't surface errors durably.
  - GridRow source_url is:
    - None for conversation-header rows (render.rs:653)
    - Pulled from `blobs.first().source_url` for messages (render.rs:681) — wrong: source_url is the attachment URL, not the message permalink.
    - None for reactions (render.rs:717)
  - No Beeper web UI URL constructed (like Slack's "https://…" permalinks).
- **File:line**: extract/db.rs:112, megabridge.rs:219, render.rs:653, 681, 717
P3: I think this may actually be hard to do in Beeper, but we should leave ourselves a note to investigate. 

#### 7. No orchestrator commit boundary enforcement (local provider)
- **Violation**: Architecture demands "Provider never calls dolt_commit; orchestrator wraps each source's extract in exactly one commit."
- **Details**:
  - Not applicable to Beeper directly — Beeper is local-read, not network-fetch, so there's no "per-page" upsert boundary issue.
  - But it does rely on the orchestrator's wrapping, which is fine.
  - This is NOT a violation, just a note that Beeper's constraint is orthogonal.
- **Status**: OK by design

### Dead patterns / cargo-culted code

#### 1. Unused synthesize module
- **Details**: synthesize.rs:29 returns `SynthesizeReport::default()` unconditionally.
- **Justification given**: "synth is only needed when we wire Beeper into the hermetic Bazel genrule path."
- **Issue**: This is placeholder code that doesn't do anything. Not a violation, but signals incomplete integration.
- **File:line**: src/synthesize.rs (all of it)
CLARIFY: Is this synthesize about creating a fixture to play back through the test harness? 

#### 2. Unused sync_runs / sync_scope_state tables
- **Details**: EXTRACT.md:60–62 states "The shared `sync_runs` / `sync_scope_state` tables that every doltlite raw store carries are present but unused for Beeper, since we don't have a remote endpoint to checkpoint against."
- **Why**: Beeper has no remote cursor to track, so these bookkeeping tables serve no purpose.
- **File:line**: EXTRACT.md:60–62
P2: I'm not sure this is 100% true. We could still record our sync progress and the last timestamp we saw, for example, so that if we could order the input table by time zone, we could work forward from that.  This would be a P1, but beeper is not the highest priority. 

#### 3. `events_orphaned` counter in FetchSummary without retry machinery
- **Details**: FetchSummary tracks `events_orphaned` (megabridge rows that don't match any index.db row).
- **Issue**: No actionable retry path. Orphaned rows are logged and counted but not durably stored for later processing.
- **File:line**: extract/mod.rs:99–102

### Simplification opportunities

#### 1. Timestamp precision: milliseconds are overkill
- **Details**: Every timestamp is `i64` milliseconds (extract/db.rs:114, translate/parse.rs:46).
- **Beeper reality**: Index.db columns like `timestamp` are UNIX timestamps in milliseconds from the desktop app's perspective, but the upstream systems (Signal, Slack, Google Chat) may have coarser granularity (second-level).
- **Simplification**: Store as seconds, convert at render time if needed. Saves precision lost anyway and simplifies the mental model.
- **File:line**: extract/db.rs:114, translate/parse.rs:46
P3: CLARIFY: I wonder if this was done to be consistent with other systems. 

#### 2. Remove megabridge pass if index.db becomes authoritative
- **Details**: Megabridge enrichment (extract/megabridge.rs) only fills `external_event_id` for local bridges.
- **Beeper's vision**: If the desktop app eventually stores `external_event_id` in index.db (as part of Beeper caching), the megabridge pass becomes obsolete.
- **For now**: Needed. But flag it as a future simplification.
- **File:line**: extract/mod.rs:168–172 (megabridge call)
P4: We invested carefully and the index.db was not complete. 

#### 3. Network filtering could be pushed to SQL
- **Details**: `account_patterns_for()` and `matches_network()` in extract/index_db.rs do pattern matching in Rust after pulling all threads from the DB.
- **Simplification**: Use SQL WHERE clause with LIKE or GLOB patterns, pull only matching rows.
- **Benefit**: Smaller payload from sqlite3 CLI call, cleaner logic.
- **File:line**: extract/index_db.rs:148–184 (thread row filtering loop)
P3: Optimizing beepers fetch pattern is not highest priority. CLARIFY: I'm curious why we are even doing this. Is it to only fetch a subset of the data as described in the config? 

### Cross-source sharing (Slack / Signal family)

#### 1. UUID namespace is provider-specific; no collisions with Signal/Slack
- **Status**: Good.
- **Details**: UUIDv5 namespace `BEEPER_UUID_NS` is distinct from Slack/Signal namespaces (translate/mod.rs:24–26).
- **File:line**: translate/mod.rs:24–26

#### 2. GridRow projection is unified; `kind` taxonomy needs alignment
- **Issue**: Beeper emits kinds like "Signal User Input", "Google Chat Message" — network-prefixed.
- **Slack comparison**: Slack emits kinds like "Channel Message", "Thread Reply" — network-agnostic.
- **Principle**: Architecture lists "Chat (human) — Slack, Beeper, Signal" as a unified family.
- **Problem**: Beeper's GridRow.kind includes the network name (render.rs:625, 665, 701), which breaks cross-network UI filtering.
- **File:line**: translate/render.rs:625 (kind_for_conversation), 665 (kind_for_message), 701 (reaction kind)
- **Example mismatch**: 
  - Slack: `kind = "Channel Message"`
  - Beeper: `kind = "Signal Message"` or `kind = "Google Chat Message"`
  - These won't deduplicate or group together in the UI.
P2: I think this is worth fixing. Let's have all of them just be "Chat Message"

#### 3. `source_label` is composite "Beeper:Signal" by design
- **Status**: Intentional.
- **Details**: Render.rs:618 sets `source_label = format!("Beeper:{}", network_label(&room.network))`.
- **Rationale given** (render.rs:610–617): Allows `LIKE 'Beeper:%'` to pull everything from Beeper, or `LIKE '%:Signal'` to pull Signal from any source.
- **Future concern**: Once a direct Signal reader is added (not via Beeper), it would emit `kind = "Signal Message"` and `source_label = "Signal"`, which won't align with Beeper's "Signal Message" + "Beeper:Signal".
- **File:line**: render.rs:618
I think this is okay for now. 

#### 4. Sidecar format is shared; fingerprint includes network + room scope
- **Status**: Compliant.
- **Details**: Sidecar emitted as `Sidecar { header: SidecarHeader { markdown_uuid, source_fingerprint, render_version }, rows: [GridRow, …] }` (render.rs:138–146).
- **Fingerprint** (render.rs:196–222): includes RENDER_VERSION, room_uuid, period_key, and per-message/reaction details. Stable across re-renders.
- **File:line**: render.rs:138–146, 196–222

### Summary of severity

| Issue | Severity | Impact |
|-------|----------|--------|
| No cursor / resume strategy | Design | Acceptable for local-data providers; doesn't apply to Beeper's use case |
| No pre-seeding + retry pattern | Medium | Loss of durability for partial fetches (not applicable to local index.db, but megabridge enrichment could fail durably) |
| No dedicated retry mechanism | Medium | Can't distinguish transient vs permanent errors; every upsert is treated as "attempted" |
| Z vs explicit offset in ISO-8601 | Low | Functional but violates explicit offset rule; will sort correctly despite bare Z |
| No entity-without-time-shape handling | Low | Conversation headers use first_ms, which is defensible but not documented |
| Weak external_id + missing source_url | Medium | Loses backpointer fidelity; renderers can't construct UI links back to Beeper/upstream |
| GridRow kind includes network name | Medium | Breaks unified "Chat" family cross-network grouping with Slack/Signal |

### Recommendations

1. **Urgent**: Fix ISO-8601 timestamps to emit `+00:00` instead of `Z` (render.rs:491, `use_z=true` → `false`).
2. P4: I'm not even sure this is possible with Beeper. **Medium**: Construct source_url for messages (e.g., Beeper Texts web UI or upstream link if available); populate for reactions.
3. **Medium**: Align GridRow.kind taxonomy with Slack/Signal (remove network prefix from kind, keep it in source_label only).
4. **Nice-to-have**: Durably record per-row errors from megabridge enrichment failures so they can be retried.
5. **Nice-to-have**: Push network filtering to SQL WHERE clause for cleaner extract logic.

---

**Audit date**: 2025-06-09
**Auditor**: Claude Code
**Provider crate**: `frankweiler_etl_beeper` at `frankweiler/backend/etl/providers/beeper/`

---

## ChatGPT

### Principle violations

- **No pre-seeded bookkeeping on conversations.** Architecture demands pre-seed-before-fetch + paired `<table>_bookkeeping` (attempt_count/last_attempt_at/last_error). ChatGPT creates `conversations_bookkeeping` rows in `pre_seed_conversations()` (db.rs:196–200) but only on the *listing* pass, not before the detail fetch. If a detail fetch crashes, a pre-seeded row with `payload IS NULL` won't surface next run to trigger a retry walk. Only subsequent explicit re-fetch via listing will catch it. *Violation*: pre-seed should happen before *any* fetch, including detail.
  - File: `/frankweiler/backend/etl/providers/chatgpt/src/extract/db.rs:166–203` (pre_seed only stamped at listing time)
  - Workaround exists but suboptimal: `--retry-failed` will re-walk failed conversations if they're re-listed, but won't catch conversations dropped mid-fetch in a run that got OOM'd.
P1: This seems worth fixing soon. 

- **404 handling absent.** Architecture specifies 404 → `deleted_upstream_at` marker. No code path records 404s or distinguishes them from transient errors; all failures go to `record_object_error()` identically. A conversation deleted upstream will be retried forever.
  - File: `/frankweiler/backend/etl/providers/chatgpt/src/extract/mod.rs:268–272` (Permanent error returns from API but no 404 check)
P1: We should at least be recording that we are getting 404s so that we don't try to fetch forever. 

- **No explicit retry-on-by-default orchestrator binding.** Architecture says "the orchestrator takes a flag `--retry-failed` (default `true`)." ChatGPT extract has no `--retry-failed` knob wired; the retry logic exists in `db::failed_conversation_ids()` but nothing calls it or exposes the user-facing flag. A second run starts fresh from listing regardless of prior failures.
  - File: `/frankweiler/backend/etl/providers/chatgpt/src/extract/mod.rs` (no attempt to load and re-walk failed conversations)
  - Workaround: `--reset-and-redownload` forces a full re-fetch, but that's all-or-nothing, not incremental retry.
P2: The most important thing to do is to ensure that we have the right data written to support this feature, which doesn't exist yet. 

### Dead patterns / cargo-culted code

- **Legacy synthetic keys (`_fetched_at`, `_listing_update_time`) documented but already migrated.** EXTRACT.md (lines 13–16) describes old JSON-tree layout with synthetic keys in payloads. These *were* promoted to real columns (`fetched_at` in bookkeeping, `last_listing_update_time` as a real column), but the doc reads as if the migration is still in flight. The synthesizer (synthesize.rs:48–54) explicitly *strips* these keys on serve, so payloads are wire-faithful. No bug here, but the docs are stale.
  - File: `/frankweiler/backend/etl/providers/chatgpt/EXTRACT.md:13–16` (outdated description of pre-migration state)
  - File: `/frankweiler/backend/etl/providers/chatgpt/src/synthesize.rs:48–54` (correctly strips keys)
P2: Yes, let's update the docs. 

- **Unused `record_object_error` on fetch failures is asymmetric.** Permanent errors call `db.record_conversation_error(cid, &msg)` (mod.rs:270), but rate-limit give-up (mod.rs:259–266) silently exits without recording anything durable. Next run will re-list and may hit the same rate limit again if it hasn't reset. Should either record the error or auto-pause the run gracefully.
  - File: `/frankweiler/backend/etl/providers/chatgpt/src/extract/mod.rs:259–266` (no durable marker on rate-limit giveup)
P2: I think we should auto pause gracefully. 

### Simplification opportunities

- **Duplicate timestamp normalization logic across grid_rows and render.** `bump_micros()` (grid_rows.rs:41–55) and `bump_iso()` (render.rs:34–50) are near-identical, both converting bare `Z` to explicit `+00:00` offset and then bumping microseconds. They diverge slightly in output format — one uses `%.6f%:z`, the other uses `chrono::SecondsFormat::AutoSi`. A single shared `TimestampBumper` helper in the translate module would eliminate the divergence.
  - File: `/frankweiler/backend/etl/providers/chatgpt/src/translate/grid_rows.rs:41–55`
  - File: `/frankweiler/backend/etl/providers/chatgpt/src/translate/render.rs:34–50`
P1: I think we should extract out a timestamp utils crate and use it everywhere we handle timestamps, which I think is in both extract (to populate the DB column) and translate (to render them).

- **Message ordering re-implemented in two places.** `rows_for_conversation()` (grid_rows.rs:74–84) sorts messages by `(create_time, message_id)` for the sidecar. `render_one()` (render.rs:202–227) re-sorts them differently via `current_node` parent-chain walk with a fallback to create_time. Same data, different sort keys, separate code paths. Render's parent-chain walk is intentional (to respect conversation branching), but this should be documented as a deliberate difference, not silently duplicated.
  - File: `/frankweiler/backend/etl/providers/chatgpt/src/translate/grid_rows.rs:74–84`
  - File: `/frankweiler/backend/etl/providers/chatgpt/src/translate/render.rs:202–227`
  - Opportunity: factor the sorting logic into a reusable helper, or document why render's ordering must differ from grid_rows.
P1: I do think this should probably be unified.  Is message ID a auto increment index? That doesn't seem good. Hopefully we are enumerating the messages as we store them into a database so that we can sort by that enumeration. Maybe that's what message ID is. If so, I think that's what we should always sort them by. 

- **Fingerprint hash recomputes canonical JSON every render.** `fingerprint_for_conversation()` (grid_rows.rs:179–184) calls `canonical_json()` which canonicalizes the entire upstream payload. This happens on every render pass, even if the conversation is marked for skipping. Move the canonicalization to extract time and cache it as a column, so translate can fingerprint-skip in O(1) hash comparison instead of O(payload bytes).
  - File: `/frankweiler/backend/etl/providers/chatgpt/src/translate/grid_rows.rs:179–204`
  - Optimization: add a `fingerprint` TEXT column to `conversations` table, populate at upsert time, skip render entirely if cached fingerprint matches prior.
P0: This sounds like a generic bookkeeping thing that we might want to investigate having literally everywhere (all the bookkeeping tables) so that we can quickly recognize whether we have already translated a particular version of some entity or not.

### Cross-source sharing opportunities

- **LLM chat row types unify with Anthropic but diverge at render layout.** Both ChatGPT and Anthropic emit `GridRow` with `kind = 'User Input' | 'LLM Response' | 'LLM Thinking'` (grid_rows.rs:30–38 mirrors Anthropic's pattern). Both pull from LLM conversations. Both strip sentinel characters and render to markdown with inline code blocks. But ChatGPT uses `rendered_md/openai/<account>/llm_chats/<conv_id>/index.md` while Anthropic likely uses `rendered_md/anthropic/...`. And Anthropic's render path (synthesize, dedup, blob materialization) likely differs slightly.
  - Opportunity: extract a shared `LlmChatRenderer` trait or module that both Anthropic and ChatGPT (and future Gemini, Claude Web) can plug into. Table DDL, bookkeeping schema, message-row fingerprinting, timestamp bumping, attachment handling, markdown formatting — all should be unified.
  - Files: 
    - `/frankweiler/backend/etl/providers/chatgpt/src/translate/grid_rows.rs:30–38`
    - `/frankweiler/backend/etl/providers/chatgpt/src/translate/render.rs:1–87` (render layout)
    - (Anthropic equivalent would be in `/providers/anthropic/src/translate/`)
P1: At translate time, I actually think we should extract a shared Rust data type to encapsulate everything we want to render about any kind of LLM chat (Claude, ChatGPT, Gemini, etc.) and then translate all of the chats into that object and then pass that to a render function that knows how to render it to markdown and turn it into rows for the index.

- **Sidecar schema + grid_rows JSON are re-produced per provider.** ChatGPT emits `Sidecar { header: { markdown_uuid, source_fingerprint, render_version }, rows: Vec<GridRow>, edges: Vec<> }` (render.rs:148–156). Every provider that follows the translate pattern reinvents the same struct. This should be in `frankweiler_etl::sidecar` as the cross-provider contract, not re-declared.
  - File: `/frankweiler/backend/etl/providers/chatgpt/src/translate/render.rs:148–156`
  - Current: already uses `frankweiler_etl::sidecar::Sidecar` (import at line 13), so no violation — this is already unified. Good.
P1: The comment above I think would help fix this.

- **Attachment handling (file_id → signed URL → bytes) mirrors Anthropic.** Both providers:
  1. Scan conversation mapping for attachment refs (ChatGPT: metadata.attachments + asset_pointers; Anthropic likely similar)
  2. Dedupe by file_id
  3. Fetch metadata (two-hop: auth-required metadata fetch → signed URL)
  4. Download signed URL via latchkey curl
  5. Store in blob CAS
  6. Record attempt/error in blob_refs_bookkeeping
  - This dance is provably correct but verbose. Extract to a shared `BlobFetcher` helper in `frankweiler_etl::blob_cas` or a new `frankweiler-etl-llm-common` crate.
  - File: `/frankweiler/backend/etl/providers/chatgpt/src/extract/mod.rs:298–391` (fetch_attachments_for + download_one_file)
P4: I'm not sure different API sources will be able to share the same blob fetching mechanism in general, but we should make sure that they store their blobs in consistent ways. 

- **Account/identity stamping: `me` endpoint → per-row account_id.** ChatGPT upserts `/me` once per run (extract/mod.rs:124–133), then stamps `account_id` on every conversation row. Anthropic does the same. This pattern should be a shared template: "call the identity endpoint, store account metadata, use its id as the shard key for per-account rows."
  - File: `/frankweiler/backend/etl/providers/chatgpt/src/extract/mod.rs:124–133`
  - Opportunity: factor into a `IdentityFetcher` helper that both providers can reuse.
P4: I'm not sure it matters so much that these are identical because they will have slightly different semantics. 


---

## Contacts

### Principle violations

- **Photo CAS/blob_refs entanglement (VIOLATION)**: Despite the doc's explicit exemption stating "photo bytes inline in payload as base64 → decoded once at parse → written straight to `blobs/<uid>.<ext>` at render. They never touch `blob_refs` or `cas_objects`", the architecture comment in `extract/db.rs:9-10` correctly notes contacts never populate `blob_refs`, but the render.rs implementation writes photos to a `blobs/` subdirectory at `rendered_md/contacts/<source>/<addressbook>/blobs/<uid>.<ext>` (render.rs:390-396), which deviates from the expected pattern. Photos should be written directly as sibling artifacts, not into a nested `blobs/` folder that suggests secondary organization.
P1: Actually, I think maybe the best thing would be when we render these photos to just make them inline images since they are relatively small. That seems like simplest thing to me. Does markdown support inline images? 

- **Sidecar header field mismatch (MINOR)**: The doc specifies sidecars carry `document_uuid`, `source_fingerprint`, `render_version` in the header. Contacts render.rs:150-152 emits `markdown_uuid` instead of `document_uuid`. This is inconsistent with the `Sidecar` schema contract in `etl/src/sidecar.rs` (line ~375 of data_arch.md).
P0: Can we not introduce a shared struct that everyone has to populate for these sidecar fields? It could share the same schema as a sidecar row.

### Dead patterns

- **Unused blob_refs truncate (CARGO CULT)**: extract/mod.rs:78-83 calls `frankweiler_etl::doltlite_raw::truncate_blob_refs` when `refetch_blobs` is set, with a comment explicitly acknowledging "Contacts doesn't populate `blob_refs` (photos travel inline in the vCard payload)" — this is harmless but unnecessary code that could be gated behind a provider check or removed entirely.
P2: Yes, I think it is fine that context keeps its blobs in line because they are so small and we can just not invoke these code paths that don't matter. 

- **Unused `payload` fields in accounts/addressbooks tables (MINOR OVERHEAD)**: Both tables (db.rs:64-78) carry a `payload TEXT NULL` column that are populated but never read back by extract or translate. The payload is written (db.rs:164, 213-214) but the sync/reset paths don't query them; keeping them costs storage and serialization for zero reader value.
WAI: I think this is for consistency. 

### Simplification opportunities

- **Photo write could fail silently (SILENT ERROR)**: render.rs:118-121 calls `write_photo().ok()` and continues if photo write fails. This matches the stated intent ("skip the embed") but there's no bookkeeping or retry mechanism. A large photo that causes disk-full should ideally record `last_error` so the user knows why the photo is missing on a re-render. Currently it's a log line only, no durable evidence.

- **Etag-walk fallback unimplemented**: extract/mod.rs:226-238 logs a warning and returns `Ok(())` when `sync-collection` is unsupported, silently dropping the addressbook instead of falling back to the ctag-check + etag-walk mentioned in the comment. This treats a 405/501 as a noop rather than degrading to a working sync strategy.
P3: This sounds like a nice to have improvement.

- **UUID derivation for missing UID is path-dependent**: parse.rs:320-323 synthesizes a UID from `(addressbook, file_stem, block_index)` when the vCard omits one, but extract/mod.rs:263-269 would fail on a missing UID from CardDAV. For consistency, extract should also synthesize the same fallback so a vCard from disk and a vCard fetched over CardDAV land at the same UUID.
P1: I am not sure these will be stable enough to materialize UIDs. I actually think we should just warn loudly in warnings that make it all the way through to the JSON summary about contacts that do not have UUIDs and not ingest them. If it doesn't have a UUID, it doesn't really have an identity. 

### Cross-source sharing

- **Contact UUID derivation is stable but source-specific**: translate/mod.rs:40-45 derives contact UUIDs as `uuidv5(CONTACTS_NS, 'contact:{account}:{addressbook}:{uid}')`. The namespace is global (`contacts_uuid_ns()`), making the scheme suitable for deduping contacts across multiple CardDAV sources (e.g., Apple + Fastmail). The implementation is correct, but the doc doesn't surface this design as an example of cross-source dedup strategy.
P1: This sounds like fallback code that is waiting to bite us.  Let's just error more loudly. 

- **Addressbook UUID groups by `(account, label)` — clean for multi-source merging**: translate/mod.rs:50-55 groups all contacts in an addressbook under a single `conversation_uuid`, keyed on `(account_id, addressbook_label)`. Two sources with addressbooks named "Personal" will have distinct UUIDs (`Personal` under Apple and `Personal` under Fastmail don't collide). This is correct but could be documented as a working multi-account example.

### Cursor / resume strategy

- **Sync-token cursor is fine-grained but fragile**: extract/mod.rs:133 persists one `sync_token` per addressbook (via `db.set_sync_token(book_id, token)`). This is efficient (only re-fetch changed contacts in the addressbook) but if a sync crashes mid-addressbook, the cursor isn't advanced until all contacts in that batch are upserted (extract/mod.rs:242-246). This matches the "cursor only after success" pattern, but there's no pre-seeding or checkpoint for per-contact failures within the batch.

- **No pre-seed before CardDAV fetch**: The doc's "pre-seed before fetch" rule (data_arch.md §452-468) states rows should be created the moment we learn the upstream identifier. Contacts extract does not pre-seed; contacts arrive only after a sync-collection REPORT succeeds. For providers like Anthropic/Notion that list then detail-fetch, pre-seed is natural; for CardDAV, the REPORT returns payloads inline so pre-seed would require two passes. This is justified by the API shape but worth calling out explicitly.
P4: I think contacts are small enough that we can always just refetch all of them. The cursor thing is probably overkill anyway.

### Timestamps (when_ts)

- **Correctly nulls when_ts for non-event rows**: render.rs:123 uses `contact.revision` (the vCard `REV:` field) as `when_ts`, falling back to the `now` parameter when absent. The doc (data_arch.md §273-289) explicitly allows `when_ts` null or sentinel for non-event entities like contacts. The fallback to `now` is a sentinel, not ideal (it claims every contact was modified right now), but the design acknowledges the non-event shape.

- **No fallback to vCard creation logic**: vCards don't carry `CREATED:` fields (RFC 6350 omits them), so there's no upstream-provided alternative to `REV:`. The `now` fallback is reasonable, but a smarter fallback (e.g., file mtime for disk imports) isn't implemented. This is not a violation, just incomplete.
P1: Let's just not make up timestamps, but leave them null.  No fallback.  I think we should probably leave ourselves a note in the data architecture doc about not making up data and not having these fallback paths that mask data incompleteness. 

### Fingerprint & incremental

- **Source fingerprint includes RENDER_VERSION**: render.rs:200-224 computes fingerprint as a hash of `RENDER_VERSION` + uid + addressbook + display_name + revision + emails + phones + addresses + org + title + note + photo byte length. This is correct (bumping RENDER_VERSION invalidates all sidecars, as intended by data_arch.md §173-175), but notably excludes the full photo *bytes* — only the byte length is hashed. This is a memory optimization (photos can be large) but means a byte-identical photo under a different encoding wouldn't trigger re-render. Acceptable trade-off given the scope, but worth noting.

- **Incremental dedup is efficient**: render.rs:106-109 skips re-render if the prior fingerprint matches and the .md file exists. Matches the translate-side dedup principle (data_arch.md §147-150). Clean.

### Commit boundaries

- **No explicit `dolt_commit` in provider (CORRECT)**: extract and translate are silent on commits; the orchestrator handles them (per data_arch.md §294-312). extract/mod.rs, translate/mod.rs, and translate/render.rs all return summaries, never call `commit_run`. Correct.

### Sidecar contract

- **Sidecar structure mostly correct but header field naming**: The sidecar (render.rs:148-155) emits:
  ```json
  {
    "header": {
      "markdown_uuid": "…",
      "source_fingerprint": "…",
      "render_version": 1
    },
    "rows": […]
  }
  ```
  The doc specifies `document_uuid`, not `markdown_uuid`. This is a minor naming inconsistency but the field is present and correct in meaning.
  P0: Yes, like I said, I think we should have a struct that we populate where we can't screw up the fields, and that helps us get this sidecar format correct. We could use the struct both where we create this sidecar and where we consume it. 

- **RenderedMarkdown struct populated correctly**: render.rs:159-168 passes the right fields to `on_doc_complete`: markdown_uuid, source_fingerprint, upstream_cursor (the vCard REV), render_version, and rows. The `upstream_cursor` maps to contact.revision, which is the incremental cursor (each contact's REV field). Correct.

### Other observations

- **Account identity via URL host (not ideal for multi-account)**: extract/mod.rs:327-338 and lib.rs §22-35 note that latchkey keys credentials by service name, which maps to URL host. Two Fastmail accounts on the same host can't coexist without registering two services with different names. This is a latchkey limitation, not a contacts-provider bug, but the workaround (register `fastmail-contacts-1` + `fastmail-contacts-2`) isn't documented as a standard pattern in contacts.
P2: I guess let's note this in the documentation.

- **No edge ingestion (ACCEPTED)**: Contacts are not chat; they don't carry threading or back-references. render.rs never emits edges, and the `on_doc_complete` call passes `Vec::new()` for edges (render.rs:167). Correct.

- **GridRow `kind = "Contact"` is stable**: render.rs:364 always sets `kind: "Contact"`, matching the schema (backend/schema/src/generated/grid_rows.rs, likely). Clean.

- **No dedup across sources at extract time**: If the same contact (same UID) lands in two sources (Apple + Fastmail), they get different contact UUIDs due to the account_id in the UUID derivation (translate/mod.rs:40-44). The UI is responsible for dedup/merge. This matches the principle (data_arch.md §645-649: unification happens in GridRow, not in raw). Correct.
P2: Yes, this is correct. Let's just note it in the documentation.


---

## Email

### Principle violations

- **Missing email pre-seeding (§ Retry and fetch durability, "Pre-seed before fetch")**: 
  - `Email/get` responses are ingested directly into the `emails` table via `upsert_email_in()` (db.rs:790+), but there's no prior `ensure_object_row` call on the listing-phase email IDs.
  - `Email/changes` produces lists of `created` / `updated` ids (mod.rs:435-436), then detail-fetches in batches (mod.rs:441-456), then ingests via `ingest_email_list()`. If a detail fetch dies mid-batch, the IDs are lost and we won't retry them on the next run—they're unknown to the bookkeeping system.
  P0: This sounds important to fix, but unfortunately it is hard to test.
  - **Contrast**: mailboxes follow the pattern correctly (sync_mailboxes → incremental_mailboxes lines 307-364 fetch to_fetch list, then upsert; joined with `upsert_mailbox_in` which calls `dr::record_object_attempt`).
  - **Contrast**: blobs are pre-seeded as stubs when oversize (mod.rs:750-758: `db.pre_seed_blob_stub`) so bookkeeping survives the skip.
  - **Fix**: when `Email/changes` reports `created` list, immediately pre-seed rows with `account_id`, `thread_id`, `blob_id=NULL`, `payload=NULL` before detail-fetching. If the fetch crashes, `payload IS NULL AND attempt_count > 0` rows are visible for retry on the next run.

- **Timestamp normalization not enforced at extract time (§ Time and ordering discipline)**:
  - JMAP `receivedAt` / `sentAt` arrive as ISO-8601 strings per RFC 8621, stored verbatim in the `emails.received_at` / `emails.sent_at` columns (db.rs:208-210, 213-215). No validation that they include explicit offset.
  - At translate time, `when_ts` is set to `em.received_at.clone().unwrap_or_default()` (render.rs:740, 784), which passes through as-is into the `GridRow`. If upstream sends `2024-01-15T10:30:00Z` (bare Z) or naive `2024-01-15T10:30:00` (no offset), it flows downstream uncorrected.
  - **Fix**: validate at extract time that timestamps are RFC 3339 with explicit offset, or normalize `Z` → `+00:00` during `EmailRow::from_payload`.
  P1: Again, I think we should introduce a timestamp utility crate to help us manage this. What I think the philosophy should be is that we never make up data that wasn't given to us, but I guess we will have to assume sometimes that a timestamp was UTC and able to make it sortable. We should have that assumption happen in exactly one place in the whole code base. 

- **No explicit source_url for Email threads**:
  - Notion, Slack, GitHub, and most chat providers populate `source_url` with the upstream web URL (e.g. `https://app.fastmail.com/mail/<mailbox>/<emailId>`). Email threads set `source_url: None` (render.rs:345, 762, 798).
  - The comment acknowledges this (render.rs:336-340): "JMAP doesn't carry a stable web URL per thread" and the Fastmail URL format (`https://app.fastmail.com/mail/<mailbox>/<emailId>.<threadId>?u=…`) needs the account id (`?u=…`) which isn't in the extract.
  - **Fix**: if `session.accounts` carries an account-level URL stem (check the session discovery response), construct it. Otherwise leave as None, but document the gap.

### Dead patterns / cargo-culted code

- **`block_on_load_all()` in a synchronous translate context**:
  - parse.rs:9 wraps `block_on_load_all(db_path)` (which is async internally but blocks on the current thread). This works for the mbox fallback path (translate-only, no JMAP server), but the pattern is unconventional—most providers load raw data inline in the translate call. Fine as-is, but worth noting it's a workaround for translate-only (contacts does the same).

- **`Email/query` pagination restart on `queryState` shift (mod.rs:522-531)**:
  - When full-enumerating via `Email/query`, if the result set changes mid-pagination (queryState shifts), the code restarts from page 0. This is defensive and correct (JMAP spec allows this), but in practice Fastmail / standard JMAP servers don't shift state mid-query on a single account. Worth keeping for robustness, not cargo cult.

- **Destroyed email hard-delete of rows + joins + bookkeeping (db.rs:33-34, delete_emails call)**:
  - When JMAP reports `destroyed` IDs, the code hard-deletes the email row, its `email_mailboxes` / `email_keywords` / `email_attachments` joins, and the bookkeeping (mod.rs:460-461 calls `db.delete_emails`). This is correct per the comment (doltlite history preserves prior state), but it's worth noting: if a user accidentally deletes an email upstream and later restores it, we'll re-ingest it fresh. No permanent tombstone. This is fine for the use case but not the pattern—other providers (Slack) keep tombstone markers for historical reference.

### Simplification opportunities

- **Redundant state-token namespacing**:
  - JMAP state tokens are stored per `(account_id, type_name)` under keys like `jmap:A1:state:Email` (db.rs:154-156). The `account_id` is already the natural isolation unit; the `jmap:` prefix is overfitting. Simplify to `<account_id>:state:<type>` (or drop the `:state:` infix). Minor, but reduces key-space noise.

- **Hardcoded batch sizes**:
  - `EMAIL_GET_BATCH = 50` (mod.rs:45), `EMAIL_QUERY_PAGE = 500` (mod.rs:47), `THREAD_GET_BATCH = 100` (mod.rs:49). These are reasonable defaults but not tunable. If a user's Fastmail instance has slower CPU or the network is flaky, no way to dial down batch size without a code change. Consider moving to config. (Not load-bearing, but DRY.)

- **RENDER_VERSION = 1 (forever)**:
  - translate/mod.rs:13 sets `RENDER_VERSION: u32 = 1` with a comment that bumping forces full rebake. No schema change anticipated, so fine. But if the render logic changes (e.g., attachment materialization path), remember to bump this; document the practice in a crate-level comment.

### Cross-source sharing opportunities

- **Email belongs to a "chat-human" family** (alongside Slack, Beeper, Signal, but distinct from chat-LLM):
  - Email's grid_rows schema: `kind="Email" | "Email Thread"`, `conversation_uuid=thread_uuid`, `message_index=(idx per thread)`, author, text body, attachments via `qmd_path/blobs/…`.
  - This overlaps with Slack (`kind="Message"`, `channel`, `thread_ts`), Beeper, Signal. The schemas are already unified at GridRow level. But the **raw store structure differs**: email has `emails`, `threads`, `email_mailboxes` join; Slack has `messages`, no explicit threads table. Could normalize:
    - Move thread deduplication (which thread does this email belong to?) into a shared `conversation_threads` table pattern.
    - Unify attachment join shape across chat-human providers (all use `<entity>_attachments` + blobId pointers).
  - **Recommendation**: audit Slack, Beeper, Signal raw schemas side-by-side and propose a `chat_message`, `chat_thread`, `chat_participant_join` template that email, Slack, etc. all use. Will reduce translate complexity and make cross-provider queries cheaper. Lower priority (no blocker), but worth a future phase.
  P4: Email feels pretty different.

- **Message-ID as upstream identifier is strong**:
  - Email stores JMAP Email `id` as the primary key (PK in the `emails` table). JMAP ids are opaque per-server (e.g., "G5a4f5c6d"). For mbox / Takeout paths, `Message-Id` header is fallback (mbox.rs:27-29). This is correct per the architecture (upstream identifier as PK). If a future provider (e.g., a second email account with different JMAP server) is added, each gets its own `<name>.doltlite_db` per the framework; message-id collisions between accounts are impossible. Clean.

- **Sidecar JSON contract is well-formed**:
  - `Sidecar` emits `header: { markdown_uuid, source_fingerprint, render_version }` + `rows: [GridRow, …]` + `edges: []` (render.rs:207-215). Matches the spec in data_architecture_ingestion.md § Translate and downstream stages. No deviation, no cargo, good.

- **Attachment handling is CAS-aligned**:
  - Attachments live in `blobs` table (CAS) keyed by blake3 hash. Email rows reference them via `email_attachments.blob_id` (foreign key into `blobs.id`). This matches the blob-split pattern in the port guide § 7. Good.
  - **One caveat**: inline attachments (Content-ID, disposition=inline) are materialized to `blobs/` alongside non-inline ones (render.rs:374-400). The distinction is at render time (is_inline_attachment), not at extract. This is fine—all bytes are content-addressed; the in-vs-out-of-line distinction is metadata. Correct.

### Minor observations (non-violations)

- **Mailbox hard-delete on Mailbox/changes destroyed list**: 
  - Similar to email: when a mailbox is deleted upstream, `db.delete_mailboxes()` hard-deletes it. This assumes mailboxes are ephemeral or rare to restore. For email, mailbox deletes are typically folder deletes (Trash, Custom), which is reasonable to drop from the graph. Acceptable trade-off.

- **No retry-on-by-default knob for email**:
  - The architecture calls for `--retry-failed` (default true) to be configurable per source (§ Retry and fetch durability, "Retry policy is config, not code"). Email doesn't expose this knob. The orchestrator likely provides it globally; email just obeys the per-attempt bookkeeping in `record_object_attempt()` (db.rs:368, 787, 828). Missing the *per-source* config knob mentioned in the spec, but not a hard violation if the global knob exists.

- **No upstreamCreationTime / sentAt enforcement**:
  - JMAP provides both `sentAt` (the client's Date header) and `receivedAt` (server receive timestamp). Email stores both as columns (db.rs:97-98) but uses only `receivedAt` for ordering and `when_ts`. This is correct (receive order is the canonical temporal order). `sentAt` is preserved in `payload` for rendering; good.


---

## GitHub

### Principle violations

1. **Per-item error handling missing for child entities** (extract/mod.rs:200-216)
   - Comments/reviews/review comments fetched via `.unwrap_or_default()` cascade errors to empty collections
   - Extract silently skips malformed child responses; no `record_object_attempt` or error tracking for individual comment failures
   - Violates: "Per-item failures are tolerated... leave durable evidence in the row" (Error handling §432-434)
   - Missing: `deleted_upstream_at` marker for 404s on known comments; retry state not tracked

2. **No per-item retry markers or `deleted_upstream_at`** (extract/db.rs:147-407)
   - All upserts call `dr::record_object_attempt(&mut tx, table, &id, None)` with `None` error parameter
   - 404 or parse errors in `fetch_one_pr` return `Ok(())` silently, log warn, continue (extract/mod.rs:188-196, 201-216)
   - No mechanism to distinguish transient retry-able failure from confirmed deletion
   - Violates: "404 on a known-existed thing... row should carry distinct `deleted_upstream_at` marker" (Transient vs non-transient §502-507)

3. **No pre-seed-before-fetch for child entities** (extract/mod.rs:200-216)
   - Issue comments, PR reviews, PR review comments discovered and fetched in single pass
   - Child rows created at fetch success only, not when their parent PR is discovered
   - If detail fetch crashes, no trace of attempted children left in DB
   - Violates: "Pre-seed before fetch... Row appears at fetch success" (Retry and fetch durability §454-468)
   - Acceptable trade-off: GitHub's upstream API doesn't expose child entity IDs in listing before detail fetch, preventing pre-seed

4. **Intra-run backoff lacks rate-limit header inspection on secondary 429** (extract/client.rs:112-121)
   - Handles primary 403 + `x-ratelimit-remaining: 0` with retry logic
   - Handles secondary 429 generically; `retry-after` and `x-ratelimit-reset` parsed correctly
   - No explicit guidance for secondary-rate-limit budgeting; relies on exponential backoff to 120s max
   - Works but less principled than "retry policy is config, not code" (Retry policy is config §490-495)

5. **Single UUID namespace for PR comments across repos unchecked** (translate/parse.rs:22-53)
   - `github_issue_comment_uuid` uses only `github:{repo}:issue_comment:{id}`
   - GitHub numeric ID spaces are per-repository within issue/PR comment scope, but global within PR reviews
   - UUID namespace `b1a90c3a-1f7f-5d4b-9a23-7e3f2b8d0001` is GitHub-wide, not per-repo
   - Risk: if GitHub ever hands out duplicate numeric IDs across repos (unlikely) or if repo id collides, UUIDs collide
   - Acceptable: GitHub id spaces are actually stable and per-endpoint; documented in db.rs:10-18

### Dead patterns / Cargo-cult code

1. **Legacy event-store JSONL layout in EXTRACT.md** (EXTRACT.md:1-20)
   - Documents old shape: `self_identity/{created,updated}/events.jsonl`, `sync_state.json`
   - Actual code uses doltlite DB only; no JSONL ever written (extract/db.rs, not event_store)
   - Confuses readers about how GitHub raw store actually works
   - Update: rename EXTRACT.md sections to match DB-based architecture

2. **Commented-out rate-limit constants** (extract/client.rs:23-25)
   - `RETRY_MAX = 7`, `RETRY_INITIAL_BACKOFF_MS = 2_000`, `RETRY_MAX_BACKOFF_MS = 120_000`
   - No configuration hook to tune per-config; hardcoded as "works for GitHub"
   - Not cargo-cult but missed opportunity for `--retry-config` (compare slack provider)

3. **FetchSummary counts all on success path** (extract/mod.rs:197-198, 202-216, summary:88-95)
   - Child fetch errors return `Ok(())` but don't bump error counter
   - `FetchSummary.new_issue_comments` etc. count successes, not attempts
   - Operator can't distinguish "fetched 100 comments, 2 failed" from "fetched 100 comments"
   - Dead code: `errors` field unused

### Simplification

1. **Composite PK for PR could be cleaner** (extract/db.rs:116-119)
   - `pr_pk(repo, num)` returns `format!("{repo}#{num}")` string
   - Could use typed `(String, u32)` with custom `Display` impl
   - Actual problem: string concat is fragile; no validation on `#` uniqueness in `repo` field
   - Low priority: GitHub repo names already forbid `#`

2. **`record_object_attempt` always called with `None` error** (extract/db.rs:172, 237, 282, 328, 404)
   - Every successful upsert passes `None` for error parameter
   - Pattern suggests original intent was to pass error on failure, but extracts don't do that
   - Simplify: remove error parameter, rely on bookkeeping sidecars for durability

3. **Duplicate child-entity load methods** (extract/db.rs:411-436, 438-466)
   - `load_pull_requests()` and `load_children(table)` both iterate, deserialize, filter `payload IS NOT NULL`
   - Could unify: single `load_entities(table: &str)` returning generic `Vec<LoadedEntity>`
   - Low priority: only called once per provider-run

4. **scope_state delegates don't add value** (extract/db.rs:470-476)
   - `load_scope_state` and `upsert_scope_state` are thin wrappers on `dr::` functions
   - Just call `dr::` directly from `fetch()` (extract/mod.rs:258-270)

### Cross-source sharing (GitLab code-review-thread family)

1. **Code-review-thread UUID generation conflict between GitHub and GitLab** (translate/parse.rs:26-53)
   - GitHub PR: `github:{repo}:pr:{number}` → UUID (GridRow for PR itself)
   - GitHub review comments (inline): `github:{repo}:pr_review_comment:{id}` → UUID (code review child)
   - GitLab MR: uses similar pattern `gitlab:{repo}:mr:{number}`
   - GitLab note (inline comment): uses `gitlab:{repo}:note:{id}`
   - Issue: no shared UUID namespace; can't deduplicate or cross-reference across sources
   - Architecture says "git_sha and external_id are explicitly there for this" (Object identity §234-236)
   - Actual state: GridRow carries both fields (grid_rows.rs:190, 218-219), but no cross-source join uses them
   - **No actual violation** — multiple namespaces intentional; dedup is content-hash-based in UI, not UUID-based

2. **GridRow fields for code-review family properly populated** (translate/grid_rows.rs:188-191, 216-219)
   - PR GridRow: `git_sha: pr.head_sha`, `external_id: pr.pr_number` ✓
   - Comment GridRow: `git_sha: c.commit_id`, `external_id: c.external_id` ✓
   - Both populated for every code-review row
   - Enables GitLab and other code-review sources to share family logic
   - Well-designed; follows architecture intent

3. **Sidecar JSON structure ready for cross-source family sharing** (translate/grid_rows.rs:40-54)
   - Each comment carries `external_id`, `path`, `line`, `commit_id` as separate JSON fields
   - GridRow schema unifies at projection layer (grid_rows.rs:197-226)
   - No schema unification in raw store (GitHub and GitLab have separate entity tables) ✓
   - Follows principle: "Unification should never happen in the raw store" (Shared schemas §643-644)

### Timestamps and event-shape

1. **When_ts for PRs uses updated_at, fallback to created_at** (translate/grid_rows.rs:167-171)
   - PR: `updated_at` or `created_at`, no synthesis
   - Comments: always `created_at` from upstream
   - Review comments: `created_at` from upstream
   - All timestamps are ISO-8601 strings from GitHub payload
   - No explicit offset check; GitHub always returns `+00:00` implicitly
   - Good enough but missing validation: should verify all are truly ISO-8601 with offset

2. **No timestamp synthesis for sub-items** (translate/parse.rs:206-320)
   - Reviews inherit PR timestamp implicitly (no synthesis attempted)
   - Inline comments don't get microsecond-bump if multiple exist on same line in same second
   - Violates: "Microsecond-bump for synthesized timestamps" (Time and ordering §260-266)
   - Impact: ordering within a single (path, line, second) group not stable across re-runs if new comments added
   - Acceptable trade-off: GitHub `created_at` has second precision; >1 comment/second on same line is rare

### Cursor and refresh-window strategy

1. **Refresh window + scope state well-designed** (extract/mod.rs:104-115, 149-175)
   - State stored in `sync_scope_state` table (delegated to dr:: helpers)
   - `since_for_scope` picks `max(state[scope], now - refresh_window_days)`
   - Safely re-queries trailing 30 days by default
   - No checkpoint files; dedup index is cursor ✓
   - Matches forward-walk pattern described in architecture (Cursor / resume strategy §315-328)

2. **Scope discovery persists before PR fetch** (extract/mod.rs:267-271)
   - `new_scope_state` committed after discovery, before PR detail fetch
   - Crash mid-PR-batch leaves partial PR list but complete scope state
   - Next run will skip already-discovered PRs (dedup on upsert)
   - Well-done resilience pattern

### Fingerprinting and sidecar contract

1. **Source fingerprint includes RENDER_VERSION and all comment data** (translate/grid_rows.rs:72-87)
   - Hash includes RENDER_VERSION (circuit breaker for schema changes)
   - PR payload canonicalized then hashed
   - Comments sorted by external_id then hashed
   - Deterministic: same upstream payload → same fingerprint
   - Sidecar emitted with fingerprint; translate-side dedup skips unchanged sidecars ✓
   - Follows: "source_fingerprint short-circuits re-render" (Translate and downstream §390-392)

2. **Sidecar headers carry document_uuid and render_version** (translate/render.rs:1-142, implied by Sidecar struct)
   - GridRow rows carry `markdown_uuid: Some(pr.uuid)` (grid_rows.rs:194, 222)
   - Follows contract: `Sidecar` struct with header + rows (Architecture §365-381)
   - Load layer honors fingerprint in `markdowns_loaded` bookkeeping (implied by architecture, not visible in GitHub code)

### Raw schema and wire-fidelity

1. **Payload preservation is strict** (extract/db.rs:155, 204, 257, 301, 360)
   - `payload_str = serde_json::to_string(payload)` then `jsonb(?)` on write
   - Read side: `json(payload)` and `serde_json::from_str(&s)` round-trip
   - No synthetic fields (`_fetched_at`, `_listing_update_time`) injected into payload ✓
   - Timestamps and URLs lifted to real columns (updated_at, html_url, head_sha, etc.) ✓
   - Violates wire-fidelity principle: none detected
   - Follows: "Don't pollute payloads with downloader-synthesized keys" (Wire-fidelity §165-169)

2. **JSONB storage working correctly** (extract/db.rs:588-602 test)
   - Test verifies `typeof(payload) = 'blob'` (JSONB encoding)
   - Efficient storage; enables dolt-side queries on payload fields
   - Good

### Monitorability

1. **Progress bar integration** (extract/mod.rs:281-291)
   - Sets progress length, increments per PR, displays `{repo}#{num}`
   - Matches `obs::ObsArgs` contract (Architecture §94-102)
   - Well-done; operator sees per-PR progress

2. **Request counting** (extract/client.rs:54-56, mod.rs:296)
   - `GitHubClient` maintains atomic request count
   - Reported in `FetchSummary`, included in final summary
   - Helps operator understand API quota consumption

3. **Tracing logs on error** (extract/mod.rs:185-190, 201)
   - Per-PR failure: `tracing::error!(repo, num, error = %e, ...)`
   - Per-scope discovery: `tracing::info!(scope, ?since, ...)`
   - Rate-limit retry: `tracing::warn!(url, status, attempt, sleep_ms, ...)`
   - Good coverage for debugging

### Commit boundaries

1. **Single orchestrator commit per run guaranteed** (extract/mod.rs:246-297)
   - `ExtractRun::start()` and `run.finish()` wrap the entire fetch
   - No `dolt_commit` called inside provider code ✓
   - Follows: "Providers do not call dolt_commit... orchestrator wraps each source" (Commit lifecycle §294-300)

### Testing

1. **Fixture-based playback tests** (synthesize.rs, tests/)
   - TNG fixture data in `tests/fixtures/` (implied by architecture)
   - `GithubSynth` synthesizer converts event-store layout to playback responses
   - Playback allows extract to run without live API
   - Pattern matches architecture (Testing with TNG fixtures §532-547)

2. **No live-test artifacts checked in** (tests/github_live.rs, tests/playback_roundtrip.rs implied)
   - Live tests tagged `manual` (inferred, not visible in reviewed code)
   - No real user data in checked-in fixtures ✓

### Deviations from documented behavior

1. **EXTRACT.md describes outdated event-store layout**
   - Section "Output": lists `self_identity/{created,updated}/events.jsonl` etc.
   - Actual code uses doltlite DB with single sqlite file
   - Should be updated to: "Output: `<data_root>/raw/<name>.doltlite_db` with tables: self_identity, pull_requests, issue_comments, pr_reviews, pr_review_comments, {table}_bookkeeping for each"

2. **sync_scope_state table not documented in db.rs comments**
   - db.rs:10-18 lists data tables but omits sync_scope_state
   - Hidden inside `dr::` delegation
   - Document: "sync_scope_state is created by `dr::` helpers and carries per-scope last-seen-at"

### Recommendations

1. **Add `deleted_upstream_at` marker and 404 handling**
   - In `fetch_one_pr`, catch 404 on PR detail and set `deleted_upstream_at = now()`
   - For child entities, wrap paginate() results to check status; 404 → skip, log, don't retry
   - Requires schema change: add `deleted_upstream_at TEXT NULL` to entity tables and bookkeeping

2. **Decompose error handling into failure recording**
   - When child fetch fails (parse error, network), call `dr::record_object_attempt(..., Some(error_msg))`
   - Operator can then run `--retry-failed` to re-attempt known failures
   - Allows future retry policy configuration

3. **Add timestamp offset validation**
   - Assert in translate/parse that every `created_at` / `updated_at` contains `+` or `Z` offset
   - Catches upstream schema drift before it silently breaks ordering

4. **Update EXTRACT.md and add DOLTLITE_RAW.md**
   - Rename current EXTRACT.md → "Database Schema" or "Raw Store"
   - Add migration note: "Port from event-store JSONL to doltlite DB"
   - Cross-link to architecture doc Commit lifecycle and Wire-fidelity sections

5. **Consider microsecond-bump synthesis for comment ordering**
   - If same-second multiple comments on same inline thread become real issue, add synthesis
   - For now: acceptable to rely on external_id secondary sort

---

## GitLab

### Principle violations

**UUID identity (UUIDv5)**
- gitlab_mr_uuid using hardcoded GITLAB_UUID_NS namespace UUID is correct pattern (parse.rs:21-31)
- But gitlab_note_uuid uses only `gitlab:{proj}:note:{id}` as input; missing `iid` to disambiguate notes across discussions
  - Risk: different notes with same upstream ID across multiple MRs map to same UUID
  - Expected: `gitlab:{proj}:mr:{iid}:note:{id}` or discussion scope
  - parse.rs:32-38

**Event-shaped rows: when_ts discipline**
- MR row uses `updated_at` with fallback to `created_at` (grid_rows.rs:127-131); correct ISO-8601 format preserved from upstream
- Note rows use `created_at` directly without validation (grid_rows.rs:163)
- **Missing**: no explicit validation that timestamps are ISO-8601 with explicit offset; passthrough from payload assumes upstream shape
  - Real risk if GitLab ever changes format or returns bare `Z` vs explicit `+00:00`
  - Expected: explicit ISO-8601 + offset validation or microsecond-bump for synthesized (none needed here but discipline absent)

**Cursor / resume strategy (refresh_window)**
- Uses `scope_state` table with `refresh_window_days=30` (extract/mod.rs:67, default 30)
- Correctly re-fetches MRs in trailing window via `updated_after` filter (extract/mod.rs:124-125)
- **But**: discussions are re-fetched for every MR unconditionally, not windowed (extract/mod.rs:234-236)
  - On second sync, if an MR is unchanged (skipped_unchanged_mrs path), discussions are still fetched/upserted
  - Pure inefficiency, not a correctness bug, but violates "efficiently incremental" principle
  - extract/mod.rs:316-340

**404 → deleted_upstream_at marker**
- Completely absent; no 404 handling at all in GitLab client
- client.rs:87-91 treats 502-504 as transient, falls through to permanent error
- But 404 would also trigger "permanent" error path without any special marker
  - Expected: `deleted_upstream_at` column in merge_requests + discussions tables
  - extract/mod.rs does not distinguish 404 from other errors; all logged and skipped
  - Violates "confirm-deletion without retrying forever" principle

**Pre-seed before fetch (NULL payload)**
- Discussions are NOT pre-seeded before fetch
  - Discovery loads MR list with minimal fields, calls detail fetch for full MR
  - Then immediately calls discussions fetch without pre-seeding rows
  - If discussions fetch crashes, no trace left (no partial discussion rows)
  - Expected: pre-seed discussion rows with `payload IS NULL`, then fetch + upsert
  - extract/mod.rs:210-239

---

### Dead patterns

**Synthesizer module (synthesize.rs)**
- Contains old event-store playback logic for fixtures
- Comments refer to "event-store layout" and `{created,updated}/events.jsonl` structure (synthesize.rs:3-16)
- Port of old Python code; actual extract now uses doltlite, not event-store
- Likely still used by tests but dead code path in production
- File: synthesize.rs entire module

**Double-ish sidecar emission**
- render.rs emits both `.md` and `.grid_rows.json` in `render_one_mr` (render.rs:262-274)
- Then caller `render_gitlab` also calls `on_doc_complete` with identical sidecar data (render.rs:311-321)
- Both build `Sidecar { header, rows }` separately
- Pattern works but duplication suggests vestigial half-refactor from older architecture
- render.rs:262-274 and render.rs:310-320

---

### Simplification

**Scope state cursor**
- Both GitLab and GitHub share `since_for_scope` helper in `frankweiler_etl::scope_state` (extract/mod.rs:94)
- Extract/mod.rs just re-exports it unchanged; good reuse
- **Simplify**: document this as the shared pattern for both; remove duplication comments

**Fingerprint + RENDER_VERSION**
- Both gitlab and github use identical fingerprint strategy: canonicalize JSON, hash RENDER_VERSION + payload + sorted items
- grid_rows.rs:62-76 mirrors github/src/translate/grid_rows.rs:72-87 exactly
- **Simplify**: extract to shared `frankweiler_etl::fingerprint` module or macro; both providers are identical
- Pattern is load-bearing (sidecar `source_fingerprint` field) but code duplication is unnecessary

**No blobs for GitLab**
- Unlike Slack, Anthropic, GitHub, GitLab provider has no blob storage layer
  - No files/attachments in MRs (discussion notes are text-only or Markdown)
  - No `blob_refs` or `blob_refs_bookkeeping` tables
- This is correct (no resource to store), but code doesn't even mention it
  - **Could clarify**: add comment explaining why blob storage skipped (MRs don't carry file attachments the way Slack/GH PRs do)
  - extract/mod.rs header or db.rs header

---

### Cross-source sharing with GitHub

**Code-review-thread family alignment**
- Both GitLab MRs and GitHub PRs are code-review threads; architecture doc explicitly names them (data_architecture_ingestion.md:661-663)
- Both have `git_sha` + `external_id` fields in GridRow
- **Alignment**: both correctly populate these:
  - GitLab: `git_sha: mr.head_sha` and `external_id: mr.mr_iid` (grid_rows.rs:150-151)
  - GitHub: `git_sha: pr.head_sha` and `external_id: pr.pr_number` (github/translate/grid_rows.rs:190-191)
- ✓ Good

**Note/comment structure divergence**
- GitHub discussions are flattened from `pull_request_review` + `pull_request_review_comment` + `issue_comment` tables
  - Multiple entity types, different schema per comment kind, linked via `in_reply_to_id`
  - github/db.rs: 4 entity tables (pull_requests, issue_comments, pr_reviews, pr_review_comments)
- GitLab discussions are already threaded natively in upstream API as `discussions` containing `notes`
  - Single entity type (discussion), all notes inside payload
  - gitlab/db.rs: 1 discussion table, notes unrolled during parse
- **Result**: both arrive at GridRow correctly, but raw schema is maximally divergent
  - No opportunity to unify raw store (by design — wire-fidelity)
  - But translate-stage unification should be stronger
  - **Could share**: common `CommentRow` / `NoteRow` struct + fingerprinting logic in shared module

**Entity table shape**
- GitHub: `pull_requests`, `issue_comments`, `pr_reviews`, `pr_review_comments` — 4 tables, many PK choices
- GitLab: `merge_requests`, `discussions` — 2 tables, composite string PKs (`proj!iid`, `proj!iid#discussion_id`)
- Both use upstream IDs as PKs (no surrogate autoincrement) — ✓ correct
- **Divergence in simplicity**: GitLab is cleaner (fewer entity types) but both are correct
  - No unification opportunity; schema shape is upstream-driven

**Bookkeeping usage**
- Both use `dr::bookkeeping_ddl_for(table)` to auto-generate `<table>_bookkeeping` sidecars
- Both call `dr::record_object_attempt(tx, table, id, None)` on every upsert
- ✓ Identical pattern across providers; consistent

**MR/PR discovery and filtering**
- GitHub: `search-issues is:pr <scope>` with `updated:>=since` filter (extract/mod.rs:125-135)
- GitLab: `/merge_requests?scope=<scope>&state=all&updated_after=<since>` (extract/mod.rs:120-128)
- Both correctly use scope state + refresh window
- GitHub truncates `since` to `YYYY-MM-DD` for search syntax (extract/mod.rs:104-115)
- **Opportunity**: both use identical `since_for_scope` helper; GitHub's date truncation is site-specific, not shared
  - Could clarify in schema docs that `scope_state` is UTC ISO-8601 seconds precision, each provider truncates for their API

**Skipped-unchanged optimization (MRs only)**
- GitHub: does NOT skip unchanged PRs — every PR detail is re-fetched unconditionally
- GitLab: skips MRs whose `updated_at` matches the discovery listing (extract/mod.rs:330-340, line 333 increments `skipped_unchanged_mrs`)
- **Divergence**: GitLab is more efficient on warm syncs
  - GitHub could adopt the same pattern
  - Not a violation but a missed optimization in GitHub

---

### GitLab Summary

**Severity of issues**:
1. **High**: UUID collision risk in note UUIDs (parse.rs:32-38); missing 404 handling + deleted_upstream_at (client.rs, db.rs)
2. **Medium**: Discussions not pre-seeded (extract/mod.rs); double sidecar emission (render.rs); discussions always re-fetched (extract/mod.rs)
3. **Low**: Missing timestamp validation discipline; code duplication in fingerprint + scope_state; synthesizer dead code
4. **Alignment**: Overall good on code-review-thread family but room for shared modules (fingerprint, comment row type)


---

## Notion

### Principle violations

- **Pre-seed incomplete** — Pages implement `dr::ensure_object_row` pattern via `upsert_pages()` with `payload=None` (extract/db.rs:225), satisfying the main design doc example. **But blocks and comments lack pre-seeding**: they go straight to detail upsert in `upsert_blocks()` and `upsert_comments()` (extract/db.rs:238,278) without a listing→detail split phase. The doc calls pre-seed "pre-seed always" as the aspiration; blocks/comments violate that. **Mitigation present**: `fetch_all_children()` and `fetch_all_comments()` are paginated cursors that land full bodies, so discovery and detail are fused—but if a detail fetch fails mid-batch, rows with `payload IS NULL` don't exist for retry. extract/mod.rs:484-489 silently skips block errors instead of persisting them.

- **No 404→deleted_upstream_at marking** — Error paths record generic `last_error` via `record_page_error()` (extract/db.rs:308,314). No distinction for confirmed deletions (HTTP 404 on a known-existed page). The `record_blob_error()` path (412) is fine for blobs; pages/blocks/comments have no equivalent `deleted_upstream_at` column. extract/mod.rs:422 logs 404-like errors as warn/continue but doesn't mark them distinctly.

- **Timestamp lacks explicit offset guarantee** — `when_ts` in GridRow uses `last_edited_time` or `created_time` strings from Notion's API verbatim (translate/grid_rows.rs:192-197, 265-269, 301-305). Notion returns ISO-8601 with `Z` suffix (UTC). **The code assumes upstream is always UTC and never checks**: no explicit `+00:00` rewriting step, and no `Z`→`+00:00` normalization. If Notion ever includes offset variants, this breaks the "strict ISO-8601 with offset" rule. Comments sort by `created_time` directly without validating offset presence (grid_rows.rs:623-627). No microsecond-bump for sub-items lacking own timestamp (comments within a discussion are all `created_time`-based, no synthetic `µs+parent` pattern).

- **Block UUID backpointers missing in sidecar** — `notion_block_uuid` column in GridRow is populated for comment threads (grid_rows.rs:287,323) but only when `parent_block_id` is present. **Block-level document renders (if they exist) have no backpointer column to the owning block**. The doc lists "provider-specific cross-references" as first-class; Notion should surface block←→document links symmetrically.

### Dead patterns

- **Unused DATA_TABLES entries** — extract/db.rs:39 declares `["pages", "blocks", "databases", "users", "comments"]` but no code ever writes to `databases` or `users` tables. The DDL is created (db.rs:41-76), full_ddl() adds bookkeeping for both (90), but extract/mod.rs never calls `upsert_databases()` or `upsert_users()`. These are cargo-culted from an earlier design. No tests exercise them. **Low risk** (inert tables), but clutter + dead DDL.

- **`ensure_id()` helper unused** — extract/db.rs:178-183 exposes a public `ensure_id()` method that wraps `dr::ensure_object_row`, but no call site in the provider uses it. Pages pre-seed inline in `upsert_pages()` instead (225-229). The helper was left behind.

- **`page_states()` cache is incomplete** — extract/db.rs:155 snapshots `(id, last_edited_time, has_payload)` once at sync start (extract/mod.rs:652). Later, `mirror_page()` updates `page_states` in memory (470), but a second walk of the same page (e.g., inbox discovers a page already in subtree) re-checks the in-memory version, not the DB. If a page's `last_edited_time` advances between the two discoveries, the in-memory cache serves stale data. **Not a bug in practice** (same sync run, unlikely collision), but design smell: re-fetching the cache mid-sync is fragile.

### Simplification opportunities

- **O(N²) block→page walk on large trees** — Fixed in grid_rows.rs with memoization (grid_rows.rs:476-519 comment + code), but only in translate. Extract's `walk_page_blocks()` does BFS; no quadratic cliff because extract discovers via API pagination, not local tree walk. **Already solved at translate layer** (grid_rows.rs comments, lines 726-747). No action needed.

- **Inline image blob fetch** — extract/mod.rs:129-180 fetches image bytes immediately after page blocks, bypassing the normal retry-on-failed-blob pattern. Per-block GET is small; justified by "let a single sync run produce a self-contained DB" comment (521-523). **But**: signed URLs rotate; if a page fetch succeeds but image fetch fails, the image row sits in `blob_refs` with `blake3=NULL` and `last_error=…`, and `--retry-failed` will re-attempt it. This is fine. However, if the page is fetched again later (e.g., upstream edit), `blob_exists()` check (141) will skip the image re-fetch, leaving a stale `blake3=NULL` row. Not critical (CAS merges on hash), but housekeeping gap.

- **Comment parent fallback** — extract/mod.rs:541 falls back to page ID when comment parent is missing. Per-comment `last_error` tracking (346) doesn't record which parent ID was assumed. Later, a retry that fixes the parent chain won't know to re-parent the comment. **Low risk** in practice (Notion API should always include parent), but brittle.

### Cross-source sharing

- **GridRow `when_ts` consistency** — Notion's pages/comments use raw upstream `last_edited_time` / `created_time` ISO-8601 strings. **Slack message timestamps (`ts`) are Unix seconds** (a different provider; not directly relevant here). **GitHub uses `created_at` / `updated_at` ISO-8601**. For cross-provider temporal views to work, all three need the same offset normalization. Notion's code assumes `Z`; **a grep over translate/grid_rows.rs shows no explicit offset validation or rewriting**. Other providers (slack, github, etc.) may have the same gap. This is a project-wide issue, not Notion-specific, but Notion is a data point.

- **Fingerprint schema** — translate/grid_rows.rs:356,369 hash RENDER_VERSION + canonical JSON of upstream payloads. ThreadDocument fingerprints over comments only (369), PageDocument over page+blocks+comments (356). This mirrors translate/render.rs:1198-1307 fingerprint skip checks. **Matches the sidecar contract** (docs/dev/data_architecture_ingestion.md §Translate and downstream stages). **No deviation**, cross-source patterns are consistent.

- **Sidecar emission** — render.rs:1223-1234,1330-1341 emit `*.grid_rows.json` with `Sidecar { header: SidecarHeader { markdown_uuid, source_fingerprint, render_version }, rows, edges }`. **Matches the canonical contract** (data_architecture_ingestion.md, Sidecar struct). No provider-specific quirks.

---

**Summary**: Notion's pre-seed pattern is incomplete (blocks/comments lack it), timestamps lack explicit offset normalization (upstream-assumed `Z`), and dead tables clutter the schema. No commit-boundary violations detected. Bookkeeping sidecars are present. Retry-on-failed is implemented. The provider is generally sound but has edge-case fragility around comment parent resolution and stale blob refs after re-fetch.

---

## Perseus

### Principle violations

**None identified.** Perseus is the exemplar of respecting the architecture. It deliberately rejects machinery that doesn't apply to an immutable corpus.

---

### Dead patterns / Cargo-culted code

#### 1. Unused `FetchSummary` fields in extract phase
- **Location**: `extract.rs:66-72`
- **Issue**: `FetchSummary` carries `skipped: usize`, but Perseus never increments it. The only codepath is success or error; there's no "was already downloaded so skip" semantics because the upstream is immutable. Field should be removed or documented as vestigial.
- **Severity**: Cosmetic. The summary is correctly formatted and reported in `main.rs:1678`.

#### 2. `--reset-and-redownload` path in extract does work that shouldn't exist
- **Location**: `extract.rs:85-87`
- **Issue**: `if opts.control.reset_and_redownload { clear_xml_files(...) }` clears `.xml` files from `out_dir` when the user passes `--reset-and-redownload`. This flag is designed for "invalidate cached entity state, keep CAS blobs" on incremental sources. For Perseus, the upstream files themselves are immutable, so `--reset-and-redownload` should be a no-op — or better, a soft error with guidance: "Perseus is immutable; this flag has no effect. To re-render, use `RENDER_VERSION` bump or `--refetch`."
- **Severity**: Medium. Silently deletes local files on a flag meant for invalidation semantics. The test at `extract.rs:182-194` validates that only `.xml` is touched, which is correct, but the operation is confusing when the corpus never changes.

#### 3. Three-way decode path in `curl_to_file`
- **Location**: `extract.rs:119-137`
- **Issue**: The function is correct, but the comment about `--refetch-blobs` / `--reset-and-redownload` elsewhere in the architecture is irrelevant to Perseus. There's no "re-fetch" semantics — either the files are present and translate-ready, or they're not. The error handling is sound (non-zero exit on 4xx/5xx), but the function doesn't need to be part of a broader retry / durability harness.
- **Severity**: Cosmetic. Function is correctly implemented; the confusion is conceptual.

---

### Simplification opportunities

#### 1. Decouple Perseus from `ExtractControl`
- **Current state**: `fetch(...)` accepts `FetchOptions { control: ExtractControl }` (line 63) to check `control.reset_and_redownload` (line 85).
- **Opportunity**: For an immutable source, eliminate the `control` parameter or replace it with a narrow boolean `--clear_old_files` (or just remove the feature entirely). The orchestrator can still pass other control fields to other providers without Perseus needing them.
- **Impact**: Reduces conceptual surface area. Makes it explicit that Perseus has different invalidation semantics.
- **File**: `extract.rs:54-64, 85-87`

#### 2. Inline the extract phase into translate
- **Current state**: Extract is a separate async phase that returns file metadata. Translate is a separate phase that reads the files. Both live under `sync/src/main.rs`'s per-source dispatch.
- **Opportunity**: Because Perseus has no API fetch, no incremental cursor, and no doltlite output, the extract phase is really just "ensure XMLs are present" — a precondition for translate, not a standalone output. Moving the file-fetch logic into translate's entry point (or a small `ensure_files()` precondition) would reduce the cognitive load. The bazel test target `perseus_translate_test` already treats them as coupled (it stages XMLs and calls translate directly, skipping extract).
- **Impact**: Reduces boilerplate in `main.rs`, makes the "stateless file-to-markdown pipeline" shape more obvious.
- **File**: `sync/src/main.rs:1667-1681, 1393-1397, 1304`; `translate/mod.rs`

#### 3. Remove the bookkeeping / pre-seed / UPSERT machinery from conceptual load-bearing
- **Current state**: The architecture doc emphasizes `attempt_count`, `last_error`, `last_attempt_at` bookkeeping tables, NULL-payload pre-seeding, and `ON CONFLICT(id) DO UPDATE` UPSERT semantics. These are *load-bearing* for incremental API fetches.
- **Perseus status**: No entity table. No doltlite output at all. The Translate phase emits `.grid_rows.json` sidecars directly. All the schema-evolution / incremental-resume machinery that constraints the rest of the system is absent.
- **Opportunity**: Explicitly document this in [`lib.rs:12-25`](lib.rs) — the architecture doc already calls out Perseus as special (line 619–625), but it's worth a note here that the contract is "stateless XML → markdown; no intermediate raw store, no dedup, no retry bookkeeping." This is good! It means Perseus can be simplified aggressively and doesn't constrain future raw-store design.
- **File**: `lib.rs` (docs already strong)

#### 4. Fingerprint strategy is correct but verbose
- **Current state**: `compute_book_fingerprint` / `compute_chapter_fingerprint` in `render.rs:716-744` hash content + `RENDER_VERSION` to decide if a doc needs re-rendering.
- **Status**: This is the right pattern and aligns with the architecture ("incremental: the sidecar `source_fingerprint` short-circuits re-render" — doc line 390). No simplification needed, but worth noting that Perseus doesn't need the "upstream content changed" dimension of the fingerprint — only "did the renderer (RENDER_VERSION) change?" The fingerprint still includes the chapter/section text content for defensive reasons (catches data corruption if the XML is re-parsed differently), so the current approach is sound.
- **Severity**: None. Good as-is.

#### 5. Synthetic timestamp logic is serviceable but could be clearer
- **Current state**: `synth_when_ts` (render.rs:272-277) generates timestamps by computing `(book_n * 10_000 + chapter_n)` seconds from `ts_base()`, to keep reading order stable in the grid's `when_ts` sort.
- **Architecture fit**: The architecture doc says "Entities without a time-shape... accept that `when_ts` is either null or a sentinel" (doc line 285). Perseus uses a synthetic ordering sentinel, which is fine.
- **Opportunity**: The choice of sentinel (2026-01-01 midnight + order-preserving offset) is arbitrary. A clearer approach might be to use null `when_ts` and rely on `external_id` ordering (book.chapter.section) for UI fallback, or to document the sentinel value in the schema so future readers know "2026-01-01 = Perseus synthetic baseline." The current approach is not wrong, but the magic number `10_000` (max ~100 chapters per book?) could have a comment.
- **File**: `render.rs:42-44, 272-277, 452, 493, 540`
- **Severity**: Low. Works correctly; just add a comment explaining the 10_000 stride.

---

### Cross-source sharing

#### 1. UUID derivation is perfectly portable
- **Location**: `lib.rs:82-181`
- **Status**: Perseus uses deterministic UUIDv5 from a frozen namespace + component identifiers (book, chapter, section, language, sentence index). The tests in `lib.rs:183-206` lock UUIDs against the original Python script's output for cross-database consistency.
- **Sharing opportunity**: If a future source needs stable hierarchical UUIDs (e.g., a TEI-based corpus, a hierarchical archival collection), the `perseus_uuid_ns()` / `book_uuid()` / `chapter_uuid()` pattern is directly reusable. Consider moving the namespace + base pattern to a shared `uuid_helpers` module.
- **Impact**: Minimal; only matters if a second source adopts this pattern.
- **File**: `lib.rs:94-132`

#### 2. Bilingual sentence-alignment edge schema is reusable
- **Location**: `render.rs:637-714` (edges emit `"bilingual-alignment"` label with per-sentence anchors)
- **Status**: The schema carries `(src_markdown_uuid, src_anchor_uuid, dst_markdown_uuid, dst_anchor_uuid, label)` and the renderer constructs edges deterministically so re-ingest is idempotent.
- **Sharing opportunity**: If a future corpus has parallel editions (Old English + Modern English, Hebrew + Aramaic + Greek parallels, etc.), the `chapter_edges()` + per-sentence wrapping pattern is a template. The `paragraph_sentence_uuid()` derivation (lib.rs:146-157) could be generalized to `section_span_uuid(book, chapter, section, lang, span_type, span_index)` for multi-language alignment across any hierarchical text.
- **Impact**: Medium. Good foundational pattern for multilingual corpora.
- **File**: `render.rs:637-714`, `lib.rs:146-157`

#### 3. Sidecar contract is exactly per spec
- **Location**: `render.rs:584-609`
- **Status**: Writes `{ header: { markdown_uuid, source_fingerprint, render_version }, rows: [...], edges?: [...] }`. Matches the architecture doc's `Sidecar` struct (doc line 370–381).
- **Sharing**: All providers should follow this. Perseus does. No lift needed.
- **File**: `render.rs:584-609`

#### 4. No multi-work extensibility yet
- **Location**: `lib.rs:5-10`, `extract.rs:20-26`, `config.rs:150-160`
- **Status**: Hard-coded to Thucydides (TLG0003.TLG001). Adding a second work (e.g., Herodotus) requires moving work-specific constants (`TLG0003_TLG001`, `WORK_TITLE`, `WORK_SHORT`) from `lib.rs` into a per-work config struct on `PerseusSync`.
- **Sharing opportunity**: If a second immutable corpus provider lands (e.g., a vCard addressbook, a static Wikipedia corpus), the multi-work configuration pattern is a template.
- **Impact**: Low urgency; the first-work shape is clean.
- **File**: `lib.rs:5-10, 82-85`, `config.rs:150-160`, `sync/src/main.rs:1393-1397`

#### 5. Grid row schema unification is mature
- **Location**: `render.rs:446-476` (book row), `478-516` (chapter row), `519-582` (section row)
- **Status**: All rows carry the full `GridRow` schema — `uuid`, `provider`, `kind`, `when_ts`, `conversation_name`, `markdown_uuid`, `message_index`, `external_id`, `source_url`, `qmd_path`, `edges` links, etc. The grid backend queries rows with a single schema and doesn't branch on provider.
- **Sharing**: All new providers should do the same. Perseus does. No lift needed.
- **File**: `render.rs:446-582`

---

### Assessment

**Overall**: Perseus is a masterclass in respecting the architecture while doing something fundamentally different (immutable corpus vs. incremental API). It:

- ✓ Uses `GridRow` schema coupling and the bazel test rig (reason to stay in a provider crate)
- ✓ Follows the sidecar contract exactly
- ✓ Implements deterministic UUIDs that survive re-ingest
- ✓ Emits `edges` rows for bilingual navigation
- ✓ Fingerprints correctly to short-circuit unchanged renders
- ✓ Has zero bookkeeping tables, zero UPSERT complexity, zero cursor logic
- ✓ Rejects machinery that doesn't apply (`--reset-and-redownload` as a no-op concept)

**Violations**: None.

**Cargo-culted code**: The `--reset-and-redownload` handling (line 85–87 of extract.rs) is the only inherited pattern that doesn't belong. Low-risk but conceptually confusing.

**Simplifications**: Extract and translate could be fused in one pass (line 2 above); the multi-work config is not yet built (line 4 above). Neither blocks the current work.

**Dead weight**: None significant.

---

### Minor issues

1. **`extract.rs` line 72**: `skipped` field in `FetchSummary` is always zero. Could remove or mark as deprecated.
2. **`render.rs` line 42-44**: Magic number `2026-01-01` for `ts_base()` should have a comment explaining why that year. Likely chosen to sort after all real event-shaped data; document this.
3. **`render.rs` line 274**: Stride of `10_000` assumes <100 chapters per book. Add comment: `// Assume ≤100 chapters per book; stride is 10_000 seconds ≈ 2.7 hours per book`.


---

## Signal

**Summary**: Signal is the simplest template (backup-file one-shot ingestion), honored mostly well. Key violations: sidecar uses `markdown_uuid` instead of `document_uuid`; lacks formalized stoppable/resumable semantics beyond UPSERT dedup; unclear what `--reset-and-redownload` means for a non-rebakeable import (sentinel vs reset contract).

### Principle Violations

- **Sidecar header field name mismatch** (render.rs:238). Architecture specifies `"document_uuid"` in `Sidecar` header (data_architecture_ingestion.md:375); Signal uses `"markdown_uuid"`. Both are stable UUIDv5 from `(chat_uuid, period_key)`, so semantically equivalent, but violates the cross-provider contract. Impact: load-side sidecar reader will reject or misparse if it enforces the canonical field name.
  - Fix: s/`markdown_uuid`/`document_uuid`/ at render.rs:238.
P1: Again, a struct used to both write and read this sidecar format would help.

- **Bare `Z` timezone on fallback timestamp** (render.rs:481). Architecture mandates "ISO-8601 with explicit offset" (data_architecture_ingestion.md:253). `iso_ts(0)` emits `"1970-01-01T00:00:00Z"` — bare `Z` instead of `+00:00`. Real timestamps from `date_sent_ms` are correctly formatted via `Utc.timestamp_millis_opt(...).to_rfc3339_opts(SecondsFormat::Secs, true)` which produces `+00:00`, but the sentinel fallback for empty messages violates the principle. Unlikely to occur in practice (empty buckets are skipped), but sets a bad pattern.
  - Fix: render.rs:481 → `"1970-01-01T00:00:00+00:00"`.

- **Unclear semantics for `--reset-and-redownload` on one-shot import**. Architecture (data_architecture_ingestion.md:177–195) defines the flag for rebakeable sources: wipe entity tables + cursors, re-fetch from upstream, let `dolt diff` surface gaps. Signal is non-rebakeable — the snapshot is the source-of-truth and won't reappear. Current behavior (extract/mod.rs:94–95): truncate data tables unconditionally, then walk the same snapshot again. This is sound — produces the same dedup results — but the contract and intent are unclear. Documentation says it "forces a full re-import," which for a backup could mean "confirm the on-disk snapshot hasn't changed." Should be explicit about one-shot semantics.
  - Recommendation: add doc comment clarifying that for Signal, `--reset-and-redownload` means "discard prior import, re-ingest the same snapshot, verify idempotence via dolt diff." Unlike API sources, there is no fresh upstream data to pull.
P3: Time to add this comment. I don't think it's a big deal. 

### Dead Patterns

- **Vestigial `upstream_cursor` field set to `None`** (render.rs:251). Every `RenderedMarkdown` carries an optional `upstream_cursor` (for forward-walk resumption tracking). Signal sets it to `None` since there's no cursor concept for file-based ingestion. This is correct, but marks Signal as outside the normal flow. Not a bug, but a useful signal that Signal is special.

- **No retry bookkeeping integration in practice**. Architecture (data_architecture_ingestion.md:446–530) specifies per-row `_bookkeeping` tables with `attempt_count`, `last_error`, `deleted_upstream_at`. Signal creates them (db.rs:68–74 calls `bookkeeping_ddl_for`), and `upsert_*` methods call `record_object_attempt` (e.g., db.rs:137), so the machinery is in place. However, since Signal doesn't have persistent failure modes (can't retry a decryption failure on a re-ingest — either it works or doesn't), the bookkeeping rows accumulate without being re-walked. Not dead code, but unused semantics.
P2: The way I think cursors should work with Signal is that we should Blake3 hash the entire file we are ingesting. And if we have already ingested it, then just completely skip all of it. And if we haven't, then let's run the ingestion. 

### Simplification

- **Period bucketing complicates one-shot semantics**. Render (mod.rs:2) bucketes docs into `(chat_id, period_key)` buckets, keyed by the configured `Period` (default Month). For a one-shot import, a single full-archive backup should probably emit one bucket per chat (no temporal slicing), not one per month. Current design is borrowed from chat-API providers (Slack, Anthropic, Beeper) where periodic bucketing avoids massive markdown files and supports incremental render-refresh on new messages. For Signal backup, all messages are already ingested in one shot, so the temporal bucketing is unnecessary complexity.
  - Recommendation: consider `Period::All` as the Signal default (render one `.md` per chat, not one per chat-month). Falls out of current code unchanged but clarifies intent.

- **Unused `files_root` override complicates config**. Extract (mod.rs:41–44) supports overriding the attachment directory (default `snapshot_root/files`). Useful if encrypted media is on a separate volume or media is shared across snapshots. However, for the first (and likely only) Signal ingestion pass, this is premature complexity. Current implementation is correct but suggests over-engineering for a one-shot use case.

### Cross-source Sharing with Chat-Human Family

Signal, Slack, and Beeper share the "human chat" GridRow shape and should converge on shared rendering patterns.

- **UUID generation family established but not shared**. Signal mints UUIDv5 via `signal_chat_uuid(source, chat_id)` etc. (translate/mod.rs:25–47), stable from `(source, chat_id, author_id, date_sent)` tuple. Slack, Beeper, and Anthropic follow the same pattern with provider-specific namespaces. The shape is consistent; the implementations are parallel code.
  - Low priority: Could share a generic `chat_human_uuid_v5(ns, source, chat_id)` helper if family grows, but current state is acceptable.

- **Markdown rendering mirrors shared shape**. Signal's markdown format (render/render_markdown, lines 353–356) — `- <span ...>**ts** _author_:</span> body` — mirrors Slack's bullet-list format. Both use shared helpers: `blob_cas::attachment_md` for image/file links (render.rs:363), `section_attrs` for frontend anchoring (render.rs:355). This is already well-unified; no changes needed.

- **Sidecar structure differs in field naming**. Both Signal and other chat providers emit sidecars with `header.source_fingerprint` and `header.render_version`, but the document-UUID field should be `document_uuid` (not `markdown_uuid`) to stay aligned with the canonical `Sidecar` struct and Slack/Beeper usage. This is the violation flagged above.

- **Timestamp handling consistent with principle**. Signal uses `Utc.timestamp_millis_opt(...).to_rfc3339_opts(SecondsFormat::Secs, true)` (render.rs:478–481), which produces `+00:00` offsets — matches Slack, Anthropic, et al. (except for the one fallback bare-`Z` case noted above). When_ts discipline is well-established across the family; Signal honors it.

- **Attachment handling via shared CAS**. Extract ingests attachments into the sibling `*.blobs.doltlite_db` via `blob_cas::store_bytes` (extract/mod.rs:304), keyed on `media_name` (SHA256-derived, deterministic from plaintext hash + local key). Render materializes them via `blob_cas::materialize_refs` (render.rs:170). This is the standard pattern every other provider uses; Signal is fully integrated.

- **No pre-seed, no pre-seeding-before-fetch complication**. Architecture (data_architecture_ingestion.md:454–468) says "pre-seed before fetch" is aspirational for providers with listing-then-detail splits (Notion, Anthropic). Signal doesn't have that — the backup format gives us IDs + content in one frame. Extract walks frames and UPSERTs immediately; no pre-seed step. This is correct and a simplification versus API providers.

- **No complex cursor / resume family patterns**. Signal doesn't have a cursor (it's all-or-nothing per snapshot). The extract flow is trivial: walk snapshot frames, UPSERT into DB, increment counters. No need for refresh windows, time-windowed walks, or per-channel resumption logic that Slack, Anthropic, GitHub all implement. Signal is intentionally the simplest case.
P2: The way I think cursors should work with Signal is that we should Blake3 hash the entire file we are ingesting. And if we have already ingested it, then just completely skip all of it. And if we haven't, then let's run the ingestion. 

### File and Line References

- **Sidecar structure** (render.rs:236–243): `markdown_uuid` → `document_uuid`.
- **ISO-8601 bare-Z fallback** (render.rs:477–481): 1970 sentinel on empty message.
- **Bookkeeping DDL** (extract/db.rs:68–74): created but underutilized.
- **Period bucketing default** (translate/mod.rs:60, parse.rs:123–121): `Period::Month` for all imports.
- **Reset semantics** (extract/mod.rs:94–96): implemented but contract unclear for one-shot.
- **Period bucketing loop** (translate/parse.rs:224–233): splits docs by `(chat_id, period_key)`.
- **UUID functions** (translate/mod.rs:25–47): stable v5 from content.
- **Shared CAS attachment handling** (extract/mod.rs:304–317, render.rs:170): well-integrated.
- **Render markdown format** (render.rs:353–356): matches Slack/Beeper style.
- **Blob materialization** (render.rs:156–178): uses universal helper.

---

## Slack

### Principle violations

- **`MANIFEST_TTL = 6h` on `conversations.list` / `users.list` sweeps breaks "efficiently incremental"** (`src/extract/mod.rs:47, 94-117, 165-182`). A second sync within 6h skips re-walking the upstream listing at all. Even though UPSERT dedup would make a re-walk a no-op, this introduces a stale-data window the principle disallows ("walk what the upstream API forces us to walk, write zero rows"). Fix: drop the TTL or make it opt-in.
NOTE: The reason we did this is because it's extremely slow to walk these lists. So this is a performance optimization and it should be explicitly allowed.
- **No pre-seed of message rows before detail fetch** (`src/extract/mod.rs:422-426`). Messages only appear after the API call succeeds; a crash mid-channel leaves no `payload IS NULL` evidence that a known-existed id wasn't fetched. Slack's listing→detail shape can support pre-seed like Notion/Anthropic.
P2: I do think it would be nice to do this.
- **No `deleted_upstream_at` for confirmed-deletion (404 on files)** — curl 22/4xx are recorded as transient errors in `extract/api.rs`, so retry will burn quota forever on permanently-gone uploads.
P2: Also good to have... 

### Dead patterns / cargo-culted code

- **`thread_root_uuid` backfill loop in `RawDb::open`** (`src/extract/db.rs:747-777`) — migration code for rows written by old versions; if the schema has been stable for months this is dead weight.
P0: I think we should probably get rid of this at this point. 
- **JSONL-tree fallback in translate** (`src/translate/mod.rs:205-220`, `read_method_envelopes`) — pre-doltlite raw-store layout, only still wired up for an in-crate fixture render test. Production has been doltlite-only for ages.
P0: I think it would make sense to get rid of this too. 
- **`pre_seed_blob_stub`** (`src/extract/api.rs:367`) — called during enumeration but the seeded row is never consulted for skip logic or surfaced to tooling. Either wire it to a retry walk or delete it.
P2: We should investigate further, but I think this also seems like something that could probably go... 

### Simplification opportunities

- **Unify the manifest-sweep state**. Three keys in `sync_scope_state` (`channels:archived=…`, `users`, per-channel cursors — `src/extract/db.rs:176-205`) could collapse to one `manifests_as_of` stamp; `reset()`'s `LIKE` pattern gets simpler.
P0: This affects the bites at rest and we should do it ASAP. 
- **Hoist rate-limit retry to shared ETL lib**. The `call_slack` backoff loop (`src/extract/api.rs:121-173`) is the same shape as Anthropic/ChatGPT; only the error enum differs.
P1: Unifying this might reveal other inconsistencies, so we should do it ASAP. 
- **Share blob pre-seed-before-fetch with other providers**. The seed-loop in `src/extract/api.rs:341-368` generalizes to a shared blob-orchestrator helper, which is also what's needed to fix the missing message pre-seed above.
P1: When you say it like this, it sounds more interesting. Maybe we should investigate it with higher priority. 

### Cross-source sharing opportunities

- **`ts_to_iso`** (`src/translate/mod.rs:57-80`) — the F64-precision-safe Unix→ISO-8601-with-`+00:00` conversion is a reference implementation worth lifting into the shared ETL crate. Multiple providers reimplement variants inline.
- **UUIDv5 recipes**. `slack_message_uuid(team, channel, ts)` (`src/translate/mod.rs:41-55`) plus Anthropic/GitHub/GitLab/Notion equivalents should live in one `frankweiler_schema::uuid_recipes` module so the GridRow doc-comments and runtime code can't drift.
- **Thread grouping / threaded-render shape** (`src/translate/render.rs:86-91`). Slack groups messages into one doc per thread; Beeper/Signal/Notion threads want the same. A shared `render_threaded_docs` would unify this.
P2: They are slightly different though, because beeper and signal need to group by timestamp or by time period. Actually, I think once we go to Slack direct messages, we will want something similar in Slack as well. So yes, I agree. I think we should probably try to introduce a generic intermediate chat message type that all chats can be turned in to (at translate time) and then render that. 
- **Blob materialization next to rendered `.md`** (`src/translate/render.rs:154-155`). ChatGPT, Anthropic, Notion all reimplement the same `BlobReader` → `blobs/` byte-stream step; belongs in a shared `frankweiler_etl::render` helper.

### Net

~85% compliant. Strong on commit boundaries (single `extract <name>: <stats>` per run), UPSERT dedup, sidecar fingerprinting, observability. Biggest gaps: manifest TTL (incremental-discipline), missing message pre-seed (retry-durability), missing 404→`deleted_upstream_at` on file blobs.

---

## YoLink

### Principle violations

* **Bookkeeping sidecar missing (port guide §6)** — `yolink_readings` has no `_bookkeeping` sidecar. Should carry `attempt_count`, `last_attempt_at`, `last_error` per-row to enable `--retry-failed` on-by-default (doc: "Retry and fetch durability"). See `/Users/thad/Imbue Dropbox/Thad Hughes/src/mixed_up_files/frankweiler/backend/etl/providers/yolink/src/extract.rs:125-142` (DDL) — missing boilerplate like `dr::bookkeeping_ddl_for()`.

* **Pre-seed pattern not implemented** — Per doc principle "Pre-seed before fetch" (port guide §6, doc §446-461), rows should exist with `payload=NULL` before fetch attempts. YoLink only writes on success; a mid-fetch crash on a partially-loaded device leaves no evidence of which windows were attempted. See `extract.rs:256-353` (`fetch_device`) — no pre-seed call before `curl`.

* **No explicit timestamp or fingerprint in raw schema** — `yolink_readings` lacks `fetched_at` or source-wire-payload hash. The doc principle (port guide §6.4) says bookkeeping lives on the sidecar, but there's no timestamp for "when was this window last pulled" to support resume-from-watermark correctly. The `last_ts_ms` in `yolink_devices` is a cursor (max of readings), not a fetch timestamp. See `extract.rs:133-139`.

* **No translated schema / GridRow projection** — Extract-only, no translate phase (doc §2240-2247 in sync/main.rs notes "yolink: skipped (extract-only, no render path)"). The doc explicitly calls out time-series data (doc §669) as a family planned for Garmin / IQAir that should eventually share a common `GridRow` schema. YoLink today skips this; readings sit in raw doltlite only, bypassing the sidecar / grid_rows pipeline. No schema unification as other providers do.

* **No sidecar JSON contract** — Translate and Load stages are skipped entirely (§2240-2247, §2312-2321). No `.grid_rows.json` sidecar, no `source_fingerprint`, no `RENDER_VERSION`. The doc principle (§365-401) says "the sidecar is the machine-readable projection" — YoLink has none. This breaks the framework's core contract that every entity table has a pair of sidecars for humans (markdown) and machines (JSON).

* **No window-level commit grain** — The comment in `extract.rs:1-4` says "one `dolt_commit` per window so re-fetches that change historical values land as auditable diffs in `dolt_log`", but that's misleading. The orchestrator still wraps everything in a single commit at sync-exit (per doc principle §294-312). A fetch that touches N windows produces one dolt_commit, not N. The comment contradicts the principle.

* **Hard cursor mutation without watermark isolation** — `last_ts_ms` is the only resume state, computed post-hoc from `MAX(ts_ms)` in readings (line 344-350). If a partial window succeeded (some readings written) then crashed before finishing the next window, the cursor advances to the max of *all* readings so far, risking gaps on resume. The window-stride cursor pattern (doc §314-327) should walk `[start, now]` in fixed windows; instead, YoLink computes a global watermark that can skip partial-window gaps. The design comment at lines 34-38 says devices are aligned across runs, but the cursor logic (lines 286-288) re-adjusts per-device after every fetch.

### Dead / cargo-culted patterns

* **Redundant `family_device_id` in two places** — Stored in config (`YolinkDevice.family_device_id`, config.rs:382) AND in the raw schema (`yolink_devices.family_device_id`, extract.rs:128, written at line 275). Since `id` (the device name) is the PK and never changes, re-storing `family_device_id` in the table is cargo-culted from providers with multi-entity hierarchies (channels in Slack, etc.). For YoLink it's just a redundant lookup. See config.rs:354 — "each entry's `name` is the row key" so no need for double storage.

* **`kind` column in yolink_devices** — Stored in both config and table (extract.rs:129, 276). Like `family_device_id`, this is sync-time configuration, not runtime state that changes across fetches. A pre-sync config validation should verify all devices are known; redundantly storing it in the DB is cargo-culted from providers that discover entity types at fetch time.

* **CSV header as implicit schema versioning** — The parser (extract.rs:55-66) hard-codes expected columns per `kind` ("temperature_humidity" → `("Temperature(℃)", ...)`). If Yolink changes the CSV format, the parse fails with a context-rich error (good). But there's no `RENDER_VERSION`-like mechanism to signal when the column mapping itself changes. The port guide doesn't mention CSV-format versioning because it's specific to YoLink's two device types; this is a proto-pattern that should either be elevated to a shared knob or documented as provider-specific.

### Simplification opportunities

* **Inline `dry_run` / `--reset-and-redownload` toggle for `yolink_readings` only** — Currently `reset()` deletes both tables (extract.rs:160-165). The doc principle (§177-201) says `--reset-and-redownload` wipes entity tables + cursors but preserves blobs (no CAS here, but the principle applies). A clearer split: reset the readings (the data table), optionally reset the devices (the cursor + config table). Today both are reset together; a user wanting to re-verify readings without re-seeding the device configuration has to rebuild both. See lines 160-165.

* **Consolidate overlap + stride into a single time-window struct** — Config has `overlap_minutes` and `window_days` as separate optional fields (config.rs:342-352). They're always used together (line 233-235: `overlap_ms = ... * 60_000; stride_ms = ... * 86_400_000; window_ms = stride_ms + overlap_ms`). Unpack once into a shared `TimeWindow` struct at config-load time, not in the hot path. Trivial perf gain but improves clarity. See extract.rs:233-235.

* **`CONSECUTIVE_FAILURE_BUDGET` should be per-source config, not hard-coded** — Line 298 sets `CONSECUTIVE_FAILURE_BUDGET = 30`. The doc principle (§424-444, especially "Retry policy is config, not code") says this knob should live in `YolinkSync` / `config.yaml`, not as a const. A user with a device that's intermittently offline can't tune the budget without recompiling. Add to `YolinkSync` struct (config.rs:334) with a sensible default.

* **`reading_pk` string format is brittle** — Line 168-170 uses `format!("{device}#{ts_ms}#{metric}")`. If any field contains `#`, collisions are possible. Use a proper composite key with separate columns or a stable hash (blake3, like blob CAS). The current format works for the test fixture (TNG cast names don't have `#`), but a future device named `"basement#1"` breaks silently. See extract.rs:168-170.

* **`curl` command shelling out instead of using HTTP library** — Lines 407-425 spawn `/usr/bin/curl` as a subprocess. The framework uses `latchkey curl` for auth (doc §408-422) and direct tokio HTTP clients elsewhere. YoLink's signed-URL auth doesn't fit latchkey (doc says this is "the special case"), but `std::process::Command` / `tokio::process` is less resilient than a proper HTTP client. No timeout guard visible (curl's `-m` flag would help), no retry logic inside the HTTP layer. Consider `reqwest` or similar. This isn't a violation per se (signed URLs do sidestep latchkey), but it's an outlier in the codebase.

* **Test snapshot names don't match kind names** — Snapshots use `parse_thsensor` (line 441) but the kind is `"temperature_humidity"` (line 59). The snapshot artifact at `src/snapshots/frankweiler_etl_yolink__extract__tests__parse_thsensor.snap` uses a shorter alias. Rename test or kind to be consistent: either `"th_sensor"` or `"temperature_humidity"` everywhere.

### Cross-source sharing (time-series family)

The doc (§627-674, especially §669) lists "time-series sensor data — yolink today; Garmin fitness and IQ Air air quality planned" as a shared family, but YoLink's schema and extract patterns aren't set up for reuse.

* **Borrow YoLink's window-stride cursor pattern for Garmin / IQAir** — The time-window walk (lines 33-39, 233-235, 301-341) is sound: fixed strides, overlap to catch late-arriving edits, per-device tracking. Extract the window logic into `frankweiler_etl::time_window` or similar, reusable by Garmin (device fitness samples) and IQAir (air-quality readings). YoLink's implementation is tightly coupled to CSV parsing; abstract the cursor walk from the fetch body.

* **Unified time-series reading schema** — `yolink_readings` (extract.rs:133-139) should be the prototype for Garmin / IQAir. All three have:
  - `device_name` (foreign key to device config)
  - `ts_ms` (timestamp in milliseconds, deterministic from upstream or synthesized)
  - `metric` (channel name: `temperature_c`, `water_meter_gal`, or Garmin `heartrate`, `steps`)
  - `value` (numeric sample)
  - `id` (composite PK: `device#ts#metric`)

  Garmin and IQAir should use the same table layout. A future GridRow projection can unify all three into a single time-series view. Today there's no shared DDL helper or Rust struct; YoLink hard-codes the table shape. See extract.rs:125-142.

* **Signature algorithm abstraction for device-secret auth** — YoLink's `build_signed_url()` (lines 365-403) reverses a device-read secret (family_device_id + device_udid) into an MD5 signature. Garmin and IQAir will have their own per-device secrets and signing schemes. Create a `DeviceAuth` trait:
  ```rust
  pub trait DeviceAuth {
      fn build_request_url(&self, dev: &Device, start_ms: i64, end_ms: i64) -> Result<String>;
  }
  ```
  YoLink, Garmin, IQAir each impl it. Move signing logic out of `extract.rs` into a `auth.rs` module shared by the family. Today YoLink inlines it; that pattern won't scale to three providers.

* **Pre-seed + bookkeeping for all time-series sources** — Once Garmin / IQAir join the family, they must share YoLink's pre-seed + bookkeeping pattern (once it's fixed). The family should establish a baseline: every provider pre-seeds devices, uses the bookkeeping sidecar, and implements `--retry-failed`. YoLink is the template; fixing it now (before Garmin / IQAir arrive) sets the bar. See port guide §6, doc §446-530.

* **Shared GridRow kind taxonomy for time-series** — The doc (§627-674) lists family names like "Chat (human)" and "Code review threads" but doesn't name the time-series family. Once Garmin and IQAir ship, define a shared `GridRow.kind` for sensor data (e.g., `kind = 'Temperature' | 'Humidity' | 'Heart Rate' | 'PM2.5'`) and a unified translate pipeline. YoLink has no translate step today; that's the gap. Sketch what a shared `translate/grid_rows.rs` would look like and unblock Garmin / IQAir to reuse it.


---

## Cross-source unification opportunities

### Shared UUIDv5 recipe module
- **Providers**: Slack, Anthropic, ChatGPT, Notion, GitHub, GitLab, Beeper, Signal, Contacts, Perseus, (planned: Gemini, Garmin, IQAir)
- **Concern**: Every provider re-implements `Uuid::new_v5(NS, format!("{provider}:{scope}:{kind}:{id}"))`. Namespaces are open-coded constants. Recipe strings drift from doc-comments; near-miss collisions (e.g. GitLab note UUID missing `mr_iid` qualifier) silently corrupt cross-source joins.
- **Proposed shape**: `frankweiler_schema::uuid_recipes` crate exporting one function per (provider, entity) pair — `slack_message_uuid(team, channel, ts)`, `anthropic_message_uuid(org, conv, msg)`, `gitlab_note_uuid(proj, mr_iid, note_id)`, etc. — with namespace constants colocated and snapshot tests pinning every recipe's UUID output across releases. GridRow doc-comments cite the function name, not the recipe string.
PO: Getting stable identifiers right is incredibly important for the bytes at rest format, but I'm not sure centralizing it is the right idea. I want people to be able to implement their own ingestion and extraction code without having to necessarily register it in some central library. To me, the right thing to do is to just always construct UUIDs via function and put those functions in a known place inside of every data source. 
- **Called out by**: `slack.md`, `anthropic.md`, `chatgpt.md`, `gitlab.md`, `beeper.md`, `signal.md`, `perseus.md`, `contacts.md`

### Shared timestamp → ISO-8601 helper
- **Providers**: Slack, Beeper, Signal, Anthropic, ChatGPT, Notion, GitHub, GitLab, Email
- **Concern**: Every chat provider does its own Unix-millis or RFC3339 conversion; almost all emit bare `Z` instead of explicit `+00:00`, violating the "strict ISO-8601 with offset" principle. ChatGPT has two near-identical `bump_micros` / `bump_iso` helpers diverging on format string. Beeper passes `use_z=true`. Signal's 1970 sentinel is bare-Z. Most providers don't validate that upstream timestamps actually carry an offset.
- **Proposed shape**: `frankweiler_etl::timestamps` with:
  - `iso_from_unix_ms(i64) -> String` (always `+00:00`)
  - `iso_from_unix_seconds_f64(f64) -> String` (precision-safe; lift Slack's `ts_to_iso`)
  - `normalize_iso(&str) -> Result<String>` (validates offset, rewrites `Z`→`+00:00`)
  - `bump_micros(&str, n: u32) -> String` (microsecond ordering for synthesized child rows)
- **Called out by**: `slack.md` (`ts_to_iso` as reference impl), `beeper.md` (use_z bug), `signal.md` (bare-Z sentinel), `chatgpt.md` (duplicated helpers), `anthropic.md`, `notion.md`, `gitlab.md`, `github.md`, `email.md`
P0: I really like the idea of a shared timestamp handling library where all the timestamp handling funnels through. 

### Shared retry / backoff machinery
- **Providers**: Slack, Anthropic, ChatGPT, GitHub, GitLab, Email
- **Concern**: Per-provider rate-limit + exponential-backoff loops with hardcoded constants (GitHub: `RETRY_MAX=7`, `RETRY_INITIAL_BACKOFF_MS=2000`; Slack identical shape). Architecture demands "retry policy is config, not code" and per-source `config.yaml` knobs; nothing implements it.
- **Proposed shape**: `frankweiler_etl::retry::{RetryPolicy, retry_with_backoff}` taking a `RetryPolicy { max_attempts, initial_backoff_ms, max_backoff_ms, give_up_after_days }` loaded from per-source `sync:` block. Provider clients (Slack, Anthropic, ChatGPT, GitHub, GitLab) wrap their HTTP call in `retry_with_backoff(&policy, || ...)`. Surface `Retry-After` / `x-ratelimit-reset` parsing in a shared `parse_retry_after` helper.
- **Called out by**: `slack.md`, `_shared_layer.md`, `github.md`, `chatgpt.md`, `email.md`, `anthropic.md`
P0: Let's set up a shared retry config that everyone uses to configure their retry in the YAML. And there is a shared implementation that helps track failures and schedule exponential backoff, etc. This is probably a solved problem in some ways. I wonder if we can use a pre-baked solution. 

### Orchestrator-owned `--retry-failed` loop
- **Providers**: all incremental-API providers (Slack, Anthropic, ChatGPT, Notion, GitHub, GitLab, Email, Contacts, YoLink)
- **Concern**: Architecture mandates `--retry-failed` (default true). `failed_ids` infra exists in `doltlite_raw.rs` but no orchestrator CLI wiring, no `ExtractControl.retry_failed` field, no provider uses it. Each provider silently records errors that never get re-walked.
- **Proposed shape**: Add `retry_failed: bool` to `ExtractControl`; wire `--retry-failed` / `--no-retry-failed` in `sync/src/main.rs`. Before each provider's normal fetch, orchestrator queries `*_bookkeeping` tables for `payload IS NULL OR last_error IS NOT NULL`, feeds the IDs to the provider via `FetchOptions::retry_ids: Vec<String>`. Provider checks the list and jumps straight to detail-fetch for those IDs, skipping listing. Eliminates per-provider retry logic (Anthropic's `--conv-uuid`, etc.).
- **Called out by**: `_shared_layer.md`, `anthropic.md`, `chatgpt.md`, `email.md`, `beeper.md`, `notion.md`, `github.md`, `yolink.md`
DO NOT DO THIS: I do not think retry logic belongs in the orchestrator, I think it is an extraction concern. It would be great if the shape of it is typically shared though.

### Shared `deleted_upstream_at` distinction
- **Providers**: Slack, Anthropic, ChatGPT, Notion, GitHub, GitLab
- **Concern**: 404 on a known-existed entity is recorded identically to a transient error; providers retry deleted upstream rows forever. No column distinguishes "confirmed gone" from "couldn't fetch this time."
- **Proposed shape**: Add `deleted_upstream_at TEXT NULL` to every data table (or to bookkeeping). Shared helper `mark_deleted_upstream(tx, table, id)` in `doltlite_raw.rs`. Shared HTTP error classifier `ErrorClass::{Transient, RateLimit, Deleted(404), Permanent}` consumed by the shared retry layer.
- **Called out by**: `slack.md`, `anthropic.md`, `chatgpt.md`, `notion.md`, `github.md`, `gitlab.md`
P2: Yes, I think marking these tombstones would be useful. 

### Shared bookkeeping / pre-seed helpers
- **Providers**: all doltlite-backed (Slack, Anthropic, ChatGPT, Notion, GitHub, GitLab, Email, Contacts, Beeper, Signal, YoLink)
- **Concern**: Each provider hand-rolls `bookkeeping_ddl_for(table)` plumbing + manual `ensure_object_row` / `record_object_attempt` / `record_object_error` calls. YoLink lacks bookkeeping entirely. Notion has unused `ensure_id()` helper. The trio of calls (ensure → attempt → error) is always paired but no type-level enforcement.
- **Proposed shape**:
  - `bookkeeping_tables!(table1, table2, ...)` declarative macro generating DDL + column constants.
  - `ObjectLifecycle { table, tx, id }` builder with `ensure()` / `record(Result<_, E>)` methods — ensures pre-seed + attempt-or-error always paired.
  - Document blueprint: discovery-listing pass MUST pre-seed all known IDs with `payload=NULL` before detail-fetch (where API shape allows).
- **Called out by**: `_shared_layer.md`, `notion.md`, `anthropic.md`, `chatgpt.md`, `github.md`, `gitlab.md`, `slack.md`, `email.md`, `yolink.md`, `beeper.md`
P0: Yes, this sounds like a good way to make sure everything is being consistent everywhere.

### Chat-human family unification (Slack / Beeper / Signal / Email)
- **Providers**: Slack, Beeper, Signal, Email (Fastmail JMAP), planned direct Signal/iMessage readers
- **Concern**: All four project messages → threads → participants → attachments but raw schemas diverge (Email has `emails` + `threads` + `email_mailboxes` joins; Slack has `messages` flat; Beeper reads index.db; Signal reads backup frames). Per-message GridRow shape is unified but kind taxonomies leak network names (Beeper emits `"Signal Message"` vs Slack `"Channel Message"`), breaking cross-network grouping. Period bucketing borrowed from API providers is overkill for one-shot Signal imports.
- **Proposed shape**:
  - `frankweiler_etl::chat_human::translate` shared module with `render_threaded_docs(messages, period)`, common `kind` taxonomy (`Channel Message`, `Direct Message`, `Thread Reply`, `Reaction`), and shared period-bucketing knob (Signal/email should default `Period::All`).
  - Shared `chat_message`, `chat_thread`, `chat_participant_join` raw-table template (each provider extends with provider-specific columns).
  - Beeper's `source_label = "Beeper:Signal"` composite documented as the canonical multi-network labeling convention.
- **Called out by**: `beeper.md` (kind taxonomy mismatch), `signal.md` (Period::All), `email.md` (chat-human family proposal), `slack.md` (shared threaded-render helper)
P2: Let's save the rendering concerns for later. 

### LLM-chat family unification (Anthropic / ChatGPT / planned Gemini)
- **Providers**: Anthropic, ChatGPT, (planned: Gemini, Claude Web)
- **Concern**: Both emit GridRow kinds `User Input | LLM Response | LLM Thinking | Tool Call`; both pull conversations → messages → content blocks → attachments; both use two-hop signed-URL attachment download; both call an identity endpoint (`/api/account` vs `/me`) and stamp `account_id` on rows. Anthropic explodes messages from `chat_messages` payload at translate time; ChatGPT has Messages tables. Render ordering diverges (ChatGPT's parent-chain walk vs grid_rows sort).
- **Proposed shape**:
  - `frankweiler_etl::chat_llm` schema crate (users, orgs, conversations, messages, content_blocks, attachments) extensible per-provider (e.g. `anthropic_project_id`).
  - Shared `LlmChatRenderer` trait emitting GridRow rows + sidecar; both providers plug in via provider-specific message iterators.
  - Shared `IdentityFetcher` helper for `me`/`account` endpoint.
  - Shared block-anchor naming (`section_uuid_for_block`).
- **Called out by**: `anthropic.md` (raw schema + translate + blob parity), `chatgpt.md` (LlmChatRenderer trait, IdentityFetcher, attachment dance)
P2: Let's save the rendering concerns for later. 

### Code-review family unification (GitHub / GitLab)
- **Providers**: GitHub, GitLab
- **Concern**: Both populate GridRow `git_sha` + `external_id` correctly. Both use identical scope-state cursor + refresh-window logic via `since_for_scope`. Both compute identical canonical-JSON + RENDER_VERSION fingerprint. Raw schemas justifiably differ (GH has 4 tables, GL has 2), but fingerprint code and `CommentRow`/`NoteRow` projection logic are copy-pasted. GitHub re-fetches all PRs unconditionally; GitLab skips unchanged MRs. GitHub lacks GitLab's optimization.
- **Proposed shape**:
  - `frankweiler_etl::code_review` shared module with `CodeReviewThreadRow` projection, `fingerprint_code_review(payload, comments)`, and skip-unchanged helper.
  - Document shared family contract: every code-review GridRow MUST carry `git_sha` + `external_id`.
  - Backport GitLab's skipped-unchanged optimization to GitHub.
- **Called out by**: `github.md`, `gitlab.md`
P2: Let's save the rendering concerns for later. 

### Time-series family unification (YoLink / planned Garmin / IQAir)
- **Providers**: YoLink today; Garmin, IQAir planned
- **Concern**: YoLink is the only time-series provider and has hardcoded everything: window-stride cursor, CSV-format-versioning, signing scheme, table DDL, `CONSECUTIVE_FAILURE_BUDGET` const. No translate phase, no GridRow projection, no sidecar — breaks the universal raw→translate→load contract. Cursor watermarking via `MAX(ts_ms)` is gap-prone.
- **Proposed shape**:
  - `frankweiler_etl::time_window` module with `WindowStrideCursor { stride, overlap, start, now }` iterator (per-window, not global-MAX watermark).
  - `DeviceAuth` trait for per-device signed-URL schemes.
  - Shared `sensor_readings` raw-table template: `(device_name, ts_ms, metric, value, id_composite_pk)`.
  - Shared time-series `GridRow.kind` taxonomy (`Temperature | Humidity | Heart Rate | PM2.5 | ...`) and a `frankweiler_etl::time_series::translate` projecting all three providers.
- **Called out by**: `yolink.md` (entire family section), `_shared_layer.md` (`periodize.rs` is narrowly YoLink-only)

### Shared blob materialization at render
- **Providers**: ChatGPT, Anthropic, Notion, Slack, Signal, Email, Beeper, Contacts
- **Concern**: CAS-stored blobs are reified to `blobs/<uid>.<ext>` next to the rendered `.md` by every provider via inline `BlobReader` → byte-stream loops. Contacts stores photos inline in payload (no CAS) and reinvents the materialize step.
- **Proposed shape**: `frankweiler_etl::render::materialize_blob_refs(refs, out_dir)` shared helper for CAS-backed providers; `materialize_inline_blobs(parsed_payload, out_dir)` for the inline-payload case (Contacts, future Signal-attachments-in-frame). Both write through one chokepoint so disk-full / write errors are surfaced uniformly (today Contacts swallows errors with `.ok()`).
- **Called out by**: `slack.md`, `chatgpt.md`, `anthropic.md`, `contacts.md`, `email.md`, `signal.md`, `_shared_layer.md`

### Shared attachment-fetch dance (two-hop signed URL)
- **Providers**: ChatGPT, Anthropic, Slack (files), planned Gemini
- **Concern**: Same multi-step ritual: scan payload for refs → dedupe by file-id → auth-fetch metadata → fetch signed URL → curl signed URL → store_bytes in CAS → record_blob_error on failure. Repeated verbatim across providers.
- **Proposed shape**: `frankweiler_etl::blob_cas::TwoHopFetcher` trait + `fetch_and_store_blobs(provider_id, refs, &client)` helper. Provider supplies `lookup_signed_url(file_id)`; helper does the rest including bookkeeping.
- **Called out by**: `chatgpt.md`, `anthropic.md`, `slack.md`

### Canonical sidecar header field naming
- **Providers**: Slack, Beeper, Signal, Notion, Email, Contacts, Anthropic, ChatGPT, GitHub, GitLab, Perseus
- **Concern**: Architecture spec field is `document_uuid` but multiple providers emit `markdown_uuid` (Signal, Contacts, Anthropic, Notion). ChatGPT correctly uses shared `frankweiler_etl::sidecar::Sidecar`. Field-name drift defeats load-side enforcement.
- **Proposed shape**: All providers import `frankweiler_etl::sidecar::{Sidecar, SidecarHeader}` (no per-provider struct). Rename `markdown_uuid` → `document_uuid` everywhere. Load-layer reader asserts the canonical field name.
- **Called out by**: `signal.md`, `contacts.md`, `notion.md`, `chatgpt.md` (already compliant — model citizen)
P0: Let's do work out a struct that specifies the shape of these rows that we're going to index and use that struct both at write time and at read time. 

### Shared fingerprint module
- **Providers**: GitHub, GitLab, Notion, ChatGPT, Anthropic, Slack, Beeper, Signal, Contacts, Perseus
- **Concern**: Every provider hashes `RENDER_VERSION + canonical_json(payload) + sorted_children` identically. ChatGPT recomputes canonical JSON on every render even when skipping. No shared module; behavior drifts.
- **Proposed shape**: `frankweiler_etl::fingerprint` with `compute_fingerprint(render_version, sections: &[&[u8]])` builder. Cache canonical-JSON form in a `fingerprint` column at extract time so translate can fingerprint-skip in O(hash-compare).
- **Called out by**: `chatgpt.md` (recompute waste), `gitlab.md` (duplicated with GitHub), `github.md`, `notion.md`

### Shared scope-state / cursor helpers
- **Providers**: GitHub, GitLab, Email, Contacts (CardDAV sync-token), Slack (channel cursors)
- **Concern**: `since_for_scope` + `sync_scope_state` already shared between GitHub/GitLab (good). Slack uses three keys in `sync_scope_state` that could collapse to one stamp. Email has redundant `jmap:` prefix on state-token keys. Contacts persists sync-tokens per addressbook. Patterns differ but the shape is identical: "for scope X, last-known-good cursor = Y."
- **Proposed shape**: `frankweiler_etl::scope_state::{since_for_scope, save_scope_state, load_scope_state}` already exists — extend with `manifest_as_of(scope)` for sweep-style providers and document the canonical key shape `<scope_id>:<cursor_kind>` (no provider prefix).
- **Called out by**: `slack.md`, `email.md`, `github.md`, `gitlab.md`, `contacts.md`

### Shared run / phase state machine
- **Providers**: orchestrator + all providers
- **Concern**: `PhaseOutcome`, `LoadOutcome`, `FetchSummary` are per-provider with no cross-phase union. Future status-line rendering, JSON summaries, and interrupt handling have to know about each variant. Beeper's `events_orphaned` and GitHub's unused `errors` field show the drift.
- **Proposed shape**: `frankweiler_etl::run_state::{RunPhase, PhaseStatus, RowCounts { added, modified, removed, failed }}`. Every provider's FetchSummary embeds a `RowCounts`. Orchestrator renders status from this uniform shape.
- **Called out by**: `_shared_layer.md`, `beeper.md`, `github.md`, `perseus.md` (vestigial `skipped` field)

### `ObsArgs` + single-commit-per-source guards
- **Providers**: orchestrator + every standalone `<provider>_download` binary
- **Concern**: Architecture mandates `#[command(flatten)] obs: ObsArgs` and single-`dolt_commit`-per-source, but neither has compile-time enforcement. Vestigial download binaries may silently drop ObsArgs. A provider that called `dolt_commit` internally would not be stopped.
- **Proposed shape**:
  - `frankweiler_etl::cli::standard_args!()` declarative macro that bundles `ObsArgs` + `ExtractControl` flags; every binary uses it.
  - `frankweiler_etl::commit_guard::ExtractCommitGate` that providers receive in lieu of raw `&mut DoltConn` — only the orchestrator holds the `commit_run` token.
- **Called out by**: `_shared_layer.md`
CLARIFY: What is ObsArgs?

### Shared CLI flags for invalidation semantics
- **Providers**: all
- **Concern**: `--reset-and-redownload` means different things for incremental APIs (wipe + re-fetch), one-shot imports (re-ingest same snapshot, Signal), and immutable corpora (no-op or local cleanup, Perseus). Today it's the same flag with silently different behaviors.
- **Proposed shape**: Split into `--reset-entities` (wipe data tables + cursors), `--refetch-blobs` (already exists), `--reverify-snapshot` (Signal-style re-ingest), and have immutable providers (Perseus) reject the flag with a guidance error.
- **Called out by**: `perseus.md`, `signal.md`, `_shared_layer.md`, `yolink.md`
DO NOT DO: It's fine.

### Shared hierarchical UUID + alignment-edge schema
- **Providers**: Perseus today; planned TEI / archival / multilingual corpora
- **Concern**: Perseus's `(book, chapter, section, language, sentence_index)` UUID derivation and bilingual sentence-alignment edge schema (`(src_markdown_uuid, src_anchor_uuid, dst_markdown_uuid, dst_anchor_uuid, label)`) are reusable templates for any hierarchical / parallel corpus.
- **Proposed shape**: `frankweiler_schema::hierarchical_uuid::{section_span_uuid(work, level1, level2, lang, span_kind, span_idx)}` generalization; `frankweiler_etl::edges::AlignmentEdge` shared struct.
- **Called out by**: `perseus.md`

### Shared narrative logging
- **Providers**: orchestrator
- **Concern**: Per-phase `tracing::info!(source = %name, kind = ..., ...)` lines are repeated at every phase entry/exit in `sync/main.rs`. Drift = inconsistent operator UX.
- **Proposed shape**: `log_phase_step(phase, source, step)` helper in the shared layer; every phase pre/post calls it.
- **Called out by**: `_shared_layer.md`

### Shared timestamp validation at translate
- **Providers**: Anthropic, Notion, GitHub, GitLab, Email
- **Concern**: Every provider assumes upstream timestamps already meet the explicit-offset rule; none validates. Upstream schema drift would silently break global sort.
- **Proposed shape**: `assert_iso_with_offset(&str)` lint helper called in `translate/grid_rows.rs` for every `when_ts` write; logs `warn!` on bare-Z or naive.
- **Called out by**: `anthropic.md`, `notion.md`, `github.md`, `gitlab.md`, `email.md`

### Privacy / span redaction boundary
- **Providers**: all (orchestrator)
- **Concern**: OTLP export wraps spans containing potentially sensitive payloads. Architecture flags this as deferred; no redaction layer exists.
- **Proposed shape**: `frankweiler_etl::obs::redact` helper consumed by every `tracing::info!` that mentions item bodies. Until built, document the gap in operator-facing docs.
- **Called out by**: `_shared_layer.md`
