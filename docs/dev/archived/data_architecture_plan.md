# Data architecture plan of attack

Plan derived from the audit ([`data_architecture_audit.md`](data_architecture_audit.md))
and Thad's inline comments on it. Companion to [`data_architecture_ingestion.md`](../data_architecture_ingestion.md)
(the principles) and the audit (the findings).

## Guiding priorities

1. **Bytes at rest come first.** Anything that fixes the shape of what
   we persist out of Extract — raw entity tables, bookkeeping columns,
   sidecar JSON, UUID recipes — gets done **before** translate-side
   polish or rendering improvements. We can rewrite renderers any
   time; we cannot rewrite years of accumulated raw stores.
2. **"Show me your tables"** — every provider has a single
   well-known schema file (load-bearing code, not parallel
   documentation) at the same fixed path, with rich inline comments
   in the spirit of a proto declaration. Reading 12 files side-by-side
   answers "what does the world look like?" without anyone writing
   a separate doc.
3. **Don't make up data.** A new principle to add to
   `data_architecture_ingestion.md`: we never synthesize values (timestamps,
   identifiers, content) that weren't given to us. Fallback paths that
   silently fabricate data mask incompleteness — they get replaced
   with loud warnings that surface in the per-run JSON summary.
4. **Retry logic stays inside extract, per provider.** Shape and
   config are shared; the orchestrator does not run a retry loop.
   ([P0.6](#p06-shared-retry-config-schema-extract-side-impl))

---

## P0 — bytes-at-rest shape and visibility

### P0.1 Self-documenting schema source-of-truth per provider ✅

**Goal**: every provider has **two** declarations-only Rust files
at fixed paths:

- **`providers/<name>/src/schema_raw.rs`** — describes what comes
  off the wire from this upstream provider and how it's persisted in
  the raw doltlite store.
- **`providers/<name>/src/schema_translate.rs`** — describes the
  normalized representation(s) translate emits. There may be more
  than one of these (the `_translate` suffix can be pluralized or
  split — e.g. `schema_translate_chat.rs` once we have a shared
  chat schema) since one provider may project into multiple
  downstream schemas. These will often derive from shared cross-
  provider abstractions (generic chat schema, code-review-thread
  schema, time-series schema). **Most of these declarations are
  serde-shaped Rust types, not SQL DDL** — the translate output is
  in-memory normalized values plus sidecar JSON. (Whatever SQL
  schema the `grid_rows` projection table uses is owned by Load,
  not by per-provider translate.)

Both files are **proto / pydantic-flavored**: types, enums, DDL
constants, and trivial schema-local helpers (e.g. an enum
`Display`). **No data-manipulation code.** `ensure_<entity>_row`,
parameter-binding, UPSERT builders, etc. stay in
`extract/db.rs` / `translate/...` and *import* from these schema
modules. The schema files should remain a very light dependency
that can be read top-to-bottom in one sitting.

Concretely, `schema_raw.rs` contains:

- `const <TABLE>_DDL: &str = "CREATE TABLE IF NOT EXISTS ...";` per
  entity, bookkeeping, and `blob_refs` table — with a `///` block
  above each explaining upstream provenance, the PK choice, what
  each column is for, where `when_ts` comes from, and a pointer to
  the UUID recipe function.
- `pub const ALL_DDL: &[&str] = &[...];` — the list `RawDb::open`
  iterates, replacing today's inline DDL in each provider's
  `extract/db.rs`.
- Enums for upstream-shaped enumerations (e.g. message types) where
  it makes the wire shape clearer.
- Schema-evolution migration **constants** (DDL strings) co-located,
  each with a doc-comment noting when it becomes safe to delete.
  The migration *runner* code lives in `extract/db.rs`.

`schema_translate.rs` contains the analogous declarations for the
normalized translate-side representation, but in **serde form**:
the Rust types (with `Serialize` / `Deserialize`) that represent the
provider's normalized rows / messages / threads / etc., the
sidecar header type when the provider needs a custom one, and
re-exports of the shared cross-provider types it conforms to (e.g.
the eventual `LlmChatTurn` from P1.4 or `ChatMessage` from P1.5).
Again, declarations only — no manipulation code.

No parallel `RAW_SCHEMA.md`. The load-bearing code *is* the
documentation; the "show me your tables" guarantee comes from
opening 12 × 2 schema files at the same fixed paths and seeing the
same shape.

**Why Rust over a `.sql` or proto file**: `rustdoc` on `const` DDL
strings gives us the same comment richness as proto without a
second toolchain, the `extract/db.rs` import side gets type-safe
constants instead of stringly-named SQL files, and shared
abstractions for `schema_translate.rs` (chat, code-review, time-
series) want to be Rust types anyway.

**Why now**: cheapest, highest-leverage P0. Doing it **before** the
schema unifications below will (a) surface inconsistencies we don't
yet know about and (b) give us a stable artifact to diff against as
the unifications land.

**Action**: prototype on `notion` or `anthropic` first (closest to
the model already), nail down what shared helpers go in and what
stays per-provider, then bring each provider up to the convention.

### P0.2 Shared `Sidecar` / `GridRow` struct, used at read AND write ✅

> Cross-cutting comment from Thad: "Let's do work out a struct that
> specifies the shape of these rows that we're going to index and use
> that struct both at write time and at read time" — and: "shared
> struct that everyone has to populate for these sidecar fields. It
> could share the same schema as a sidecar row."

**Today**: providers emit `.grid_rows.json` sidecars whose shape is
defined in `frankweiler/backend/etl/src/sidecar.rs` (`Sidecar`,
`Header`), and Load reads them with its own deserialize path. Slack uses
`document_uuid` in the header; signal calls it `markdown_uuid`
(audit: signal). Multiple agents pointed out the field names drift.

**Goal**:

- One canonical Rust struct (`Sidecar { header: SidecarHeader, rows:
  Vec<GridRow> }`) defined once.
- The GridRow struct matches the SQL schema in `backend_index.doltlite_db`
  and round-trips cleanly to and from the SQL rows.
- Every provider's translate writes through it; Load reads through it.
- No string-keyed serde maps anywhere; the schema is checked at compile
  time at both ends.
- Canonical field name: **`markdown_uuid`**. The thing we're indexing
  is the rendered markdown documents — name it for what it is. Updates
  needed: anywhere currently using `document_uuid` (architecture doc,
  Slack, etc.) flips to `markdown_uuid`. (Audit's "signal uses
  `markdown_uuid` instead of canonical `document_uuid`" finding gets
  inverted — signal was right.)

**Action**: audit every translate path, route through one
`emit_sidecar(...)` helper that takes the struct.

### P0.3 Content fingerprint in raw; translation state in the backend index

**Important correction to the original framing**: the raw stores must
stay agnostic to translation. There can be multiple translation
schemes over time. The raw store's job is only to record, for each
entity, a stable **Blake3 fingerprint of its content** — so anyone
downstream can tell whether the content changed. Translation
bookkeeping ("which fingerprint did I last translate, and into what")
lives separately, in `backend_index.doltlite_db`.

Two halves:

**P0.3a — `content_fingerprint` column on every raw entity row.**

- Blake3 of the canonical bytes of the row's content (typically the
  upstream `payload`, possibly including a few stable bookkeeping
  fields — to be designed).
- Updated by extract on every UPSERT. Cheap to compute, byte-stable
  across re-fetches when the upstream hasn't changed.
- Cargo-cult-friendly: same column shape on every entity table; lands
  in the shared bookkeeping DDL helper (P1.1).
- Replaces the per-provider `source_fingerprint` schemes that
  currently get re-computed at translate time and stored in the
  sidecar header. Translate just reads the column.

**P0.3b — translation-state table in `backend_index.doltlite_db`.**

- Keyed by `(provider, entity_uuid, translation_scheme, render_version)`.
- Stores the `content_fingerprint` that was last translated under
  that scheme + version.
- Translate's "what's stale" query becomes: left-join raw entity
  rows against this table; rows where the raw fingerprint differs
  (or no row exists yet) need work.
- Naturally supports multiple translation schemes coexisting (today
  we have one — markdown + GridRow — but the door's open).

**Side benefit**: per-source "X / Y items translated" progress
reporting becomes a SQL count, not a filesystem walk of
`rendered_md/`. (The audit called this out as a translate-side
observability gap.)

**Action**: design the `content_fingerprint` recipe (input bytes
exactly), add the column to the bookkeeping DDL helper (P1.1), design
the translation-state table schema in the backend index, retrofit one
provider as the reference. Migrate the rest one at a time.

### P0.4 PK / UUID recipes — homed in the schema files (no `uuid.rs`) ✅

> Thad: "Getting stable identifiers right is incredibly important for
> the bytes at rest format, but I'm not sure centralizing it is the
> right idea. I want people to be able to implement their own
> ingestion and extraction code without having to necessarily register
> it in some central library. The right thing is to just always
> construct UUIDs via function and put those functions in a known
> place inside of every data source."

**Convention that landed (during P0.1 rollout)** — recipes live
**inside the schema file they key into**, not a separate `uuid.rs`:

- **Raw-store synthesized PK recipes** (signal's
  `chat_item_id_recipe`, yolink's `reading_id_recipe`, github's
  `pr_pk`, gitlab's `mr_pk_recipe` + `discussion_pk_recipe`) live in
  `providers/<name>/src/extract/schema_raw.rs` — next to the DDL
  constant that says "this column is the PK". Both writer (extract)
  and reader (translate, dedup-key formatters in synthesize, etc.)
  import the same `pub fn` so the recipe can't drift between sides.
- **Translate-side GridRow UUIDv5 recipes** (e.g. beeper's
  `beeper_room_uuid` / `beeper_event_uuid`, github/gitlab grid-uuid
  fns) live in `providers/<name>/src/translate/...` — they target a
  different namespace (cross-provider grid identity) than raw-store
  PKs, so co-locating them with the raw schema would conflate two
  concerns. When `schema_translate.rs` exists for a provider, that's
  their natural home; until then they stay in the translate module
  that emits them.
- Providers whose entities use native upstream UUIDs (anthropic,
  notion, chatgpt) have no recipes to declare; their `schema_raw.rs`
  module rustdoc notes this explicitly so a reader doesn't go
  hunting for missing recipe functions.

**Why this beats `providers/<name>/src/uuid.rs`**: a separate
`uuid.rs` would have one foot in extract and one in translate, with
no way to say which side owned it. Co-locating each recipe with the
schema it keys into puts the recipe's contract — "what PK does this
column hold" or "what UUID does this `GridRow` field hold" — within
one rustdoc-hop of the recipe function itself.

**Status**: convention adopted across all 9 providers landed so
far (anthropic, chatgpt, signal, contacts, yolink, github, gitlab,
notion, beeper). No drift between writer and reader callsites
where recipes were lifted.

### P0.5 Shared timestamp utility crate; no fabricated timestamps ✅

> Thad: "I really like the idea of a shared timestamp handling library
> where all the timestamp handling funnels through" — and elsewhere:
> "we should probably leave ourselves a note in the data architecture
> doc about not making up data and not having these fallback paths
> that mask data incompleteness... we will have to assume sometimes
> that a timestamp was UTC and able to make it sortable. We should
> have that assumption happen in exactly one place in the whole code
> base."

**Goal**: a tiny crate (`frankweiler-time`?) that all extract + all
translate code uses. Public API around:

- `IsoOffsetTimestamp` newtype that *requires* explicit offset on
  construction (no bare `Z`, no naive).
- `parse_with_assumed_utc(s) -> Result<IsoOffsetTimestamp, _>` —
  **the single function in the whole repo where "assume UTC" is
  allowed to happen** — and only when the input genuinely lacks
  offset info from upstream.
- `microsecond_bump_off(parent, n)` — the canonical synthesized-stamp
  recipe for sub-items lacking their own time.
- A `validate_iso_offset(s) -> Result<...>` for translate-time checks.

**Today**: bare-Z and naive datetimes leak through in places (audit
called out beeper, signal, email).

**Action**: stand up the crate; require it in extract + translate;
forbid `chrono::DateTime::to_rfc3339` and similar shortcuts via a
clippy / module-private discipline. Add the "no fabricated timestamp"
rule to `data_architecture_ingestion.md`.

**Landed**: `frankweiler-time` (`frankweiler/backend/time/`) owns
`now_local`, `parse_strict`, `parse_with_assumed_utc`, `bump_micros`
and friends; every `chrono::{Utc,Local}::now().to_rfc3339*` callsite
in the workspace funnels through it (251981f). The "no fabricated
timestamps" principle lives in `data_architecture_ingestion.md`.
`GridRow.when_ts` is now `Option<String>` end-to-end (schema,
generated Rust, all 10 producers, load.rs, dolt_repo, SearchRow,
api.ts) — null when there's no source-side stamp, so the principle
holds at the type level. Contacts without `REV:` and any other
non-event-shaped rows now emit `null`, never a wallclock sentinel.
Two known fabricators remain and are documented as such: perseus's
`synth_when_ts` (immutable corpus, pending the corpus-vs-event
story) and the beeper / signal `iso_from_ms` fallback paths (now
`tracing::warn!` loudly per 251981f).

### P0.6 Shared retry config schema (extract-side impl)

> Thad: "Let's set up a shared retry config that everyone uses to
> configure their retry in the YAML. And there is a shared
> implementation that helps track failures and schedule exponential
> backoff, etc. This is probably a solved problem in some ways. I
> wonder if we can use a pre-baked solution."
>
> **And explicitly**: "I do not think retry logic belongs in the
> orchestrator, I think it is an extraction concern. It would be
> great if the shape of it is typically shared though."

**Goal**:

- A `RetryPolicy { max_attempts, initial_backoff, max_backoff,
  give_up_after, ... }` struct, serde-deserialized from a per-source
  block in `config.yaml`, with a global default.
- A shared `Backoff` helper (look at `backoff` crate or `tokio-retry`
  first) that providers call into.
- The `_bookkeeping` tables (P1) feed the "what should we retry"
  query.
- Each provider implements its own retry walk over its own tables at
  the start of its extract phase — using the shared policy +
  bookkeeping helpers. The orchestrator does not have a retry mode.

**Action**: prototype the `RetryPolicy` struct + YAML schema, pick the
external crate (or roll our own minimal), retrofit one provider
(notion?) as the reference.

### P0.7 Vestigial-pattern wave

Things that distort the bytes-at-rest picture and should go before
we standardize on it:

- **Stop building `<provider>_download` rust_binary targets.** (audit
  shared-layer item; Thad: "Let's just stop building these for now.
  It just creates extra compilation/linking we don't need.") Remove
  from BUILD.bazel; keep the per-provider `extract::fetch` library API
  since that's what `sync` calls.
- **Drop the pre-doltlite JSONL-tree fallback in `slack/translate`.**
  (audit slack item; Thad P0: "I think we should probably get rid of
  this at this point.") Test fixtures should also be doltlite-shaped.
- **Drop the `thread_root_uuid` backfill loop** in slack
  `RawDb::open` if all deployed stores have been migrated. (audit
  slack item; Thad P0: "get rid of this too.")
- **Manifest-TTL on slack `conversations.list` / `users.list` stays**,
  with a code comment explaining it's a deliberate perf optimization,
  not a violation. (Thad: "The reason we did this is because it's
  extremely slow to walk these lists. So this is a performance
  optimization and it should be explicitly allowed.")
- **Anthropic `--conv-uuid` single-conversation mode stays** (Thad:
  "We actually want this because it's useful for small tests."). Same
  for similar testing-only flags elsewhere.

---

## P1 — supporting structure work

These mostly orbit the P0 items above; some are pure quality-of-life
improvements that make the P0 work easier to land.

### P1.1 Shared bookkeeping DDL helper

Macro or generator that emits the standard `_bookkeeping` columns
(`attempt_count`, `last_attempt_at`, `last_error`, plus the new
`translated_fingerprint` from P0.3) so providers don't drift. (Thad
P1: "Standardizing this would ensure we don't have any drift.")

### P1.2 Per-provider config cleanup

> Thad: "I really would like to clean up how we express per data
> source configuration and code paths."

Audit the `SourceConfig` enum, the `ExtractControl` struct, and the
per-provider config branches in `sync/src/main.rs`. Aim for: one
provider == one place where its knobs are listed. This is the right
home to land the per-source retry policy from P0.6.

### P1.3 Stable inter-phase summary struct

> Thad P1: "may affect the schema of the summary bytes."

Replace the ad-hoc per-phase outcome structs (`FetchSummary`,
`PhaseOutcome`, `LoadOutcome`) with a unified `RunPhase { name,
status, error, stats }`. Drives the end-of-run JSON summary and the
status-line UI.

### P1.4 LLM-chat translate type

> Thad P1: "At translate time, I actually think we should extract a
> shared Rust data type to encapsulate everything we want to render
> about any kind of LLM chat (Claude, ChatGPT, Gemini, etc.) and then
> translate all of the chats into that object and then pass that to a
> render function that knows how to render it to markdown and turn it
> into rows for the index."

A `LlmChatTurn` / `LlmChatConversation` enum + struct, lifted to a
shared crate. Anthropic and ChatGPT each project their raw payload to
it; one renderer + one GridRow projector consume it.

### P1.5 Generic chat-message translate type

> Thad P2 (but upgraded by his own follow-up): "Beeper and signal
> need to group by timestamp or by time period. Actually, I think
> once we go to Slack direct messages, we will want something similar
> in Slack as well. So yes, I agree. I think we should probably try
> to introduce a generic intermediate chat message type that all
> chats can be turned in to (at translate time) and then render
> that."

Same shape as P1.4 but for human chat: Slack, Beeper, Signal, Email
all project their raw to a shared `ChatMessage` / `ChatThread` type;
one renderer; one GridRow projector. Pairs with the "period grouping"
question on `periodize.rs`.

### P1.6 Reuse `periodize.rs` for chat-thread grouping

> Thad P1: "I thought we'd probably also use this for beeper and
> signal when we want to group arbitrarily long message threads. If
> we don't, we should."

Wire it into the P1.5 chat-message renderer.

### P1.7 Contacts: don't ingest UUID-less contacts; inline photos

> Thad P1: "If it doesn't have a UUID, it doesn't really have an
> identity" — warn loudly, exclude. And: "make them inline images
> since they are relatively small" — render the contact photo as a
> base64 inline image in the markdown. Verify markdown viewers render
> it.

### P1.8 Email timestamp normalization

> Thad P0 (in context, but classed here since it depends on P0.5
> landing first).

Once the timestamp utility crate exists (P0.5), email's bare-Z /
naive paths funnel through it. The audit flagged this as the most
testable timestamp gap.

### P1.9 404-tracking even where retry doesn't apply

> Thad P1, Beeper-section: "We should at least be recording that we
> are getting 404s so that we don't try to fetch forever."

Even for providers without a full retry walk, every 404 on a
known-existed id should land a `deleted_upstream_at` marker.

### P1.10 Code-review family unification (GitHub + GitLab)

Push the shared shape to translate-time: one `CodeReviewThread`
intermediate type, one renderer, one GridRow projector. Keep the
raw stores per-provider.

### P1.11 Warnings make it to the JSON summary

> Thad P1: "Let's do log warnings about this and make sure that
> warnings make it all the way through to the synchronization log
> that we write at the end."

A counter+sample buffer collected in each provider's run, surfaced in
the per-run summary alongside `errors=N`.

### P1.12 Investigate signal cursor by file-hash

> Thad P2 (but worth doing early since it changes ingestion shape):
> "The way I think cursors should work with Signal is that we should
> Blake3 hash the entire file we are ingesting. And if we have
> already ingested it, then just completely skip all of it. And if
> we haven't, then let's run the ingestion."

Same pattern likely applies to any backup-file provider added later.

---

## P2 / later (brief)

- Per-source narrative logging helper (low effort, defer).
- Render-side improvements (markdown layout, attachment rendering,
  source URLs). Thad consistently P2'd these — bytes-at-rest first.
- Privacy boundary for OTLP spans.
- Drift detection.
- Tombstone marking semantics beyond `deleted_upstream_at`.
- `dolt_commit` message template library.
- Auto-pause-on-auth-failure (P2 from email section).
- Object-lifecycle builder API.

---

## Open questions before P0 work starts

These are Thad's `CLARIFY:` / `TO CLARIFY:` markers in the audit.
Worth resolving before we touch the code they refer to:

1. **`ObsArgs`** — Thad asks twice "what is this?" and "I don't like
   the name." It's the shared clap-flattened observability arg block
   (`obs::ObsArgs` in `backend/obs/src/lib.rs`) — logging level,
   OTLP endpoint, NDJSON-vs-pretty toggle, progress-bar config. We
   should rename it (`SharedObsFlags`? `MonitoringArgs`?) and document
   it before pushing for a "every binary must include it" rule.
2. **Email comment "ingesting from a flawed data export?"** — needs
   re-reading in context; resolve when we get to the email schema.
3. **"Where would the shared blob_refs table go?"** — Thad's answer
   is: it doesn't. Per-provider blob_refs, shared CAS via blake3 is
   enough. Make that explicit in `data_architecture_ingestion.md`.
4. **Synthesize-for-test-fixtures vs synthesize-for-runtime** —
   resolve in context as part of P1.5.
5. **Beeper fetch-pattern intent** — "is it to only fetch a subset
   as described in the config?" — answer needed before touching
   beeper extract.

---

## Explicit "do not do" list

- **No retry loop in the orchestrator.** Retry is an extraction
  concern. Only the policy struct + bookkeeping helpers + Backoff
  utility are shared. (Thad: "DO NOT DO THIS.")
- **No raw-store schema unification across providers.** Each provider
  keeps its own raw tables; unification happens at translate, into
  GridRow / shared intermediate types only.
- **No central UUID registry.** Recipes live in the schema file
  they key into (`extract/schema_raw.rs` for raw-store PKs;
  `translate/...` for GridRow grid-uuids) — not a shared
  `uuid_recipes` crate, not a separate `providers/<name>/src/uuid.rs`
  file. (Thad: "Getting stable identifiers right is incredibly
  important ... but I'm not sure centralizing it is the right idea.")
  See P0.4.
- **No `endpoint_shapes` revival.** It's gone; the audit confirmed
  no stragglers.
- **No fabricated timestamps as silent fallback.** Null is the
  correct value when upstream gives us nothing; the one allowed
  UTC-assumption path is gated through the P0.5 helper.
- **No removal of testing flags.** `--conv-uuid` and similar stay —
  they're for small-test ergonomics.

---

## Proposed sequencing

1. **Week 1**: P0.1 (schema-source-of-truth shape + first 2 providers
   as the reference), P0.7 (vestigial cleanup), open-question
   resolution. **(P0.4 lands incidentally during this — recipes get
   lifted into `schema_raw.rs` as part of each provider's P0.1
   conversion.)**
2. **Week 2**: P0.5 (timestamp crate), P0.1 propagation to the rest
   of the providers.
3. **Week 3**: P0.2 (sidecar struct), P0.3 (translated_fingerprint
   column), P1.1 (bookkeeping DDL helper).
4. **Week 4**: P0.6 (retry config + reference impl in one provider).
5. **After**: P1 items in roughly the order listed.

Adjust as we get into the work and discover dependencies.
