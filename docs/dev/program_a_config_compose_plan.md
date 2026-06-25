# Program A — config: compose, don't flatten; normalize once, then pure plumbing

Tracking: GitHub issue **#41**. Branch: `program-a-config-compose-41`.

This is the last wart from the Program A config migration (PR #40 relocated
`core/config.rs` verbatim, carrying duplicate inline `*Sync` structs). This doc
is the agreed design + step-by-step plan. It supersedes the double-`flatten`
sketch in issue #41.

---

## Guiding principle

> Config is a **plain data tree** with almost zero mechanism. If a piece of code
> needs config to run, it receives that config as an **object**. Recursively: as
> we descend into deeper code, we descend into deeper subtrees, breaking the tree
> apart and handing each piece to the code that needs it.

The whole system has exactly **one** piece of mechanism, confined to a single
function at the load boundary: **`normalize()`**. Everything downstream of
`normalize()` is pure plumbing on a complete, self-contained tree.

---

## Goal

Each provider's config is defined **exactly once**, in its `*-config` crate, and
`ingest_config` *composes* those crates. Delete every inline `*Sync` struct, drop
**all** `#[serde(flatten)]`, and confine every cross-node derivation (path
resolution, global-default propagation) to one eager `normalize()` pass.

Done when: each provider's config is defined once; `ingest_config` declares no
`*Sync`, no `flatten`, and no lazy `resolved_*`/`merge` accessors; and
`bazel test //...` is green.

---

## Two kinds of "default" — keep one, banish the other to load time

1. **Built-in constant defaults** (serde `#[serde(default)]` / `Default`): a field
   absent in YAML falls to a constant (`blob_size_limit_bytes` → `None`,
   `extract_params` → 30min/50-fail, `event_tape` → enabled). These are **pure** —
   each field independently falls to a constant, no cross-node reference. **Keep
   them.** They don't break "hand the subtree to the code."

2. **Cross-node derivations** — the only real mechanism, and the source of today's
   complexity (lazy `resolved_raw_path(data_root)`, `resolved_shared(cfg)`,
   `SharedConfig::merge`, `shared_override`). Two of these exist:
   - **Path resolution**: `raw_path`/`input_path` default to
     `<data_root>/raw/<name>` — derived from a *global* (`data_root`) + the
     source's `name`.
   - **Global-default propagation**: the top-level `defaults:` block supplies
     base values for the shared tunables that each source may override.

   Both are **cross-node** (the effective value isn't in the source's own
   subtree), so both violate the principle *if evaluated lazily at the use site*.
   The fix is not to delete them — it's to evaluate them **eagerly, once**, in
   `normalize()`, producing a tree where every source is fully explicit and
   self-contained.

`data_root` and `defaults:` are therefore **authoring conveniences consumed only
by `normalize()`**. After normalization they have "evaporated" — `data_root` into
concrete absolute paths on each source, `defaults` into each source's now-complete
`common`.

---

## The model

### Ownership split

- **Orchestrator-owned identity** (NOT in any provider config schema):
  - `name` — the source's key in the `sources:` list. Providers use it only as a
    diagnostic label and already receive it at run time via `RunCtx::name`; no
    provider derives a path or DB key from it. Leaves the config schema; the
    runtime label handoff is unchanged.
  - `enabled` — whether to run the source. The provider never sees it.

- **Shared tunables** — composed into every `*-config` via a shared `SourceCommon`:
  `input_path`, `raw_path`, `blob_size_limit_bytes`, `extract_params`,
  `event_tape`. After `normalize()` these hold resolved, complete values.

- **Provider-specific** — already in each `*-config` crate (`SlackConfig.sync`,
  `EmailConfig.mbox`/`outlink_format`/…).

### Rust shape

```rust
// NEW schema-only crate `source_common` (deps: serde only)
//   holds SourceCommon + the relocated ExtractParams + EventTapeConfig
pub struct SourceCommon {
    #[serde(default)] pub input_path: Option<PathBuf>, // Some(abs) after normalize
    #[serde(default)] pub raw_path:   Option<PathBuf>, // Some(abs) after normalize
    #[serde(default)] pub blob_size_limit_bytes: Option<u64>,
    #[serde(default)] pub extract_params: ExtractParams,
    #[serde(default)] pub event_tape: Option<EventTapeConfig>,
}

// each *-config crate composes it, e.g. slack_config:
pub struct SlackConfig {
    #[serde(default)] pub common: SourceCommon,
    #[serde(default)] pub sync:   Option<SlackApiSync>,
}

// ingest_config: orchestrator envelope + a flatten-free newtype union
pub struct Config {
    pub data_root: PathBuf,
    #[serde(default)] pub qmd: QmdConfig,
    #[serde(default)] pub backend: BackendConfig,
    #[serde(default)] pub dolt: DoltConfig,
    #[serde(default)] pub sync: SyncConfig,
    /// Authoring sugar — base values folded into each source by `normalize()`.
    /// DO NOT read after load; consumers use the resolved per-source `common`.
    #[serde(default)] pub defaults: Defaults,
    #[serde(default)] pub sources: Vec<SourceEntry>,
}

pub struct SourceEntry {
    pub name: String,
    #[serde(default = "default_true")] pub enabled: bool,
    pub source: SourceConfig,
}

#[serde(tag = "type", rename_all = "snake_case")]
pub enum SourceConfig {
    SlackApi(slack_config::SlackConfig),
    Email(email_config::EmailConfig),
    // …one newtype arm per provider; payload IS the *-config type → defined once
}
```

`SourceConfig` is an **internally-tagged newtype enum** — serde reads `type:`,
strips it, deserializes the remainder into the inner `*-config` struct. No
`flatten` anywhere.

`Defaults` mirrors only the propagatable knobs (`blob_size_limit_bytes`,
`extract_params`, `event_tape`) — the same set `SourceCommon` carries, minus the
paths (paths are derived from `data_root`/`name`, not from `defaults`).

### `normalize()` — the single locus of mechanism

```rust
/// Run once, at load, immediately after deserialize. Turns the parsed config
/// into a fully-explicit, self-contained tree. The ONLY mechanism in the system.
fn normalize(cfg: &mut Config) {
    for entry in &mut cfg.sources {
        let common = entry.source.common_mut();
        // 1. propagate global defaults (source value wins; absent falls through)
        common.fold_defaults(&cfg.defaults);
        // 2. resolve paths from data_root + name (+ tilde expansion)
        common.resolve_paths(&cfg.data_root, &entry.name);
    }
    // `cfg.defaults` is now spent and must not be read again.
}
```

After `normalize()`, handing config to code is pure decomposition:
`entry.source` → the provider's `plan()`; `entry.source.common` (resolved) →
shared code; `entry.source.<provider field>` → the provider's extract. No
`resolved_*` accessors, no merge at any use site.

### YAML shape (target)

```yaml
data_root: ~/datalib

# Authoring sugar: base values folded into every source at load.
defaults:
  blob_size_limit_bytes: 5000000
  extract_params:
    maximum_time_without_progress_in_minutes: 30
    maximum_sequential_failed_requests: 50

sources:
  - name: gmail-takeout            # orchestrator: identity
    enabled: true                  # orchestrator: run-or-not
    source:
      type: email
      common:
        input_path: ~/backups/Takeout/.../All mail.mbox
      mbox: { ... }
      outlink_format: gmail

  - name: slack
    source:
      type: slack_api
      common:                      # overrides `defaults` for this source
        blob_size_limit_bytes: 1000000
        extract_params: { maximum_sequential_failed_requests: 100 }
      sync:
        media: true
        channels: ["project-data-liberation"]
```

This is **backwards-incompatible** — every checked-in `config.yaml` changes shape
(sources gain a `source:` level; shared tunables move under `common:`). Accepted.

### Typos / `deny_unknown_fields`

We do not regain strict unknown-field rejection (already inert today under
`flatten`). Out of scope; note it, don't chase it.

---

## Dependency layering (no cycles)

```
source_common (serde only)         ← SourceCommon, ExtractParams, EventTapeConfig
   ▲        ▲
   │        └── frankweiler_etl (retry/http consume ExtractParams)
   │                 ▲
   └── *-config crates (compose SourceCommon)
            ▲
            └── ingest_config (depends on source_common + all 16 *-config crates)
```

`ExtractParams` moves **down** from `frankweiler_etl` into `source_common` so the
`*-config` crates can name it without pulling ETL code; `frankweiler_etl` then
depends on `source_common` for it (re-export to keep retry/http/linkedin call
sites resolving). `EventTapeConfig` moves out of `ingest_config` into
`source_common` for the same reason.

---

## Provider inventory (16 arms)

| `type:` | `*-config` crate | inline `*Sync` to delete from ingest_config |
|---|---|---|
| `claude_api` / `claude_export` | `anthropic_config` | `ClaudeApiSync` |
| `chatgpt_api` | `chatgpt_config` | `ChatgptApiSync` |
| `slack_api` | `slack_config` | `SlackApiSync` |
| `github_api` | `github_config` | `GithubApiSync` |
| `gitlab_api` | `gitlab_config` | `GitlabApiSync` |
| `notion_api` | `notion_config` | `NotionApiSync`/`NotionInbox`/`NotionSubtrees` |
| `email` | `email_config` | `EmailSync`/`MboxSync`/`EmailOutlink` |
| `beeper` | `beeper_config` | `BeeperSync` |
| `carddav` | `carddav_config` | `CarddavSync` |
| `linkedin` | `linkedin_config` | (fetch_photos) |
| `google_takeout` | `google_takeout_config` | `GoogleTakeoutSync` |
| `perseus` | `perseus_config` | `PerseusSync` |
| `yolink` | `yolink_config` | `YolinkSync`/`YolinkDevice` |
| `signal_backup` | `signal_config` | `SignalSync` |
| `whatsapp_backup` | `whatsapp_config` | `WhatsAppSync` |
| `sms_backup_restore` | `sms_backup_restore_config` | — |

`claude_api` and `claude_export` are two `type:` values sharing one
`anthropic_config` — two enum arms, same payload type. Fine.

---

## Steps (each a scoped commit)

1. **Create `source_common` crate.** Move `ExtractParams` (from
   `frankweiler_etl::extract_params`) and `EventTapeConfig` (from
   `ingest_config`) into it; add `SourceCommon` with `fold_defaults` +
   `resolve_paths`. Re-export `ExtractParams` from `frankweiler_etl` so
   retry/http/linkedin keep resolving. Wire Bazel deps. `bazel build //...` green.

2. **Compose `SourceCommon` into each `*-config`.** Add `#[serde(default)]
   common: SourceCommon` to the 16 provider config structs; each gains a
   `source_common` dep. Add a `common_mut()`-style accessor where the enum needs
   it.

3. **Rewrite `ingest_config`.**
   - `Config` gains a real `defaults: Defaults` key (no `flatten`), documented as
     load-time-only.
   - `SourceEntry { name, enabled, source: SourceConfig }`.
   - `SourceConfig` = newtype enum, one arm per provider, payload = `*-config`.
   - Implement `normalize()` (fold defaults + resolve paths); call it from
     `load_config` right after deserialize.
   - **Delete** all inline `*Sync` structs, the old `SourceCommon` hand-mirror,
     `SharedConfig::merge`, `shared_override`, and every `resolved_*` accessor.
   - `is_managed`/`type_str`/`name` reimplemented against the new shape.
   - `validate()` delegates per-arm to each `*-config::validate()` (Notion/Yolink
     already expose it); drop the duplicated bodies.
   - Bazel deps on all 16 `*-config` crates + `source_common`.

4. **`sync/main.rs`.** Build `PlanCommon` directly from the **already-normalized**
   `source.common` (no `resolved_*` calls; `PlanCommon` becomes a thin projection
   + runtime `playback_root`). **Delete the `serde_yaml::to_value(src)`
   round-trip** (`:1280`); move the typed `*-config` straight out of
   `SourceConfig` into each provider's `plan()`. Update the `run_synthesize`
   match. (Collapsing `PlanCommon` into handing `SourceCommon` directly is a
   larger provider-touching change — deferred; keep `PlanCommon` for this PR.)

5. **Providers.** Adjust email/linkedin processor destructuring (they already
   consume `*-config` types — small). Confirm nothing reads a deleted `*Sync`.

6. **Checked-in YAML + tests.** Rewrite `docs/user/config_examples/{all_sources,
   claude_only,sample_config}.yaml` and the driver-generated `tests/fixtures`
   `extract.yaml` to the nested shape. Update `ingest_config` unit tests
   (`loads_one_of_each_source_type`, yolink/notion validation, and turn the
   shared/extract_params layering tests into **`normalize()` tests** — assert
   defaults fold in and paths resolve). Update e2e fixtures.

7. **Verify.** `bazel test //...` green (Bazel is the sole build of record — do
   not trust cargo).

---

## Early smoke test (do before touching all 16 providers)

Stand up `source_common` + **two** providers (e.g. `slack_config`,
`email_config`) end to end through the newtype enum + `normalize()`, and confirm a
representative `config.yaml` round-trips (parse → normalize → resolved tree) with
the expected absolute paths and folded defaults. Catch any `serde_yaml` newtype
surprise before fanning out.

---

## Validation checklist (don't regress)

- `//frankweiler/backend/ingest_config:ingest_config_unittests`
- `//frankweiler/backend/ingest_config:config_examples_test`
- `//tests/fixtures:ingested_tng_test`
- `bazel test //...`
