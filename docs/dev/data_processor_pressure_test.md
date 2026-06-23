# Program A — §5 pressure-test (on paper, before code)

Verified against the actual code, June 2026. This is the §5 step of
[`data_processor_and_config_refactor.md`](./data_processor_and_config_refactor.md):
pressure-test the `DataProcessor` / `SourcePlan` / provider-owned-config shape
against the awkward providers **before** writing the trait. Verdict up front:
**the shape holds for all five**, with two refinements to the doc's sketch that
need to be baked into Step 1, plus one genuinely new finding that changes how we
sequence the work.

## The headline finding the plan under-weights

**The translate (render) half is already a registry — and it took a *different*
config path than §4.2 recommends.**

`sync/src/render_and_index_md.rs` is already exactly the registry Program A wants,
*for translate*: `renderer_for(type_str, stanza) -> Box<dyn RenderAndIndexMd>`,
one `RenderAndIndexMd::run(ctx, on_doc)` impl per provider, selected by the `type:`
string. There is **no** `ExtractKind`-style enum on the translate side and no big
match — it's been a clean trait-dispatch registry all along.

But it owns config a **third** way: each renderer parses an **opaque
`serde_yaml::Value` stanza** (`Email::from_stanza`, `Beeper::from_stanza`, …),
deliberately so the registry depends on *nothing* from `frankweiler_core::config`.
That is the "config lives with the step, orchestrator forwards an opaque subtree"
direction from issue #23's thread — the opposite of §4.2's "typed `oneof`,
provider owns a typed `Config`."

So Program A is really reconciling **three** config definitions into one (the doc
says this for email specifically; it's true structurally for every provider):

| # | Where | Form | Example (email) |
|---|-------|------|-----------------|
| 1 | `core::config::SourceConfig` | typed enum variant below the providers | `Email { sync, mbox, outlink_format }` + `EmailSync`/`MboxSync`/`EmailOutlink` |
| 2 | `sync/main.rs` `ExtractKind` | typed mirror, hand-mapped in `for_source` | `ExtractKind::Jmap` / `EmailMbox` |
| 3 | `render_and_index_md.rs` | **opaque** `from_stanza` re-parse | `EmailStanza { outlink_format }` + `OutlinkFlavor` |

The email triple-definition the doc calls out is **confirmed verbatim** in the
code (config.rs:702 / main.rs:1575-1591,1737 / render_and_index_md.rs:495-520).

**Consequence for the design:** the typed-`oneof` decision (§4.2) is the right one
*and* it lets us delete definition #3 (the opaque `from_stanza` re-parse), not just
#2. The builder deserializes the provider's typed `Config` once and constructs
**both** its extract and translate processors with that typed config in hand — so
`Email::from_stanza` becomes `EmailConfig` field access. That's strictly better
than today and fully satisfies goal #1. The only thing we give up is the render
registry's current "zero dependency on the config crate" property — which is
exactly what §4.2 already decided to trade away.

## Refinement 1 — `RunCtx` is heterogeneous, not just "the pool"

The doc's trait sketch says `RunCtx` carries "the pool." The code shows extract
and translate processors need **disjoint** context:

- **Extract** processors (write the raw store) need: the pre-opened **pool**
  (`db: Some(db)` in every `extract::fetch` call), `out_dir`, `progress`,
  `control` (`ExtractControl`), the ambient `metrics`/`diagnostics`/`retry` scopes,
  and `now`. They return a provider `Stats` that `main.rs` formats into the summary
  string.
- **Translate** processors (read the raw store, write markdown) need: `root`,
  `raw_path`, `name`, `progress`, `prior_fingerprints`, and the **`on_doc`
  callback** (`&mut OnDoc`) — Program A keeps Load fused into this callback. They
  open the raw db **read-only themselves** (`db_path_for(raw_path)`), so they need
  no pool and return no pool.

So `RunCtx` is the **union** of today's `ExtractPlan` fields and `RenderCtx`
fields, and `run()` must be able to reach the `on_doc` sink. Concretely, either:

- `RunCtx` holds `Option<&Pool>` + `Option<&mut OnDoc>` + the common fields, or
- (cleaner) `RunCtx` holds the common fields and an enum/role payload, with extract
  processors reading `ctx.pool()` and translate processors reading `ctx.on_doc()`.

This does **not** bend the single-`run()`-method trait — it just means `RunCtx` is
richer than the one-liner. Bake the real `RunCtx` shape into Step 1. `raw_store_path()
-> Option<PathBuf>` stays the signal for "I write a pool, orchestrator opens+commits
it" (extract → `Some`, translate → `None`), matching today's `open_extract_db`
returning `Ok(None)` for poolless kinds (Perseus).

## Refinement 2 — `&self` + summary string both already true

`extract::fetch(...) -> Result<ProviderStats>` is already a free function taking an
owned `FetchOptions`; `RenderAndIndexMd::run(&self, ...)` is already `&self`. The
doc's "`run` takes `&self`, pool via `RunCtx`" and "A keeps the string summary" are
both already how the code is shaped — no temporal `open()`-then-stash coupling to
undo on the translate side, and only mild coupling on extract (the pool is stashed
in `ExtractPlan.db` by the pre-open loop). The new trait *formalizes* what's there.

---

## The five providers

### `google_takeout` — duplication canary ✓ (with a scope note)

- **Duplication removed.** `core::config::GoogleTakeoutSync` (config.rs:636-650,
  9 feed bools) is a hand-maintained mirror of
  `frankweiler_etl_google_takeout::extract::SyncFlags`, copied field-by-field in
  `for_source` (main.rs:1687-1697). Provider-owned config kills this: the provider
  owns a `GoogleTakeoutConfig` whose `sync:` block *is* `SyncFlags` (or trivially
  `into()`s to it), and `core::config` stops naming it. Goal #1, canary cleared.
- **Scope note on "several feeds → several processors."** §5 says "its several
  feeds map to several processors in the plan." The **current** code has **one**
  `extract::fetch` (SyncFlags selects feeds *internally*) and **one** `render` (only
  the Google Chat feed; main.rs render_and_index_md.rs:433-448). So in Program A,
  google_takeout is naturally **one extract processor + one translate processor**.
  Splitting feeds into separate processors is a behavior-neutral decomposition with
  no payoff until the scheduler exists — that is **Program B**. Keep it one-and-one
  in A. (Flagging because the doc's wording could invite scope-creep here.)

### `email` + `carddav` — two input modes under one `type:` ✓

Confirmed against `all_sources.yaml` (email at L144/L159, carddav at L174/L183):

- Email mode A (`sync:` present) → `extract::fetch` (JMAP); mode B (`sync:` absent,
  `input_path` = `.mbox`) → `extract::mbox::fetch`. One owned
  `EmailConfig { #[serde(flatten)] common, sync: Option<EmailSync>, mbox:
  Option<MboxSync>, outlink_format: Option<EmailOutlink> }` parses **both** YAML
  shapes unchanged (same serde shape as today's `SourceConfig::Email`). The email
  **builder** picks which extract processor to emit by `sync.is_some()`; translate
  is always one `EmailRender` reading `outlink_format`. `SourcePlan.extract` has
  exactly one element either way.
- Carddav is identical: `sync:` → `extract::fetch` (server), absent+`input_path` →
  `extract::vcf_dir::fetch` (file walker). One `CarddavConfig`, builder picks the
  extract processor, one `Carddav` renderer. The "two `ExtractKind` variants for one
  type" wart (`Carddav` / `CarddavFile`, `Jmap` / `EmailMbox`) collapses into "one
  builder, two branches" — a clear win.

**Implication:** "one provider may contribute one of *several possible* extract
processors, chosen by config." The builder-returns-`SourcePlan` shape handles this
natively; nothing bends.

### `yolink` — extract-only ✓

`SourcePlan { extract: vec![YolinkExtract], translate: vec![] }`. Today's translate
side dispatches yolink to a `Skip` placeholder (render_and_index_md.rs:86-89); in
the new model the empty `translate` vec **is** that, structurally — no `Skip`
struct, no `is_managed` flag, no no-op. Reads clean exactly as the doc predicts.

### `claude_export` — translate-only ✓ (with a shared-renderer note)

`type: claude_export` is `is_managed() == false` → no extract today (returns `None`
from `for_source`, config.rs:869, main.rs:1736). It renders through the **same**
`Anthropic` renderer as `claude_api` (render_and_index_md.rs:71). So:

- `claude_export` → `SourcePlan { extract: vec![], translate: vec![AnthropicRender] }`
- `claude_api`    → `SourcePlan { extract: vec![AnthropicExtract], translate: vec![AnthropicRender] }`

Two `type:` tags, **one** provider crate, **shared** translate processor. The
registry maps each tag to its own builder; both builders live in (or beside) the
anthropic crate. Empty `extract` vec runs fine — the two-wave runner just contributes
nothing to the extract wave for this source. Confirmed clean.

### `notion` / `yolink` validation rules → each `*-config`'s `validate()` ✓

`core::config::Config::validate()` (config.rs:1085-1173) holds:

- **Cross-source** invariants: non-empty names, **duplicate-name** detection. These
  are envelope-level → they stay on the `ingest-config` envelope, not on a provider.
- **Per-source** rules that move cleanly onto a provider `validate()`:
  - Notion: "must enable inbox or list ≥1 subtree page" → `NotionConfig::validate()`.
  - Yolink: ≥1 device, unique device names, `kind ∈ {temperature_humidity,
    watermeter}`, `start` is `YYYY-MM-DD`, `family_device_id`/`device_udid` are
    32-lowercase-hex → `YolinkConfig::validate()`. (Note: the error text says
    `'thsensor'` but the code accepts `temperature_humidity` — config.rs:1056 vs
    1124; carry the *code's* set, fix the message in passing.)

The `ConfigError` enum's per-provider variants (`NotionSyncEmpty`, `YolinkNoDevices`,
`YolinkBad*`) move with their rules; the envelope keeps `DuplicateSourceNames` /
`EmptySourceName`. `http` keeps **full** validation by depending on `ingest-config`,
which calls each provider config's `validate()` — schema-only, exactly goal #3.

---

## Capabilities to de-leak from the universal trait (confirmed real)

The doc says move `synthesizer()` and `attach_event_tape()` off the universal
trait. Both are real and both are minority concerns:

- **`attach_event_tape`** — today a method on `DbHandle` that is a **no-op for every
  provider except slack** (main.rs:1454-1461). → `HasEventTape`, attached to the one
  extract processor (slack) that consumes it. The orchestrator's open loop
  (main.rs:1198-1205) attaches the tape only when the processor advertises the
  capability.
- **`synthesizer()`** — playback-fixture generation, dispatched in its own match in
  `run_synthesize` (main.rs:2568-2675), present for ~7 providers and explicitly
  "no synthesizer yet" for carddav/email/signal/yolink/whatsapp. → `HasSynthesizer`,
  attached to the one processor that has it. It is **not** part of the run path at
  all (separate `--synthesize-playback-root` mode), so keeping it off `DataProcessor`
  is clearly correct.

Neither bends the core trait; both are textbook capability sub-traits.

---

## Net verdict

All five awkward shapes fit the `run()` / `SourcePlan` / provider-owned-config
model. Two refinements (richer `RunCtx`; one-and-one processors for
google_takeout, not per-feed) and one structural finding (translate is already a
registry; the typed-`oneof` decision also deletes the opaque `from_stanza`
re-parse) get folded into Step 1's trait definition. No shape needs redesign — proceed.

---

## Decisions taken & pilot outcome (June 2026)

§7 / design calls made with Thad before/during implementation:

- **Config dispatch:** typed `oneof` (deletes the `ExtractKind` mirror *and* the
  opaque `from_stanza` re-parse).
- **Config crates:** **per-provider** `*-config` crates, **Bazel-only** (no
  `Cargo.toml`; the Cargo workspace is no longer kept green — `bazel test //...`
  is the build of record).
- **Orchestrator is storage-agnostic (overrides the plan's §4.1 sketch).** It must
  not open pools, apply DDL, run `dolt_commit`, or know a store is doltlite. This
  killed `raw_store_path()`/pool/commit on the orchestrator side. Interrupt-safety
  crosses the boundary through an **opaque `Checkpoint`** trait the source
  registers; the source self-commits at end of `run()`. So `RunCtx` carries
  `register_checkpoint()` + `emit_doc()` (translate Load stays fused), not a pool.
- **Reporting:** *source owns its report*, via a shared doltlite utility. The
  **email pilot took the minimal cut** (issue
  [#37](https://github.com/imbue-ai/datalib/issues/37)): the source owns
  open/DDL/commit/`Checkpoint`, but the per-source "what changed" report stays
  orchestrator-assembled for now — the one tracked transitional storage leak,
  to be removed when the report machinery is reworked once for all providers.

### What landed in the pilot (Steps 1–2, `bazel test //...` green: 70/70)

- **Step 1 (base crate `frankweiler_etl`):** `processor.rs` — `DataProcessor`
  (single `async run() -> String`), `SourcePlan{extract,translate}`, opaque
  `Checkpoint`, `RunCtx` (`for_extract`/`for_translate`, `register_checkpoint`,
  `emit_doc`), `CheckpointSink`, `HasSynthesizer`. `raw_store.rs` — `PoolCheckpoint`
  (the reusable interrupt-commit hook). `async-trait` added as a Bazel
  **`proc_macro_deps`** entry.
- **Step 2 (email):** new Bazel-only `email_config` crate (`EmailConfig`);
  `frankweiler_etl_email::processor` — `EmailExtract` (JMAP + mbox, owns its
  store/commit/checkpoint), `EmailRender`, and `plan() -> SourcePlan`. The
  orchestrator routes **only** email through the two-wave processor path
  (`email_processor_plan` in extract, `render_email_translate` in translate);
  the other 12 stay on `ExtractKind`/`renderer_for`. The email-specific
  `ExtractKind::Jmap`/`EmailMbox`, `DbHandle::Jmap`, their `open_extract_db` arm
  and `run_inner` arms, and the orchestrator's `is_mbox_input` are **deleted**.

Gotcha worth recording: translate processors are driven with
`futures::executor::block_on`, **not** tokio's — the fused-Load callback does its
own `tokio::block_on(apply_one)` on the `spawn_blocking` thread, and nesting two
tokio runtimes there panics ("cannot start a runtime from within a runtime").

### Remaining (Steps 3–5) — do NOT park half-done

The 12 other providers + the `ingest-config` oneof + `http` repoint +
`ExtractKind`/`DbHandle`/`core::config` deletion. The email pilot is the template:
each provider gets a Bazel-only `*-config` crate, a `processor` module
(`*Extract`/`*Render` + `plan()`), and its `ExtractKind`/`DbHandle` arms deleted as
it lands. Per-provider validation rules (notion/yolink in `core::validate()`) move
onto each `*-config`'s `validate()`; the cross-source dup-name check stays at the
envelope.
