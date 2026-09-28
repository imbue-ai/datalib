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
  //datalib/backend/dag:datalib_dag_bin \
  //datalib/backend/datalib_step:datalib_step \
  //datalib/backend/signal-backup:signal_make_fixture
```

Anything before `--` is a test target. Anything after `--` is a
`rust_binary` that those tests invoke as a subprocess (LLVM needs the
binary on disk to translate the runtime hit counts back into source
locations). Output lands at `/tmp/datalib_coverage.lcov` by
default; override with `$LCOV_OUT`.

The test can be any kind. A `rust_test` that only spawns a helper
binary works the same way:

```bash
tools/run_coverage.sh \
  //datalib/backend/etl:doltlite_two_process_test \
  -- \
  //datalib/backend/etl:doltlite_two_process
```

**A test that runs library code in its own process goes after `--`
too.** The list after `--` is every binary whose hit counts the report
reads, and that includes the test binary itself when the code you care
about runs inside it. Leave it out and the report still names the
library's files, taken from a helper binary that links them, but every
line the test ran reads 0. It looks like a finished report, not an
error. The supervisor's tests run the loop in-process and spawn two
helpers, so all of them are listed:

```bash
T="//datalib/backend/dag:dag_unittests //datalib/backend/dag:supervisor_harness_test"
tools/run_coverage.sh $T -- $T \
  //datalib/backend/dag:puppet \
  //datalib/backend/dag:supervisor_store_worker
```

The report is first-party only. `--instrumentation_filter` cannot make
it so — `llvm-cov export` reads the coverage-mapping section out of the
linked binary, and that section names every file compiled into it,
vendored C included. The wrapper passes `--ignore-filename-regex`
instead; override the pattern with `$IGNORE_RE`. Without it the report
was 8.0 MB, 81% of it code we do not own (doltlite's `sqlite3.c`
amalgamation alone was 5.4 MB, plus oniguruma and ring's vendored
crypto), so `genhtml`'s tree view opened on third-party sources. With
it: 3.2 MB (2026-09-25).

HTML report:

```bash
genhtml -o /tmp/cov-html /tmp/datalib_coverage.lcov \
  --ignore-errors source,inconsistent,corrupt
open /tmp/cov-html/index.html
```

## What it measures

The most useful single coverage target right now is
`//tests/fixtures:ingested_tng_test`. It's a `py_test` wrapper around
the same `run_sync_pipeline.py` invocation as the `:ingested_tng`
genrule, exercising the **entire ETL pipeline** end-to-end across every
provider's TNG fixtures. With the wrapper above you get **489 source
files** covered, all of them first-party (measured 2026-09-25),
including:

  - the per-provider download + render (`claude`, `chatgpt`,
    `slack`, `notion`, `github`, `gitlab`, `beeper`, `signal`,
    `contacts`, `perseus`, `email`, `yolink`)
  - shared infra (`doltlite_raw`, `blob_cas`, `load`, `latchkey`,
    `obs`, `qmd_indexer`, `signal-backup` crypto + writer)
  - the `datalib-dag` orchestrator and `datalib-step` step binary
    themselves

That last point is the trick: `datalib_dag` and `datalib_step` are
`rust_binary` targets the python script spawns as subprocesses.
Naively, `bazelisk coverage` can't see into a subprocess. The setup
below makes it work.

## How it works

```
┌──────────────────┐   bazelisk coverage runs the test (py_test,
│  ingested_tng_   │   rust_test, ...). rules_rust's coverage
│      test        │   transition propagates through `data` deps
│                  │   (yes, even data!) so datalib_dag /
└────────┬─────────┘   datalib_step get built with -Cinstrument-coverage.
         │ data
         ▼
┌──────────────────┐   At test time the test spawns the
│  datalib_dag +   │   instrumented binaries. Each process writes a
│  datalib_step    │   .profraw on clean exit, at the path bazel sets
│  (rust_binary,   │   in LLVM_PROFILE_FILE (COVERAGE_DIR/%h-%p-%m),
│  -Cinstrument-   │   which children inherit.
│  coverage on)    │
└────────┬─────────┘
         │ profraw
         ▼
┌──────────────────┐   --experimental_split_coverage_postprocessing
│ testlogs/<pkg>/  │   makes COVERAGE_DIR an output of the test
│ <name>/_coverage │   action, so the profraws outlive the sandbox.
└────────┬─────────┘
         │ llvm-profdata merge, then llvm-cov export
         ▼  + binaries (llvm-cov needs them to map counters to source)
   /tmp/datalib_coverage.lcov
```

Bazel's own `coverage.dat` is not used. For a rust_test, rules_rust's
collection step (`util/collect_coverage/collect_coverage.rs`) merges
the profraws and exports them against the *test* binary. A test that
only spawns helpers links none of our code, so that binary has no
coverage mapping, the export says "no coverage data found", and
`coverage.dat` comes out empty. Without split post-processing the
profraws lived in the sandbox and were gone by then.

### Five things had to be true

1. **`-Cinstrument-coverage` reaches the binaries that run.** The
   `data` deps from the test to `datalib_dag` / `datalib_step` carry
   rules_rust's coverage transition through, so the binaries at
   `bazel-bin/datalib/backend/dag/datalib_dag_bin` and
   `bazel-bin/datalib/backend/datalib_step/datalib_step` after a
   `bazelisk coverage` invocation are the instrumented ones. This Just
   Works in rules_rust 0.70 — no custom transition, no
   `rustc_flags = select(...)`, no second binary target. Check one
   with `otool -l <binary> | grep -c __llvm_prf` (nonzero means
   instrumented).

2. **The profraws survive the test.** The wrapper passes
   `--experimental_split_coverage_postprocessing` (and
   `--experimental_fetch_all_coverage_outputs`, so a remote-cache hit
   brings them down too). Each test's profraws then sit in
   `$(bazelisk info bazel-testlogs)/<pkg>/<name>/_coverage/`, and the
   wrapper merges only the ones for the targets you passed. Because
   they are now outputs, a remote cache would take them too, so the
   wrapper also passes `--noremote_upload_local_results`. For
   `ingested_tng_test` they are 564 files and 1.0 GB, and on a slow
   link the upload held a finished test in "Testing" for over ten
   minutes.

3. **The LLVM tools match rustc's LLVM.** A profraw's format follows
   the compiler's LLVM version. The wrapper uses the `llvm-profdata`
   and `llvm-cov` shipped in the rules_rust toolchain repo (rustc
   1.98.0 ships LLVM 22; Xcode's is Apple LLVM 21), and passes them to
   the test as `--test_env=LLVM_PROFDATA --test_env=LLVM_COV`, which
   bazel's C++ collection script needs or it aborts with
   `LLVM_PROFDATA: unbound variable`. Set both variables yourself to
   override.

4. **The export names the right binaries.** `tools/run_coverage.sh`
   runs `llvm-cov export --format=lcov --instr-profile=<merged>
   <primary-bin> --object <extra-bin>...` with the binaries you list
   after `--`.

5. **The test crate is instrumented too.** An async fn's body is
   compiled into the crate that awaits it, not the one that defines
   it. When a `rust_test` awaits a library's async fn, the body that
   runs is the test crate's copy, and bazel leaves test crates
   uninstrumented unless told otherwise. Measured on
   `supervisor_harness_test`: `Store::invocations` counted 1,615 calls
   while every line of its body read 0; the profile held no record for
   the body at all. The wrapper passes `--instrument_test_targets`, so
   to see library code a `rust_test` exercises in-process, list the
   test itself after `--` as well.

### Why `data` and not `deps`?

A `py_test`'s `deps` are Python libraries — the Rust binaries can
only be `data`. The fact that the rules_rust coverage transition
flows through `data` is what makes this arrangement viable at all —
we don't need to re-architect the py_test to be a `rust_test`, and we
don't need a custom Starlark transition. A `rust_test` that spawns a
helper takes it as `data` too (`doltlite_two_process_test`).

## Adding coverage for a new pipeline

If you have another test target that drives a Rust binary as a
subprocess, the steps are:

1. Make sure the binary is in the test's `data` (not just runtime
   PATH discovery).
2. Run `tools/run_coverage.sh <test-target> -- <rust-binary>` —
   passing every `rust_binary` whose code you want represented in
   the lcov.
3. If the wrapper says `no .profraw`, nothing instrumented ran: the
   binary is probably outside `$INSTRUMENT` (default
   `^//datalib/backend[/:]`) or found on PATH instead of from `data`.
   If it says no line was hit, the binaries after `--` are not the
   ones the test ran.

## Branch coverage

Not available on the pinned compiler. The report has line and function
counts only; no `BRDA` records. Rust's branch coverage is
`-Zcoverage-options=branch`, and on stable rustc 1.98.0 both spellings
are refused (measured 2026-09-25): `-Ccoverage-options` is an unknown
codegen option, and `-Z` is only accepted on nightly. With
`RUSTC_BOOTSTRAP=1`, which unlocks nightly flags on a stable compiler,
a toy program did produce `BRDA` records, so the LLVM side works. We
have not wired that in. It would rebuild every crate under a flag
Rust makes no promises about, and a Rust bump does not help until the
option is stabilized.

## Future: Playwright / UI e2e coverage

The Playwright e2e suite at `//datalib/ui:e2e_test` drives the
backend through the HTTP server, which is a `rust_binary`. The same
mechanism should in principle work: add the backend binary to the
e2e test's `data`, run `tools/run_coverage.sh` with the e2e test
target before `--` and the backend binary after `--`. Untested as of
this writing. Would give us coverage of the request-path code that
the unit tests don't reach (HTTP routing, response serialization,
auth middleware, etc.).

## Limitations and gotchas

  - **Coverage is per-invocation.** The wrapper reads the
    `_coverage/` directories of the targets you pass, nothing else.
    Pass every test you want in one report in one call.
  - **Don't glob testlogs for profraws.** A `_coverage/` left by
    another target was written by binaries built at another time, and
    `llvm-profdata merge` rejects mismatched ones with `malformed
    instrumentation profile data: function hash is not a valid
    integer`. The wrapper resolves one directory per label for that
    reason.
  - **`bazelisk build` between coverage and export is a footgun.**
    After `bazelisk coverage`, the `bazel-bin/.../<binary>` symlink
    points at the *instrumented* artifact. A subsequent plain
    `bazelisk build <binary>` (no coverage flag) rebuilds the same
    output path with an *un-instrumented* binary, and `llvm-cov
    export` then fails with `no coverage data found`. The runner
    script deliberately does not do a second build.
  - **A failed BuildBuddy upload stops the wrapper.** `bazelisk
    coverage` exits non-zero when the build-event upload fails
    (`No route to host`), even though the test passed. Re-run; the
    test result is cached.

## Sources

  - [Instrumentation-based Code Coverage — rustc book](https://doc.rust-lang.org/rustc/instrument-coverage.html)
  - [cargo-llvm-cov README](https://github.com/taiki-e/cargo-llvm-cov)
    — the `%p%m` multi-process profraw pattern
  - [Bazel — Code coverage](https://bazel.build/configure/coverage)
  - [bazel/tools/test/collect_coverage.sh](https://github.com/bazelbuild/bazel/blob/master/tools/test/collect_coverage.sh)
