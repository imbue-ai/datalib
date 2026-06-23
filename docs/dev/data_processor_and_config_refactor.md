# Program A: provider-owned config + a `DataProcessor` trait + a registry

> **This is the do-this-first refactor.** It is bounded, it satisfies the three
> concrete goals below, and it is finishable in one committed push. It does
> **not** turn the pipeline into a DAG — that is Program B, kept separately and
> aspirationally in [`pipeline_dag_runner.md`](./pipeline_dag_runner.md). The DAG
> vision doc (`Prompt to Claude for conversion to DAG runner.md`) remains the
> north star; Program A is the foundation it will eventually stand on.

## 0. Ground truth (read first)

Verified by reading the code, June 2026 — because the existing artifacts disagree
about what exists:

- `datalib` has **no** provider-dispatch trait at all (no `DataProcessor`, no
  `Source`), **no** `build_source`, **no** per-provider config crates.
  `frankweiler-sync` still dispatches every provider through a
  16-arm `ExtractKind` enum + a parallel `DbHandle` enum + a hand-maintained
  `(ExtractKind, DbHandle)` `match` in `sync/src/main.rs`, ending in
  `unreachable!("variant mismatch")`.
- Issue [#23](https://github.com/imbue-ai/datalib/issues/23) opens by asserting a
  `Source`-trait refactor is *"landed and green."* **It is not.** That refactor
  exists only as a single **parked WIP commit** (`3b931ea`,
  *"exploratory Source-trait orchestrator refactor (WIP, parked)"*) on the
  unmerged branch `data_source_interface` in the predecessor repo `mixed_up_files`
  — not on any `main`, and not in `datalib`.
- That branch did the **trait** half (a `Source` trait + `build_source`, with
  `ExtractKind`/`DbHandle` deleted) but **not** the config half: config stayed in
  `core`. So issue #23 (config out of `core`) is the *unstarted follow-up* to a
  *parked* branch. **Nobody has shipped either half anywhere.**

The most important lesson from that history: **this refactor has already been
started once and abandoned half-done.** Program A is scoped and sequenced
specifically so that does not happen again (§6).

## 1. The three goals

1. **One config definition per source, adjacent to the source.** A source's
   schema is declared once — in (or beside) its provider crate, next to the code
   that consumes it. Not in a central enum below the providers; not re-parsed a
   second time downstream. Today email's config is effectively defined **three**
   times (`core::config::SourceConfig::Email` → the `ExtractKind` mirror in
   `main.rs` → the render-phase `EmailStanza` re-parse).
2. **A registry of sources.** The orchestrator does not enumerate providers or
   destructure their internals. It looks each source up by its `type:` tag and
   drives it through one uniform trait.
3. **Strict dependency direction.** *The orchestrator may depend on the things it
   runs; the things it runs may never depend on the orchestrator.* This is the
   non-negotiable rule. It is the **root cause** of the current pain: because the
   config schema (`core::config`) sits *below* the providers, a provider cannot
   name its own config type, so config is ferried by hand and duplicated (the
   `GoogleTakeoutSync`-in-core vs `SyncFlags`-in-provider wart #23 calls out).

Program A delivers all three, in full, for every provider.

## 1a. Naming: the trait is `DataProcessor`

The general trait every pipeline unit implements is **`DataProcessor`** — a
config-driven, monitorable unit of work the orchestrator runs, with a single
method: `run(ctx) -> summary`. We deliberately do **not** build an OOP trait
hierarchy.

**Extract and translate are *separate* `DataProcessor`s, not two methods on one
object.** A configured *data source* contributes one or more processors — email
contributes an extract processor *and* a translate processor; an API-only source
like yolink contributes *just* an extract processor; a Claude export already on
disk contributes *just* a translate processor. "Extract-only" / "translate-only"
is then **structural** (a missing processor), not a no-op default method or an
`is_managed` flag. This is the shape you actually want, and it is also the shape
that survives into Program B unchanged.

A *data source* (ingests from the external world) and a *data transform* (reads
artifacts, writes artifacts) are **roles a `DataProcessor` plays, not separate
types.** `DataSource` / `DataTransform` sub-traits get introduced *only* if some
concern turns out to be genuinely source-only (credentials, say) or
transform-only; the default — and very possibly the permanent state — is that
**`DataProcessor` is the only trait.**

Because the trait is single-method `run()`, **its shape is stable from A into B**
— B adds a scheduler *around* it; the trait is never rewritten. The one
transitional element A keeps is coarse: a provider groups its processors into the
two existing waves (`extract`, `translate`) so today's global extract-all →
translate-all → load-once structure is preserved *without* a real scheduler. **B
replaces that coarse grouping with order derived from each processor's declared
inputs/outputs.** So in A the source-vs-transform line is "which wave"; the
"declared artifact inputs" version of that line arrives with the scheduler in B.

Concrete impls keep descriptive struct names (e.g. `EmailExtract`, `EmailRender`)
and implement `DataProcessor`.

## 2. Scope boundary — what A is and is NOT

Keeping this boundary crisp is the entire reason for splitting A from B.

**Program A IS:**

- A `DataProcessor` trait (single `run()`) in a base crate; every provider's
  processors implement it. Extract and translate are *separate* processors.
- Each provider builds its processors grouped into the two existing waves
  (`extract`, `translate`); extract-only / translate-only sources are structural.
- A single `type:` → provider registry; `ExtractKind`/`DbHandle`/the big `match`
  and `unreachable!()` deleted.
- Each provider **owns its config struct**, deserialized straight from its YAML
  stanza; `core/config.rs` deleted.
- A schema-only config crate so `http` links zero extraction code.

**Program A is NOT (these are Program B):**

- Not a DAG. A **keeps the two global waves (extract → translate) + load-once**
  and runs each wave's processors as a fixed group; it does **not** derive order
  from declared inputs/outputs, and does not touch Load.
- Not per-node commit. A **keeps the orchestrator owning the raw pool** + the
  SIGINT/post-extract `dolt_commit` (commits are already per-source today).
- Not output-versions / a new change-detection contract. A leaves change
  detection exactly where it is.
- Not un-fusing Load from the translate callback.
- Not progress-NDJSON, storage-by-source relayout, or subprocess execution.

If a proposed change in A touches anything in the second list, it has scope-crept
into B. Stop.

## 3. What we keep / fix from the parked branch

The `data_source_interface` branch is a clean **dispatch** refactor and a good
template for A. Two adjustments:

**Keep as-is:**

- The trait lives in `frankweiler_etl` (the base crate every provider already
  depends on). Dependency direction correct.
- The orchestrator opens each raw-store-writing processor's pool and owns the
  post-run + SIGINT commit (today's behavior, made uniform). Deliberately *not*
  per-node commit.

**Fix (the branch did these wrong, or not at all):**

- **One object with two phase methods → separate `run()` processors.** The branch
  kept a single `Source` carrying `extract` *and* `translate` (with `is_managed`
  and no-op defaults for absent phases). A splits these into separate
  `DataProcessor`s (§1a): extract-only / translate-only become structural, the
  async/sync `extract`/`translate` asymmetry disappears, and the trait shape is
  stable into B. `run` takes `&self`, not `&mut self` — the orchestrator owns the
  pool via `RunCtx`, so the branch's `open()`-then-stash-in-`self` temporal
  coupling is gone too.
- **Config ownership** — the branch still took config pre-parsed from `core`
  (`EmailSource::new(name, raw_dir, mode, …)`) and even *added* a fourth copy
  (`EmailMode`). Program A's central change is to make the provider own and parse
  its own config (§4.2). This is goal #1 and the branch does nothing for it.
- **De-leak capabilities** — `synthesizer()` (test fixtures) and
  `attach_event_tape()` (slack-only) were methods on the *universal* trait. Move
  them to optional capability sub-traits (`HasSynthesizer`, `HasEventTape`) that
  attach to the *one* processor that needs them, so the core `DataProcessor`
  trait stays about the thing every processor does.

A introduces the separate-processor *unit* but **not** the DAG *scheduler* — the
order-from-declared-I/O machinery is precisely the supersession that lives in
Program B.

## 4. Target design

### 4.1 The `DataProcessor` trait + `SourcePlan` (base crate)

One single-method trait for the run unit, plus a small `SourcePlan` a provider
builds to group its processors into the two waves A keeps.

```rust
// frankweiler_etl::processor

/// One config-driven, monitorable unit of work. Single method. A configured
/// source contributes one or more of these (an extract processor, a translate
/// processor, or just one of them).
#[async_trait]
pub trait DataProcessor: Send + Sync {
    fn id(&self) -> &str;                       // "email/fastmail/extract" — for logs + progress

    /// If this processor writes a raw doltlite store, its path. The orchestrator
    /// opens the pool, passes it in via `RunCtx`, and owns the post-run +
    /// SIGINT commit (today's behavior, made uniform). None = writes only files.
    fn raw_store_path(&self) -> Option<PathBuf> { None }

    /// Do the work. `&self` + pool-via-`RunCtx` ⇒ no open()-then-stash coupling.
    /// Returns a short human summary for the run log (a *structured* outcome with
    /// a content-version is a Program-B concern; A keeps the string).
    async fn run(&self, ctx: &RunCtx<'_>) -> Result<String>;
}

/// What the registry produces per configured source: its processors grouped into
/// the two waves A keeps. B replaces this grouping with edges derived from each
/// processor's declared inputs/outputs.
pub struct SourcePlan {
    pub extract:   Vec<Box<dyn DataProcessor>>,   // run in the extract wave
    pub translate: Vec<Box<dyn DataProcessor>>,   // run in the translate wave
}

// Optional capabilities — NOT on the universal trait; attach to the one
// processor that needs them:
pub trait HasEventTape   { fn attach_event_tape(&mut self, tape: Arc<EventTape>); }  // slack extract
pub trait HasSynthesizer { fn synthesizer(&self) -> Box<dyn Synthesizer>; }          // fixtures
```

The orchestrator keeps today's two global waves + load-once: for every source's
`extract` processors (open store → attach tape if `HasEventTape` → `run` → commit
+ register for SIGINT), then every source's `translate` processors, then Load.
The `(ExtractKind, DbHandle)` match pair and `unreachable!()` are gone; a
`SourcePlan` per configured source replaces them. Extract-only sources have an
empty `translate` vec; translate-only sources have an empty `extract` vec — no
flag, no no-op method.

### 4.2 Provider-owned config + the registry (the heart of A)

```rust
// in the provider crate (or a tiny `*-config` crate beside it)
#[derive(Deserialize)]              // deny_unknown_fields; provider owns this
pub struct EmailConfig {
    #[serde(flatten)] pub common: SourceCommon,   // name/enabled/input_path/...
    pub sync: Option<JmapSync>,
    pub mbox: Option<MboxAccount>,
    pub outlink_format: Option<EmailOutlink>,
    pub only_extract_labels: Vec<String>,
    pub only_render_labels:  Vec<String>,
}
impl EmailConfig { pub fn validate(&self) -> Result<()> { /* provider-local rules */ } }
```

The orchestrator's only knowledge of a provider is: *its `type:` tag, how to
deserialize+validate its config, and how to build its `SourcePlan` (the grouped
processors) from it.* One place maps `type:` to provider. Two equivalent shapes:

- **Typed `oneof` (recommended; issue #23's choice).** An `ingest-config` crate
  holds the envelope + `enum SourceConfig { Email(email_config::EmailConfig),
  Slack(slack_config::SlackConfig), … }` (`#[serde(tag="type")]`). Keeps
  compile-time exhaustiveness and whole-file validation; `http` depends on this
  crate alone (schema-only). `build_source` matches the oneof and calls the
  provider's `plan(cfg) -> SourcePlan` — a trivial arm per provider, no
  field-ferrying.
- **Runtime registry.** Providers self-register (`type_tag` → deserialize+build
  fn) via `register_all()` or `inventory`. No central enum; truly
  "orchestrator enumerates nothing." Loses compile-time exhaustiveness.

**Recommendation: typed `oneof`.** It delivers all three goals — provider owns
its `Config`, orchestrator drives only through the trait, dependency direction
enforced structurally — while keeping the single validated config file and
compile-time completeness. The "registry feel" you want is already delivered by
*provider-owns-config + look-up-by-tag + drive-via-trait*; whether the lookup is
a `match` over a oneof or a `HashMap` is a 10-line detail we can change later
**without touching any provider.** Don't pay for `inventory`'s magic on a
single-binary tool yet.

### 4.3 Crate / dependency architecture

```
                         frankweiler_etl   (base: DataProcessor trait, SourceCommon, pools, ...)
                          ▲          ▲
   email-config ──────────┘          └────────── frankweiler_etl_email (impl DataProcessor; deps its email-config)
   slack-config  (serde only)                    frankweiler_etl_slack  ...
        ▲                                              ▲
   ingest-config  (Config envelope + SourceConfig oneof + load_config + validate)
        ├───────────────────────────► http   (schema only — links no extraction)
        └───────────────────────────► sync   (orchestrator: ingest-config + every provider impl)

   core/config.rs  ─────────►  DELETED
```

Every edge points **down**. `sync` → provider impls → `frankweiler_etl`.
Providers → their own light `*-config`. `http` → `ingest-config` only. This makes
the dependency rule *structural*, not merely intended. (#23's finding: under
`rules_rust`, first-party crates that only use already-present third-party deps —
true of every `*-config` — need just a `BUILD.bazel`, no `Cargo.toml`. So the
crate proliferation is cheap; decide cargo-workspace-green vs bazel-only in §7.)

> Single shared `ingest-config` holding *all* provider config structs (instead of
> ~14 `*-config` crates) is a valid cheaper middle ground: ~2 new crates, kills
> the duplication, gives `http` a schema-only dep, loses only "config struct
> physically beside the impl." Pick per §7.

## 5. Validate the pattern before writing code

Cheap insurance, on paper, before step 1. The parked branch showed all 16
providers fit *a* single dispatch trait, but A's `run()` + `SourcePlan` shape and
the config-ownership pattern are new — pressure-test **both** against the awkward
shapes:

- **`google_takeout`** — the duplication canary (`SyncFlags` vs core's
  `GoogleTakeoutSync`). Confirm provider-owned config + flatten removes the copy,
  and that its several feeds map to several processors in the plan.
- **`email`, `contacts`/`carddav`** — two input modes (API vs file) under one
  `type:`. Confirm one owned `Config` with optional `sync:`/`mbox:` sub-stanzas
  parses the existing YAML unchanged, and one builder produces the right plan for
  each mode.
- **`yolink`** — extract-only. Confirm its `SourcePlan` has an empty `translate`
  vec and that reads clean (it does — no no-op method, no `is_managed` flag).
- **`claude_export`** — translate-only. Confirm an empty `extract` vec and that
  the orchestrator runs a source with no extract processors fine.
- **`notion`/`yolink` validation rules** currently in `core::validate()` — confirm
  they move cleanly onto each `*-config`'s `validate()` so `http` keeps full
  validation with a schema-only dep.

If all five fit on paper, the `run()`/`SourcePlan`/config shape will generalize.
If one bends it (as `synthesizer`/`event_tape` bent the branch's trait), fix the
shape now.

## 6. Migration — and the definition of done

Each step compiles and keeps `bazel test //...` green. **Definition of done for
Program A = every provider migrated, `ExtractKind`/`DbHandle` and
`core/config.rs` deleted.** The email pilot is the *proof*, not the finish line —
the lesson of the parked branch is that stopping after the pilot leaves the
codebase with two dispatch systems, worse than either end state.

1. **`DataProcessor` trait + `RunCtx` + `SourcePlan` + capability traits** in
   `frankweiler_etl`. Types only.
2. **Email pilot (proof).** Add `email-config`; implement `EmailExtract` and
   `EmailRender` (two `DataProcessor`s) + an email builder returning the
   `SourcePlan`; route **only** email through the new two-wave runner; leave the
   other 12 on `ExtractKind`. Demonstrates all three goals end-to-end, behavior
   identical, golden snapshots unchanged.
3. **`ingest-config` crate** with the `SourceConfig` oneof + `load_config` +
   validation; repoint `http` to it (schema-only).
4. **Migrate the remaining providers**, one per PR, deleting each
   `ExtractKind`/`DbHandle` arm as it lands. The parked branch's per-provider
   `source.rs` files are a useful reference, adapted.
5. **Delete** the now-empty `ExtractKind`/`DbHandle`/`open_extract_db`/the big
   match/`unreachable!()`, and `core/config.rs` (and its `GoogleTakeoutSync`
   duplication).
6. **Done.** One trait, one registry, config owned per provider, dependency rule
   structural. Reassess whether/when to begin Program B.

Sequence steps 2→5 as a single committed effort. If priorities force a pause,
pause **after** step 2 (email-as-island is tolerable briefly) or **after** step 5
(complete) — never indefinitely in the middle.

## 7. Open questions (decide before the step that needs them)

- **Config dispatch: typed `oneof` vs runtime registry?** Recommend oneof
  (§4.2). *Needed at step 2/3.*
- **Per-provider `*-config` crates vs one shared `ingest-config`?** Finest deps +
  config-beside-impl, vs ~2 crates. Recommend starting with the shared crate and
  splitting later if a provider's config grows. *Needed at step 3.*
- **Cargo workspace green vs Bazel-only for the new config crates?** #23 notes
  Bazel-only is essentially free for these. Decide once, apply consistently.
  *Needed at step 1/3.*
- **`http` validation depth** — move `core::validate()`'s per-provider rules onto
  each `*-config`'s `validate()`? Recommend yes (keeps `http` fully validating on
  a schema-only dep). *Needed at step 3.*

## 8. Why this one feels good to do

Deleting the 16-arm `DbHandle`, the `unreachable!()`, and the triple-defined
email config is the satisfying, structural kind of cleanup — and it *compounds*:
once a provider owns its config, adding the next source is "write a config
struct, implement `run()` for its processor(s), return a `SourcePlan`, add one
registration line," touching no central orchestrator code. It is bounded, reversible per step, green
throughout, and it fixes the actual root cause (dependency inversion) rather than
papering over it. The one discipline it demands is finishing the migration in one
push — which the parked-branch history tells us is exactly the trap to avoid.
