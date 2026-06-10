# Coverage

`bazelisk coverage` works on this repo via LLVM source-based coverage,
including for **Rust binaries that tests launch as subprocesses**.
That last bit was the wrinkle; this doc records how we set it up and
how to use it.

## TL;DR — running coverage

```bash
tools/run_coverage.sh \
  //tests/fixtures:ingested_tng_test \
  -- \
  //frankweiler/backend/sync:frankweiler_sync_bin \
  //frankweiler/backend/signal-backup:signal_make_fixture
```

Anything before `--` is a test target. Anything after `--` is a
`rust_binary` that those tests invoke as a subprocess (LLVM needs the
binary on disk to translate the runtime hit counts back into source
locations). Output lands at `/tmp/frankweiler_coverage.lcov` by
default; override with `$LCOV_OUT`.

HTML report:

```bash
genhtml -o /tmp/cov-html /tmp/frankweiler_coverage.lcov \
  --ignore-errors source,inconsistent,corrupt
open /tmp/cov-html/index.html
```

## What it measures

The most useful single coverage target right now is
`//tests/fixtures:ingested_tng_test`. It's a `sh_test` wrapper around
the same `run_sync_pipeline.py` invocation as the `:ingested_tng`
genrule, exercising the **entire ETL pipeline** end-to-end across every
provider's TNG fixtures. With the wrapper above you get ~150 source
files covered including:

  - the per-provider extract + translate (`anthropic`, `chatgpt`,
    `slack`, `notion`, `github`, `gitlab`, `beeper`, `signal`,
    `contacts`, `perseus`, `email`, `yolink`)
  - shared infra (`doltlite_raw`, `blob_cas`, `load`, `latchkey`,
    `obs`, `qmd_indexer`, `signal-backup` crypto + writer)
  - the `frankweiler_sync` orchestrator itself

That last point is the trick: `frankweiler_sync_bin` is a `rust_binary`
the python script spawns as a subprocess. Naively, `bazelisk coverage`
can't see into a subprocess. The setup below makes it work.

## How it works

```
┌──────────────────┐   bazelisk coverage runs the sh_test.
│  ingested_tng_   │   rules_rust's coverage transition propagates
│      test        │   through `data` deps (yes, even data!) so
│   (sh_test)      │   frankweiler_sync_bin gets built with
└────────┬─────────┘   -Cinstrument-coverage.
         │ data
         ▼
┌──────────────────┐   At test time, the sh_test spawns the
│ frankweiler_     │   instrumented binary. LLVM's static runtime
│   sync_bin       │   writes a .profraw on clean exit, paths
│  (rust_binary,   │   chosen by LLVM_PROFILE_FILE in COVERAGE_DIR.
│  -Cinstrument-   │
│  coverage on)    │
└────────┬─────────┘
         │ profraw
         ▼
┌──────────────────┐   Bazel's per-test coverage runner merges
│ coverage.dat     │   profraws → indexed profile data, lands
│   (LLVM indexed  │   at .../testlogs/<target>/coverage.dat.
│   profile, v12)  │
└────────┬─────────┘
         │ + binaries  (llvm-cov needs the binary to map counters
         ▼              back to source locations)
┌──────────────────┐
│ llvm-cov export  │   Manual step — bazel's auto-export step
│  --format=lcov   │   doesn't know which rust_binary subprocess to
│                  │   point at, so it produces an empty lcov.
└────────┬─────────┘   tools/run_coverage.sh does this step.
         │
         ▼
   /tmp/frankweiler_coverage.lcov
```

### Three things had to be true

1. **`-Cinstrument-coverage` reaches the binary that runs.** The
   `data` dep from the sh_test to `frankweiler_sync_bin` carries
   rules_rust's coverage transition through, so the binary at
   `bazel-bin/frankweiler/backend/sync/frankweiler_sync_bin` after a
   `bazelisk coverage` invocation is the instrumented one. This Just
   Works in rules_rust 0.70 — no custom transition, no
   `rustc_flags = select(...)`, no second binary target. The audit
   trail of how we found this out is in `docs/data_architecture.md`'s
   commit history.

2. **LLVM tools are on PATH where bazel can find them.** The bazel
   coverage runner's collection script needs `llvm-profdata` and
   `llvm-cov`. On macOS those come from Xcode Command Line Tools at
   `/Library/Developer/CommandLineTools/usr/bin/`.
   `tools/run_coverage.sh` sets `LLVM_PROFDATA` and `LLVM_COV` via
   `xcrun --find` and passes them through to the test environment
   with `--test_env=LLVM_PROFDATA --test_env=LLVM_COV`. Without
   these, the collection script aborts with
   `LLVM_PROFDATA: unbound variable` and `error: coverage collection
   script failed`.

3. **The lcov export step uses the right binary.** Bazel's auto-export
   pass produces an empty lcov for our case because it can't tell
   which `rust_binary` the sh_test invoked. `tools/run_coverage.sh`
   does the explicit `llvm-cov export --format=lcov
   --instr-profile=<dat> <primary-bin> --object <extra-bin>...` call
   itself.

### Why `data` and not `deps`?

`sh_test` only has `data`, not `deps`. The fact that the rules_rust
coverage transition flows through `data` is what makes this
arrangement viable at all — we don't need to re-architect the sh_test
to be a `rust_test`, and we don't need a custom Starlark transition.

For `rust_test` targets the coverage transition flows through `deps`
the same way it does through `data`; there's no functional difference
for our purposes.

## Adding coverage for a new pipeline

If you have another test target that drives a Rust binary as a
subprocess, the steps are:

1. Make sure the binary is in the test's `data` (not just runtime
   PATH discovery).
2. Run `tools/run_coverage.sh <test-target> -- <rust-binary>` —
   passing every `rust_binary` whose code you want represented in
   the lcov.
3. If the bazel auto-coverage step still complains about
   `LLVM_PROFDATA: unbound variable`, the env var wasn't propagated;
   check that `--test_env=LLVM_PROFDATA --test_env=LLVM_COV` was
   passed (the wrapper script does this automatically).

## Future: Playwright / UI e2e coverage

The Playwright e2e suite at `//frankweiler/ui:e2e_test` drives the
backend through the HTTP server, which is a `rust_binary`. The same
mechanism should in principle work: add the backend binary to the
e2e test's `data`, run `tools/run_coverage.sh` with the e2e test
target before `--` and the backend binary after `--`. Untested as of
this writing. Would give us coverage of the request-path code that
the unit tests don't reach (HTTP routing, response serialization,
auth middleware, etc.).

## Limitations and gotchas

  - **macOS only as configured.** `tools/run_coverage.sh` uses `xcrun
    --find` to locate the LLVM tools. On Linux you'd point
    `LLVM_PROFDATA` and `LLVM_COV` at whatever ships with the system
    LLVM and skip the xcrun lookup.
  - **Per-test profdatas don't merge across runs.** If you run
    coverage on one test target and then on another, the second
    run's `tools/run_coverage.sh` invocation only includes the
    profdata from the second run. Coverage is per-invocation, not
    cumulative across invocations — the runner only walks the test
    targets you pass in.
  - **Stale `coverage.dat` files under `bazel-testlogs/`.** Old
    profdatas from prior coverage runs against different binaries
    can sit on disk indefinitely. The runner avoids them by
    resolving per-target paths from the labels you pass in, not by
    globbing testlogs. If you ever switch back to a glob-based
    workflow, `llvm-profdata merge` will reject the batch with
    `malformed instrumentation profile data: function hash is not a
    valid integer`.
  - **`bazelisk build` between coverage and export is a footgun.**
    After `bazelisk coverage`, the `bazel-bin/.../<binary>` symlink
    points at the *instrumented* artifact. A subsequent plain
    `bazelisk build <binary>` (no coverage flag) rebuilds the same
    output path with an *un-instrumented* binary, and `llvm-cov
    export` then fails with `no coverage data found`. The runner
    script deliberately does not do a second build.

## Sources

  - [Instrumentation-based Code Coverage — rustc book](https://doc.rust-lang.org/rustc/instrument-coverage.html)
  - [cargo-llvm-cov README](https://github.com/taiki-e/cargo-llvm-cov)
    — the `%p%m` multi-process profraw pattern
  - [Bazel — Code coverage](https://bazel.build/configure/coverage)
  - [bazel/tools/test/collect_coverage.sh](https://github.com/bazelbuild/bazel/blob/master/tools/test/collect_coverage.sh)
