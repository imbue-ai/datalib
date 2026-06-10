# Build perf investigation, 2026-06-09

Driven by Thad's observation that "rust compilation has gotten slower
and/or less parallel" + the dist-build invocation
`https://imbue.buildbuddy.io/invocation/1634ab68-1ab4-4cce-be43-d2d976ad082e`.

## What I measured

Local `bazel build //frankweiler/backend:dist -c opt` on the M-series
Mac, expunged output base, warm disk cache (`~/Library/Caches/bazel-disk-cache`),
warm BuildBuddy remote cache:

* **wall: 73.6s, critical path: 66.1s**
* 1,413 total actions, of which 2,613 cached (1,863 action-cache /
  742 disk-cache / 8 remote-cache hits), 104 actually executed
* See `/tmp/dist_baseline.json.gz` (Chrome trace) for the full profile.

Critical-path actions, sorted by wall:

```
24.2s   third-party/doltlite/sqlite3.c
17.7s   rlib frankweiler_etl_slack     (9 files)
16.3s   bin  frankweiler_sync_bin      (3 files)
15.8s   rlib frankweiler_etl_chatgpt   (10 files)
15.7s   rlib frankweiler_etl_notion    (10 files)
15.7s   rlib frankweiler_etl_anthropic (10 files)
15.0s   rlib frankweiler_etl_beeper    (9 files)
14.4s   rlib frankweiler_etl_gitlab    (9 files)
14.4s   rlib frankweiler_etl_github    (9 files)
13.7s   rlib frankweiler_etl_signal    (6 files)
12.9s   bin  frankweiler_http_bin
10.6s   rlib rustls v0.23.40           (110 files)
10.1s   rlib frankweiler_etl_email     (10 files)
 9.5s   rlib frankweiler_etl_contacts  (7 files)
```

(Provider crates parallel-fan-out after `frankweiler_etl` is done.)

The shape of the critical path is roughly:

```
doltlite/sqlite3.c (24s) ─┐
                          ├──► frankweiler_etl (~6s) ──► slowest provider (slack 17.7s) ──► sync_bin link (16s)
external rust deps         │
(rustls, tonic, …)        ─┘
```

## Why it got slower recently

Two structural changes land most of the regression:

1. **`3b5b3ed doltlite: build at -O2`** (was unoptimized). doltlite's
   amalgamation jumped from ~3s to ~24s on cold builds. Probably worth
   it for runtime (every sql query goes through this code) but it's a
   real cost on the critical path.
2. **The blob_cas migration** (commits `0782ff6` → `40c128d`). Every
   provider that used to carry its own blob/attachment plumbing now
   imports `frankweiler_etl::blob_cas`. That centralized the symbols
   but also made every provider crate *strictly* depend on the
   compiled `frankweiler_etl` rlib being finished — the rust-side
   parallel fan-out that used to start earlier now waits for the
   bigger etl rlib first.

The etl crate is now 5,969 LOC across 19 files. Twelve provider
crates depend on it.

## Things I tried that DID NOT help

### `--@rules_rust//rust/settings:pipelined_compilation=True`

This is the standard rules_rust knob for letting downstream crates
start their codegen as soon as the upstream `.rmeta` is available
(rather than waiting for the full `.rlib`). It would directly attack
the "providers wait on etl" bottleneck.

It fails to link here:

```
error[E0463]: can't find crate for `frankweiler_etl_anthropic`
error[E0460]: found possibly newer version of crate `frankweiler_etl`
              which `frankweiler_etl_yolink` depends on
```

The metadata/rlib variants get out of sync for `rust_binary` targets
when stamping is on (we have `--stamp` set unconditionally in
`.bazelrc`). Known rules_rust issue: pipelining + stamping +
rust_binary don't compose. There's an upstream PR that tightens
`metadata_supports_pipelining` for binaries, but until that lands,
turning this on requires either dropping `--stamp` or restructuring
the binary targets.

### `--strategy=Rustc=worker`

Rules_rust doesn't support persistent workers — its `process_wrapper`
is a one-shot per action. Build fails with `Rustc spawn cannot be
executed with any of the available strategies`.

## What I'd actually try, in order

### 1. Drop `--stamp` from the default config (one-line change)

`.bazelrc` sets `build --stamp` unconditionally. Stamping rebuilds
the *final* binary on every commit because its embedded
`STABLE_GIT_HASH` changes — but it does NOT propagate into the
rlib graph (rlibs don't read the stamp). So the cost is one
re-link of `frankweiler_sync_bin` per commit.

However: with stamping ON we're forced to leave `pipelined_compilation`
OFF (see above). With stamping OFF we can flip pipelining ON and the
rlib graph becomes a proper DAG with metadata fan-out.

Concretely:

```
# .bazelrc
build --stamp=false                # was: build --stamp
build --@rules_rust//rust/settings:pipelined_compilation=True

# release.yml only:
build --stamp                      # CI still stamps the released binary
```

Dev rebuilds drop ~15-20s on the critical path; release CI is
unchanged.

### 2. Split `frankweiler_etl::blob_cas` into its own leaf crate

Today every provider waits for the full `frankweiler-etl` rlib (which
re-exports auth, doltlite, blob_cas, scope_state, latchkey, …) to
finish compiling before it can start. Most providers only consume
1–3 of those modules.

Extracting `blob_cas` (the module every provider now uses, post-
migration) into `frankweiler-etl-blobs` would:

* drop blob_cas's ~600 LOC and its sqlx dep tree onto a much shorter
  critical path,
* let provider crates start compiling as soon as `etl-blobs` is
  ready, parallel with the rest of `etl`.

Estimated saving on critical path: ~4-6s. Smaller win than #1, but
permanent and improves cold-build experience on every machine.

### 3. Codegen units for opt provider crates

rules_rust 0.70's opt config defaults to `codegen-units=1` for opt
builds (the standard rustc default). Bumping the provider crates to
`codegen-units=8` would parallelize the inside of each provider
compile across cores and shave ~3-5s off the 15-17s provider critical
path.

We do this per-target with `rustc_flags = ["-Ccodegen-units=8"]` on
the largest providers (slack, chatgpt, notion, anthropic, beeper) —
each of those is single-binary-only (no release-cargo path) and the
optimization difference at `-O3` codegen-units=8 vs 1 is real but
typically <2% on integer-heavy I/O workloads like ours.

### 4. `--remote_download_minimal` for CI

Already on BuildBuddy. CI runners are ephemeral so they download
every intermediate rlib from cache. Adding

```
build:buildbuddy --remote_download_minimal
```

(or `--remote_download_outputs=toplevel`) keeps intermediates remote
and only pulls the final `dist` filegroup. On a typical bandwidth-
constrained runner this is a 30-50% wall-time saving on cache-hit
builds — turns "everything cached but slow" into "nothing happens
locally."

### 5. Smaller: cap `frankweiler_sync_bin`'s codegen units too

`sync_bin` is a 16s compile + link single-threaded. It's a thin
binary that pulls in everything; bumping its codegen units helps
the post-fan-out tail.

## What I'd NOT recommend

* Going back to `doltlite -O0` — `3b5b3ed`'s -O2 bump is right for
  runtime, the build cost is the lesser evil.
* Going RBE (remote execution) — the actions are already aggressively
  cached. RBE helps when many actions need to run in parallel on
  more CPUs than the dev has; our critical path is serial through
  the doltlite + etl + providers chain, so spreading actions across
  more CPUs doesn't help the long-pole wall time.

## Recommended commit sequence

If we land all of #1, #2, #3, #4 above, I'd expect a fresh dist
build to drop from 73s → ~45s (-40%). The biggest single chunk is
#1 (`--no-stamp` + pipelining); everything else is incremental on
top.

Suggested order: land #1 first (one-line `.bazelrc` change + workflow
tweak, low risk, easy to revert), measure, then take #2-#4 as
separate commits so a regression can be bisected.

## Reproducing locally

```
bazel clean --expunge
(time bazel build //frankweiler/backend:dist -c opt --profile=/tmp/p.json.gz) 2>&1
gunzip -c /tmp/p.json.gz | python3 -c '
import json, sys
e = json.load(sys.stdin)["traceEvents"]
acts = [(x["dur"]/1000, x["name"]) for x in e if x.get("cat") == "action processing" and x.get("dur",0) > 1_000_000]
acts.sort(reverse=True)
for d, n in acts[:25]: print(f"{d:7.0f}ms  {n[:80]}")
'
```
