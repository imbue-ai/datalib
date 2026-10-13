# CI: GitHub Actions, Bazel and BuildBuddy

How the CI is wired, what each cache is for, how to read a slow run,
and what has been measured — in one place, so the next "CI feels slow"
starts from the numbers rather than from a hunch. The workflows are
`.github/workflows/*.yml`, the configs they use are in `.bazelrc`, and
a measurement's run ids are in its linked PR or issue.

The operational rules a contributor needs day to day are in
[`AGENTS.md`](../../AGENTS.md) § "Running tests"; this page is the
reference behind them.

## What runs where

**`test.yml`** is the merge gate. Every job runs on `ubuntu-latest`
(4 vCPU); the bazel ones run inside
`ghcr.io/imbue-ai/datalib_devcontainer:latest`:

- `cargo deny` — a RustSec and license scan of both Cargo lockfiles
  (`datalib/backend/deny.toml`, `datalib/tauri/deny.toml`), on the bare
  runner, with no bazel in it.
- `bazel test //...` — the repo hygiene lint, then
  `bazel test -c opt --config=release --config=ci --nostamp --jobs=16 -- //... -//datalib/ui:e2e_test`,
  then the "No crate built a second time for a tool" check (below), then
  a `bazel build //datalib/backend:bin` staged as a downloadable
  tarball. Everything but a `main` push adds `--config=pr-tests`, which
  builds our own crates at opt-level 1 and leaves third-party crates and
  doltlite's C at 3 (see "Every `rust_test` is a whole test binary"
  below); so a PR's tarball is not what a release ships.
- `bazel test //datalib/ui:e2e_test` — the Playwright suite, alone. It
  is tagged `cpu:4`, the whole runner, and takes about six minutes, so
  in the job above every other test waited behind it. The two jobs'
  patterns are complements (`//...` less one label, and that label), so
  a new test lands in the first without anyone choosing. On a run where
  the binaries the suite drives are not cached yet, both jobs compile
  them; neither waits for the other.
- `bazel build :dist (musl static)` — the fully static release leg,
  plus `doltlite_link_test` against those binaries. Separate job so
  its ~80 s is not on the other one's critical path.
- `warm the contributor cache` — on a `main` push only; see "Two caches"
  below.

A `pull_request` run builds **the merge of the PR into `main` as it
is at that moment** (`HEAD is now at … Merge <pr> into <main>` in the
checkout step), not the branch head. So when `main` moves, the PR's
next run re-executes whatever is unique to the PR *and* downstream of
what `main` changed, even though the PR did not. A `workflow_dispatch`
run builds the bare branch head; compare like with like before calling
a cache key unstable (#484's comments are the worked example).
`test.yml` also takes two dispatch inputs: `devcontainer_tag`, to run
against an image that is not yet `latest`, and `remote_execution`, to
run the test job on BuildBuddy's executors (`--config=remote`).

**`devcontainer.yml`** builds and publishes that image
(`.devcontainer/Dockerfile`, `FROM ubuntu:26.04`): on every `v*` tag,
and on demand — not on pushes or PRs, since a build is ~8 min and
~2.5 GB and a release's worth of drift costs seconds. To try a
Dockerfile change early, dispatch it on the branch and then `test.yml`
with the `sha-` tag it prints. Every build is tagged `sha-<commit>`,
a tag build also `:X.Y.Z`. `latest` moves on a tag build, or on a
dispatch with `publish_latest` set, and only after
`.devcontainer/smoke_test.sh` has analysed `//...` from the image
under a different path and HOME than it was built with, with zero
downloads. The newest six `sha-` versions are kept and older ones
pruned. It is a workflow of its own so that nothing in a release can
keep it from publishing (#500).

**`release.yml`** runs on a `v*` tag: the runtime assets first
(the Node runtime `qmd` and `latchkey` run from, which the binaries
fetch on first use — `runtime_fetch.md`; one per host platform, plus a
CUDA overlay for Linux x86_64), then the five tarballs (macOS arm64,
Linux gnu and musl on x86_64 and aarch64), the notarized macOS app, and
the prod docker image with its doc test (`datalib/docker/doc_test.sh`,
#469). It does not build the CI image. The steps that
assemble the assets are scripts under `scripts/release/`, run by
`bazel test //...` before any tag runs them — `release_steps.md`.

**Neither image build pulls from Docker Hub.** Anonymous Docker Hub
pulls are rate-limited per IP, and a GitHub-hosted runner shares its IP
with other people's jobs, so the quota can be spent before a run starts
(v0.42.0's `docker-publish` failed on `toomanyrequests`). Both
workflows take Docker Hub images through Google's pull-through mirror,
`mirror.gcr.io`: the QEMU and BuildKit images by name, and the
Dockerfiles' `FROM` lines through a registry mirror in
`setup-buildx-action`'s `buildkitd-config-inline`, so the Dockerfiles
themselves still name Docker Hub and build the same way on a laptop. A
new workflow that pulls a Docker Hub image does the same. A tag runs
its own tree, so a tag cut before the mirror landed (v0.42.0 and
earlier) still pulls from Docker Hub, and re-running its jobs cannot
change that.

**BuildBuddy** (`imbue.buildbuddy.io`) is the action cache, the build
event stream and the remote downloader for both `test.yml` bazel jobs.
`.github/actions/prepare-bazel` writes an API key into the gitignored
`.bazelrc.user` at run time, and `.bazelrc`'s `--config=buildbuddy`
turns it on. The image build sees none of it: `devcontainer.yml` never
runs `prepare-bazel`, `.bazelrc.user` is in `.dockerignore`, and what
the image bakes is public archives verified by sha256.

## Two caches: contributor and release

Bazel does not verify an action-cache hit, so whoever can write a
cache decides what a build reading it ships. Two BuildBuddy *groups*
(a group is BuildBuddy's isolation unit: its own cache, its own keys)
keep a release from replaying anything a laptop or a PR run wrote:

| group | key (GitHub secret) | writes | reads |
|---|---|---|---|
| contributor | `BUILD_BUDDY_API_KEY` | every laptop with the key, PR and dispatch runs of `test.yml` | the same |
| release | `BUILD_BUDDY_RELEASE_API_KEY` | `test.yml` on a `push` to `main` — PR-gated code only (`--config=buildbuddy-release`) | `release.yml`, read-only (`--config=buildbuddy-release-readonly`) |

Two BuildBuddy *organizations*, each on its own subdomain
(`imbue.buildbuddy.io`, `imbue-release.buildbuddy.io`): a key only
works against its own org's host, so `prepare-bazel` takes the key and
the config together. A tag builds a commit that is already on `main`,
so the release build hits exactly what the `main` run of that commit
produced; the `-readonly` config is what keeps it from writing, not the
key. Without the release secret a tag build runs cold (~20 min a leg)
rather than falling back to the contributor cache — that is the
intended failure mode.

**The two orgs are write-disjoint, so a third writer was needed.** A
main push writes the release org and a PR run writes the contributor
org, which means nothing wrote *main's tree* into the contributor org:
the first PR run after any merge that touched a shared crate rebuilt
the world, however small the PR's own diff. #658's run is the shape —
a PR touching `http` and `ui` alone, 1677 s, 713 sandboxed actions, 141
tests executed. `test.yml`'s `warm-contributor-cache` job is the
missing writer: on a main push it runs the same two commands the PR
jobs run, with the contributor key, `continue-on-error` because its
only product is cache entries. It does not help a PR that edits a
widely-linked crate — that one rebuilds its own `rdeps` either way —
it removes the runs that were cold for no reason.

The release org exists and its read-write key is the repository
secret; a second org means creating it from BuildBuddy's org switcher,
minting a key under its Settings → API keys, and `gh secret set
BUILD_BUDDY_RELEASE_API_KEY --repo imbue-ai/datalib`.

## The caches, and what each is for

**The BuildBuddy action cache** is the one that matters: on a warm run
of `main` every compile and test is a cache hit and the run executes
nothing. An action's key covers its toolchain, so a darwin-arm64 rustc
action and CI's linux-x86_64 one never share an entry — locally the
remote cache buys sharing with your own other worktrees and machines,
not a replay of CI's work.

**`--remote_download_minimal`** (`build:ci`, #497). A warm run has
~4400 cache hits; without this each hit's outputs were downloaded to
the runner (~320 thread-seconds, ~40 s of wall clock) and then written
a second time into a `--disk_cache` that dies with the container.
`minimal` fetches only what a local action or a test needs, and
`--disk_cache=` stops the copy. The tarball-staging step passes
`--remote_download_toplevel` on its own command line because it needs
real files. A failed test's log still reaches the runner — checked
with a deliberately failing test, not assumed.

**The pre-fetched output base in the image** (#500, #503). Without it
a warm run spends ~77 s before its first action downloading and, mostly,
*extracting* every external repository on 4 vCPUs; with them on disk
the same analysis takes ~16 s. The image's `bazelisk fetch //...`
puts them at `/opt/bazel/output-base`, and `prepare-bazel` points CI's
bazel there with `startup --output_base`. Two things about this that
are not obvious:

- **The user root has to be pinned too.** Bazel 9 keeps fetched
  repositories in a *repository contents cache* under the output user
  root (`$HOME/.cache/bazel/_bazel_root/cache/repos/v1/contents/…`)
  and makes `external/<repo>` a symlink into it. `HOME` is `/root`
  when the image is built and `/github/home` on a runner, so with only
  the output base pinned every entry looks absent from the runner's
  cache and is fetched again — measured at 157 downloads, 50 s, on the
  first invocation of every run, invisible to a smoke test run with
  `docker run`'s HOME=`/root`. Hence
  `--output_user_root=/opt/bazel/user-root` in the fetch, the smoke
  test and `prepare-bazel`, and the smoke test running under a
  different HOME.
- **Lockfile churn does not rebuild the image.** A stale pre-fetch
  degrades gracefully — bazel fetches only what the lockfiles added
  since — while `MODULE.bazel.lock` changed in 57 merges in one month.
  The tag build at each release is what keeps the delta small.

The downloaded archives are dropped from the image after the fetch
(0.7 GB the runner would pull for nothing); the extracted contents are
what a run uses.

**The remote downloader** (`--experimental_remote_downloader` in the
`buildbuddy` configs, with a local fallback) serves repository
downloads from BuildBuddy instead of the origin, so a repository added
after the image was built never reaches crates.io or GitHub from a
runner. The qmd GGUF models are build actions rather than repositories
(`//third-party/qmd_models`, #499): fetched once per pin into the
action cache, with retries on HuggingFace's 429, and never moved on a
warm run.

**What is deliberately not cached:** a `--disk_cache` in CI (see
above), an `actions/cache` of the repository cache (tried — see
"Tried and rejected"), and any GitHub Actions cache of bazel's outputs
(the `test.yml` header records why: "Lost inputs no longer available
remotely" against rules_rust's `ExtractCargoTomlEnvVars` actions).

## Reading a run

Every bazel invocation prints its BuildBuddy link. From a job log:

```bash
gh api "repos/imbue-ai/datalib/actions/runs/<run-id>/jobs" \
  --jq '.jobs[] | select(.name=="bazel test //...") | .id'
gh api "repos/imbue-ai/datalib/actions/jobs/<job-id>/logs" > /tmp/ci.log
grep -oE 'https://imbue\.buildbuddy\.io/invocation/[a-f0-9-]+' /tmp/ci.log | sort -u
grep -E 'INFO: Elapsed time|processes:|Executed [0-9]+ out of' /tmp/ci.log
```

The log is served only after the job finishes. The three `grep`ped
lines are usually the whole diagnosis:

- `N processes: A remote cache hit, B internal, C local, D
  processwrapper-sandbox` — **`D` is the real signal**; sandboxed
  actions are the ones that compiled or ran.
- `Executed N out of M tests` — on a warm `main` this is 0.
- `Critical Path` — read with care. On a saturated 4-vCPU runner it
  is mostly time an action spent *waiting for a slot*: in one 1664 s
  run the "critical path" was a single 65 s test that waited 1270 s.
  It says the machine was full, not that a serial chain is that long.

**Check the queue first.** A run can be slow without doing anything:
compare the run's `created_at` with the job's `started_at`. Median
queue is seconds; the tail has been over an hour.

**A run with no jobs at all was cancelled while pending.** `gh run
view <id> --json jobs --jq '.jobs | length'` says `0`, and its
`updatedAt` is within seconds of the *next* run's `createdAt`. A
concurrency group holds at most one pending run, so a newly queued run
evicts the one already waiting — `cancel-in-progress: false` protects
only the run that is executing. So only PR runs share a group (one per
ref) and supersede one another; a push and a dispatch each get a group
of their own (`github.run_id`), because a `main` run evicted before it
starts is a commit whose actions never reach the release cache (#674).

**Runs are bimodal.** A warm run executes 0 tests; a cold one, after a
change to a shared crate, rebuilds hundreds of opt-mode Rust actions
and re-runs most of the suite. A rising median means cold runs got
more frequent, not that anything got slower. The levers on a cold run
are its blast radius, which can be measured before pushing, and what
each rebuilt binary costs (the next paragraph):

```bash
bazelisk query 'kind(".*_test", rdeps(//..., //datalib/backend/etl:datalib_etl))'
bazelisk query 'kind(".*_test", rdeps(//..., //datalib/backend/schema:datalib_schema))'
```

**Every `rust_test` is a whole test binary, and the binary is where the
cost is.** A Rust test crate is not a `cc_test` linking prebuilt
objects: rustc compiles the leaf crate, generates code for every
generic it instantiates from tokio, sqlx, serde and axum, and links
the whole stack — doltlite's C included — statically. On a `main` push
that is all at opt-level 3, so the cache it warms is the release's. A
PR run and the contributor-cache warm build our own crates at opt-level
1 instead (`.bazelrc`'s `pr-tests`): on a 4-vCPU runner code generation
is the bill, and a PR needs the code correct, not fast. An opt-only bug
then shows on the `main` push rather than the PR.
The rlibs underneath are cache hits; that last step is not, and a
shared-crate edit repeats it once per test target. So one binary per
`tests/*.rs` file is the expensive layout, and the tree uses one binary
per package instead (#664, #665); a module that needs its own tags or
its own process runs from that binary as a slice.
[`testing.md`](testing.md) § "A package's integration tests are one
binary" has the layout.

**A `[for tool]` suffix on a `Compiling Rust …` line is a second
copy** — the crate built in the exec configuration as well as the
target one, nothing shared. A `genrule` puts its `tools` there, so a
Rust binary a genrule runs goes in `srcs` (`:ingested_tng` and
`:ingested_tng_qmd` are the examples). Proc-macros and build scripts
run at build time by nature, so the crates they reach are the one
legitimate exec-config set. CI's "No crate built a second time for a
tool" step fails on anything else (`tools/check_tool_crates.py`, which
names the tool that pulled each crate in).

**Per-action detail** lives in the profile every invocation uploads
and in BuildBuddy's cache scorecard, both behind internal RPCs that
take the same API key:

```bash
# the invocation's events, including the profile's CAS address
curl -s -X POST https://imbue.buildbuddy.io/rpc/BuildBuddyService/GetInvocation \
  -H "x-buildbuddy-api-key: $KEY" -H 'Content-Type: application/json' \
  -d '{"lookup":{"invocationId":"<id>"}}'
# a blob by its bytestream:// address (the profile is command.profile.gz)
curl -s -X POST https://imbue.buildbuddy.io/api/v1/GetFile \
  -H "x-buildbuddy-api-key: $KEY" -H 'Content-Type: application/json' \
  -d '{"uri":"bytestream://imbue.buildbuddy.io/blobs/<hash>/<size>"}'
# every action-cache read/write, with hit/miss and which run wrote the hit
curl -s -X POST https://imbue.buildbuddy.io/rpc/BuildBuddyService/GetCacheScoreCard \
  -H "x-buildbuddy-api-key: $KEY" -H 'Content-Type: application/json' \
  -d '{"invocationId":"<id>","filter":{"mask":"cacheType,search","cacheType":"AC","search":"<target substring>"},"groupBy":"GROUP_BY_TARGET"}'
```

The profile's `action processing` events carry per-action durations
(including slot wait), `Fetching repository` and the `download` /
`download_and_extract` builtin calls say what a run fetched, and the
`CPU usage (total)` / `System load average` counters say whether the
box was saturated. The scorecard's `status.code` 5 is NOT_FOUND, and
`originInvocationId` on a hit names the run that produced it. Profile
blobs age out of the CAS after a few days. Input directory blobs are
not uploaded for cache-only builds, so an input tree cannot be diffed
that way — `--execution_log_json_file` on a probe run is how to see
every input digest.

**Flaky tests**: hitting "re-run failed jobs" replays the same commit,
so a commit with both a failure and a success flaked.
`scripts/flaky_tests.py --limit 400` groups runs by commit and names
the targets. It only sees flakes somebody re-ran, and GitHub deletes
logs after 90 days.

**A test log over 1 MB is not printed.** Bazel prints a failing
test's log into the job's console only when it is under 1 MB
(`--experimental_ui_max_stdouterr_bytes`); over that, the console says
`exceeds maximum size … skipping` and nothing else. A test that runs a
pipeline keeps the full output in a file under its undeclared outputs
and a short tail in its log. `ingested_tng_test` does this, and when
the run is red CI uploads its `outputs.zip` as `ingested-tng-outputs`.

## What has been measured

Warm runs unless noted; the `bazel test //...` invocation of the test
job, from that job's log. Each row's PR has the run ids.

| date | change | before | after |
|---|---|---|---|
| 2026-09-16 | fixture binaries built once, not in both configurations (#484) | cold run 1588 s | cold run 1289 s (−19%) |
| 2026-09-16 | `--remote_download_minimal`, no disk cache in CI (#497) | 142.7 s | 84.8 s |
| 2026-09-16 | qmd models as build actions (#499) | 84.8 s | 80.8 s, and 1.8 GB less transfer per run |
| 2026-09-17 | the image its own, with the pre-fetched output base (#500, #503) | 75.9 s; `Analyzed` at +72 s | 45 s; `Analyzed` at +16 s |
| 2026-09-18 | e2e runfiles carry the embedding model alone (#550) | runs that execute the e2e suite: 349–463 s, with a 1.8 GB download of two models the suite never loads on the serial tail (~20 s quiet, ~60 s under load) | 376 s; no model download. A transfer cut inside the noise on a quiet day — the runs where the tail was 60 s were the ones where BuildBuddy was busy |
| | test job wall clock, warm | ~250 s | ~144 s |
| 2026-09-22 | `datalib/backend/http`'s 14 integration-test targets become 2 (#664) | for those targets: 447 s of `Compiling Rust bin`, 254 s of `Clippy`, 290 s of `Testing` | 39 s, 5 s, 24 s. The before run's box was saturated (713 sandbox actions) and the after run's was idle, so read it as "~10x, direction certain, factor approximate". The merged 13-file binary compiles in 23 s where each 1-file binary took 20–50 s: the per-file content is nearly free, the per-binary link is the whole cost |

| 2026-10-09 | the Playwright suite in a job of its own (#1139) | from dispatch to the last test job done, median: 1565 s after a one-line change to `datalib_etl` (n=3), 614 s after a change to one e2e spec (n=5), 163 s with nothing changed | 974 s (n=3), 488 s (n=5), 156 s |

The container pull (`Initialize containers`) is 55–75 s and is the
largest fixed cost left. Dropping the archives from the image brought
it back to within 7 s of the old, smaller image; pushing the layers
zstd-compressed instead of gzip (#506) took a further ~5 s off the
test job's pull (64 s → 56–60 s, two runs) and cut the *push* in
`devcontainer.yml` from 189 s to 47 s.

## Tried and rejected

- **A persisted repository cache via `actions/cache`** (#496, closed).
  The hit worked, but the test job's cache is 6.3 GB and restoring it
  took 91 s against the 58 s of fetching it saved: net +33 s. A
  lockfile change would also have paid a ~5 min upload. The image's
  pre-fetch is the same idea with the bytes in a layer instead, where
  they cost ~7 s.
- **The models as actions, for speed** (#499). They were fetched in
  parallel with the Rust toolchain and were never analysis's critical
  path; −4 s. Kept for the transfer, not the time.
- **Larger GitHub runners.** Billed even on public repos; a 16-core
  box modelled at ~$160/month for a cold run at 47% of today's
  (#324's comments).
- **Rebuilding the image on pushes and PRs.** Lockfile changes alone
  would have meant 67 builds in a month at ~8 min and ~2.5 GB each,
  and even Dockerfile-only triggers fire on every review iteration; a
  stale pre-fetch costs only the delta, so a release's cadence is
  enough.

## Next levers, in the order they are worth trying

1. **More test jobs.** Measured on experiment branches beside #1139's
   runs, same two changes, medians: the provider tests
   (`//datalib/backend/etl/providers/...`) in a job apart from the
   rest, 974 s → 701 s on the `datalib_etl` change (n=3); the e2e suite
   in three Playwright shards (`--test_arg=--shard=i/3`), 488 s → 311 s
   on the e2e-spec change (n=4) and nothing on the `datalib_etl` one,
   where the Rust test job is the slowest. Each job added is another
   required check and another chance to lose DNS to BuildBuddy.
2. **BuildBuddy remote execution** (`--config=remote`): −34% on a cold
   run, measured in #324, left dispatch-only pending the free tier's
   cache-transfer quota — which #497 and #499 have since cut by most
   of what a warm run moved. Measured again beside #1139's runs, on
   the `datalib_etl` change: 496 s and 604 s against the runner's
   1565 s, once the executors' cache held the tree (the two runs
   before that rebuilt ~2700 actions and took ~25 min each). In all
   six runs `//datalib/backend/applets:applet_unittests` timed out at
   300 s on the executors; on a runner it takes ~30 s. That is the
   thing to explain before this can gate anything.
3. **A warm runner.** GitHub-hosted runners are fresh VMs, so the
   image pull (55–90 s) and the analysis (16 s) are paid every job.
   BuildBuddy's hosted CI runners keep the bazel server and its caches
   between runs, which removes both; self-hosted runners do the same
   at the cost of a machine. Either is a cost decision, not a config
   change.
4. **Blast radius** — what a cold run has to rebuild, and a design
   question per crate (the `rdeps` numbers above are the price tags).

## Locally, you are probably not on the remote cache

Two things hide this: `.bazelrc` gives everyone a machine-wide disk
cache (`~/Library/Caches/bazel-disk-cache`), which makes local builds
feel fast for actions *you* have built before; and the remote cache
needs `.bazelrc.user`, which is gitignored and per-workspace —
`try-import %workspace%/.bazelrc.user` resolves to the worktree root,
so a file in `datalib/` is invisible to every `.claude/worktrees/*`
clone. The `processes:` line settles it: a run on the remote cache
says `remote cache hit`; a local run without the key never does.

Keep one real file outside the repo and symlink it in:

```bash
mkdir -p ~/.config/datalib && chmod 700 ~/.config/datalib
cat > ~/.config/datalib/bazelrc.user <<'EOF'
common --remote_header=x-buildbuddy-api-key=<your-key>
build --config=buildbuddy
EOF
chmod 600 ~/.config/datalib/bazelrc.user
for d in . .claude/worktrees/*/; do
    ln -sfn ~/.config/datalib/bazelrc.user "$d/.bazelrc.user"
done
```

Not in `$HOME/.bazelrc`: the `buildbuddy` config exists only in this
repo's `.bazelrc`, and every other workspace on the machine would fail
with "Config value 'buildbuddy' is not defined". `--config=remote` is
CI-only for the same reason the cache shares nothing with a mac: the
autodetected cc toolchain is generated from the client host, so
driving Linux executors from a mac hands them a darwin toolchain.
