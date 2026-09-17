# CI: reading a run, the BuildBuddy cache, flaky tests

## "Why was CI slow?" — read the BuildBuddy invocation

Both `test.yml` jobs post to our BuildBuddy org at `imbue.buildbuddy.io`
(configured by `.github/actions/prepare-bazel`; `--config=buildbuddy` in
`.bazelrc` turns it on). **On `main` the whole build is essentially a
cache replay**, so any run that takes noticeably longer is telling you
what it had to rebuild.

Every bazel invocation prints its own dashboard link. Pull it out of the
job log:

```bash
gh api "repos/imbue-ai/datalib/actions/runs/<run-id>/jobs" \
  --jq '.jobs[] | select(.name=="bazel test //...") | .id'
gh api "repos/imbue-ai/datalib/actions/jobs/<job-id>/logs" > /tmp/ci.log
grep -oE 'https://imbue\.buildbuddy\.io/invocation/[a-f0-9-]+' /tmp/ci.log | sort -u
```

The log is only served **after the job finishes**; while it runs the API
says "still in progress".

Three lines answer "what work was actually done":

```bash
grep -E 'INFO: Elapsed time|processes:|Executed [0-9]+ out of' /tmp/ci.log
```

- `N processes: A remote cache hit, B internal, C local, D
  processwrapper-sandbox` — **`D` is the real signal.** Sandboxed
  actions actually compiled or ran; cache hits and `internal` are free.
- `Executed N out of M tests` — on a warm `main` this is **0**.
- `Critical Path` — the serial floor.

| run | elapsed | critical path | sandboxed | tests executed |
|---|---|---|---|---|
| a warm `main` | 175s | 46s | 0 | 0 of 114 |
| a one-provider PR | 142s | 74s | 27 | 8 of 114 |
| a PR touching shared `datalib_etl` | 1016s | 310s | 322 | 59 of 115 |

The cause is blast radius, and you can measure it before pushing —
`rdeps` says how much of the tree a crate is upstream of:

```bash
bazelisk query 'kind(".*_test", rdeps(//..., //datalib/backend/etl:datalib_etl))'      # ~80 test targets
bazelisk query 'kind(".*_test", rdeps(//..., //datalib/backend/schema:datalib_schema))'  # ~two thirds of the suite
bazelisk query 'kind(".*_test", rdeps(//..., //tests/fixtures:ingested_tng))'             # 3, incl. the e2e suite
```

A small helper added to a widely-linked crate is a one-time cost, not a
regression: once on `main` the cache is warm. It is worth knowing so
you can decide deliberately whether it belongs in a shared crate — the
`rdeps` number is the price tag.

**Runs are bimodal.** A warm run executes 0 tests in ~3 min; a cold one
rebuilds ~345 actions in ~20, with almost nothing between. A rising
median means cold runs got more frequent, not that anything got slower.

**A `[for tool]` suffix on a `Compiling Rust …` line is a second copy**:
the crate is built in the exec configuration as well as the target one.
A `genrule` puts its `tools` there, so a pipeline binary a genrule runs
goes in `srcs`, not `tools` (see the comment on
`//tests/fixtures:ingested_tng`). The only exec-config Rustc actions
should be the dependency-free `qmd_indexer` chain.

**A `pull_request` run builds the merge of the PR into `main` as it is
at that moment**, not the branch head, so when `main` moves the PR's
next run re-executes whatever is downstream of what `main` changed. A
`workflow_dispatch` run builds the bare branch head.

A run can also be slow without compiling anything — check whether the
job *started* late (`created_at` vs `started_at`) before reading any of
the numbers above. That is runner queueing.

`--config=remote` (`.bazelrc`) sends compiles to BuildBuddy remote
execution instead of the runner's 4 vCPUs; `test.yml` takes it via a
`remote_execution` dispatch input. It is a trial switch, not the merge
gate (#324).

## Locally you are probably *not* on the remote cache

- `.bazelrc` gives everyone a machine-wide disk cache
  (`--disk_cache=~/Library/Caches/bazel-disk-cache`), shared by every
  checkout and worktree. It only holds what *you* built; nothing CI
  built lands in it. Size its cap against how many worktrees you keep
  live (`du -sh ~/Library/Caches/bazel-disk-cache`; sitting *at* the cap
  is the symptom).
- The remote cache needs `.bazelrc.user`, which is gitignored and
  **per-workspace**: `try-import %workspace%/.bazelrc.user` resolves to
  the worktree root, so a file in the main checkout is invisible to every
  `.claude/worktrees/*` clone.

```bash
grep -c buildbuddy .bazelrc.user 2>/dev/null || echo "no .bazelrc.user in THIS workspace"
```

Even with the key, a mac shares almost nothing with CI: an action's
cache key covers its toolchain, so a darwin-arm64 rustc action and CI's
linux-x86_64 one are different actions. Locally the remote cache buys
sharing with your own other worktrees and machines. For the same reason
the `remote` config is CI-only: driving Linux executors from a mac hands
them a darwin toolchain.

The `processes:` line settles it either way: a run on the remote cache
names `remote cache hit`; a local run without `.bazelrc.user` never does.

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

Don't put `build --config=buildbuddy` in `$HOME/.bazelrc`: the config is
only defined in this repo's `.bazelrc`, and unrelated projects would fail
with "Config value 'buildbuddy' is not defined".

## "Which tests are flaky?" — read the reruns

Hitting "re-run failed jobs" replays the same commit, so a commit that
carries both a failure and a success flaked. `scripts/flaky_tests.py`
groups runs by commit, keeps the mixed ones, and reads the failed
attempt's log for bazel's `FAILED` summary:

```bash
scripts/flaky_tests.py --limit 400
```

It only sees flakes somebody actually re-ran, and GitHub deletes run
logs after 90 days.
