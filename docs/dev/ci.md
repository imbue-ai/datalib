# CI: GitHub Actions, Bazel and BuildBuddy

How the CI is wired, what each cache is for, how to read a slow run,
and what has been measured — in one place, so the next "CI feels slow"
starts from the numbers rather than from a hunch. Everything here was
verified against the tree or a real run on the date given; the run ids
are in the linked PRs and issues, so a claim can be re-checked.

The operational rules a contributor needs day to day are in
[`AGENTS.md`](../../AGENTS.md) § "Running tests"; this page is the
reference behind them.

## What runs where

**`test.yml`** is the merge gate. Two jobs on `ubuntu-latest` (4 vCPU),
both inside `ghcr.io/imbue-ai/datalib_devcontainer:latest`:

- `bazel test //...` — the repo hygiene lint, then
  `bazel test -c opt --config=release --config=ci --nostamp --jobs=16 //...`,
  then a `bazel build //datalib/backend:bin` staged as a downloadable
  tarball.
- `bazel build :dist (musl static)` — the fully static release leg,
  plus `doltlite_link_test` against those binaries. Separate job so
  its ~80 s is not on the other one's critical path.

A `pull_request` run builds **the merge of the PR into `main` as it
is at that moment** (`HEAD is now at … Merge <pr> into <main>` in the
checkout step), not the branch head. So when `main` moves, the PR's
next run re-executes whatever is unique to the PR *and* downstream of
what `main` changed, even though the PR did not. A `workflow_dispatch`
run builds the bare branch head; compare like with like before calling
a cache key unstable (#484's comments are the worked example).
`test.yml` also takes a `devcontainer_tag` dispatch input, to run
against an image that is not yet `latest`.

**`devcontainer.yml`** builds and publishes that image
(`.devcontainer/Dockerfile`, `FROM ubuntu:24.04`): on every `v*` tag,
when the Dockerfile changes (a PR builds and smoke-tests without
moving `latest`), and on demand. Every build is tagged `sha-<commit>`,
a tag build also `:X.Y.Z`, and `latest` moves only after
`.devcontainer/smoke_test.sh` has analysed `//...` from the image
under a different path and HOME than it was built with, with zero
downloads. Old `sha-` versions are pruned. Nothing in it waits on a
release job — the previous publish, a job at the end of `release.yml`
behind the prod image's doc check, was skipped for four releases and
CI ran on a two-week-old image (#500).

**`release.yml`** runs on a `v*` tag: the six tarballs, the notarized
macOS app, and the prod docker image with its doc test (#469). It does
not build the CI image.

**BuildBuddy** (`imbue.buildbuddy.io`) is the action cache, the build
event stream and the remote downloader for both `test.yml` jobs.
`.github/actions/prepare-bazel` writes the API key from the
`BUILD_BUDDY_API_KEY` secret into the gitignored `.bazelrc.user` at
run time, and `.bazelrc`'s `--config=buildbuddy` turns it on. The
image build sees none of it: `devcontainer.yml` never runs
`prepare-bazel`, `.bazelrc.user` is in `.dockerignore`, and what the
image bakes is public archives verified by sha256.

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

**The pre-fetched output base in the image** (#500, #503). A warm run
used to spend ~77 s before its first action downloading and, mostly,
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
  the output base pinned every entry looked absent from the runner's
  cache and was fetched again — 157 downloads, 50 s, on the first
  invocation of every run, and the image's own smoke test could not
  see it because `docker run` has HOME=`/root`. Hence
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
`buildbuddy` config) serves repository downloads from BuildBuddy
instead of the origin, so a repository added after the image was built
never reaches crates.io or GitHub from a runner. It was added for the
qmd GGUF models after HuggingFace answered a cold fetch with a 429
(2026-09-03); those are build actions now (`//third-party/qmd_models`,
#499), fetched once per pin into the action cache and never moved on a
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

**Runs are bimodal.** A warm run executes 0 tests; a cold one, after a
change to a shared crate, rebuilds hundreds of opt-mode Rust actions
and re-runs most of the suite. A rising median means cold runs got
more frequent, not that anything got slower. Blast radius is the only
lever on a cold run, and it can be measured before pushing:

```bash
bazelisk query 'kind(".*_test", rdeps(//..., //datalib/backend/etl:datalib_etl))'
bazelisk query 'kind(".*_test", rdeps(//..., //datalib/backend/schema:datalib_schema))'
```

**A `[for tool]` suffix on a `Compiling Rust …` line is a second
copy** — the crate built in the exec configuration as well as the
target one, nothing shared. A `genrule` puts its `tools` there, so a
pipeline binary a genrule runs goes in `srcs`; `aquery
'mnemonic("Rustc", //...)'` grouped by `Configuration:` is the check,
and the only exec-config Rustc actions left should be the
dependency-free `qmd_indexer` chain (#484).

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

## What has been measured

Warm runs unless noted; the `bazel test //...` invocation of the test
job, from that job's log. Each row's PR has the run ids.

| date | change | before | after |
|---|---|---|---|
| 2026-09-16 | fixture binaries built once, not in both configurations (#484) | cold run 1588 s | cold run 1289 s (−19%) |
| 2026-09-16 | `--remote_download_minimal`, no disk cache in CI (#497) | 142.7 s | 84.8 s |
| 2026-09-16 | qmd models as build actions (#499) | 84.8 s | 80.8 s, and 1.8 GB less transfer per run |
| 2026-09-17 | the image its own, with the pre-fetched output base (#500, #503) | 75.9 s; `Analyzed` at +72 s | 45 s; `Analyzed` at +16 s |
| | test job wall clock, warm | ~250 s | ~144 s |

The container pull (`Initialize containers`) is 55–90 s and is the
largest fixed cost left. Dropping the archives from the image brought
it back to within 7 s of the old, smaller image.

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
- **Rebuilding the image on every lockfile change.** 67 builds in a
  month at ~12 min and ~2.5 GB each; a stale pre-fetch costs only the
  delta.

## Next levers, in the order they are worth trying

1. **BuildBuddy remote execution** (`--config=remote`): −34% on a cold
   run, measured in #324, left dispatch-only pending the free tier's
   cache-transfer quota — which #497 and #499 have since cut by most
   of what a warm run moved.
2. **A warm runner.** GitHub-hosted runners are fresh VMs, so the
   image pull (55–90 s) and the analysis (16 s) are paid every job.
   BuildBuddy's hosted CI runners keep the bazel server and its caches
   between runs, which removes both; self-hosted runners do the same
   at the cost of a machine. Either is a cost decision, not a config
   change.
3. **zstd-compressed image layers** — pulls decompress faster; cheap
   to try in `devcontainer.yml`.
4. **Blast radius** — the only lever on a cold run, and a design
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
