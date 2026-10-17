---
name: release
description: Cut a new datalib release — bump the workspace version, repin lockfiles, run the consistency tests, land the bump on main through a PR, tag vX.Y.Z (which triggers release.yml), watch the release publish, and bump the pin in imbue-ai/default-workspace-template (DWT). Use when asked to "make a new release", "cut a release", or "bump the version".
---

# Release

A release is cut by pushing a `v*` tag. `.github/workflows/release.yml`
then builds the per-triple dist tarballs (incl. fully-static musl
variants), the signed + notarized macOS .app/dmg (Tauri), and the
docker images, and attaches everything to a GitHub Release. Nothing is
published from a local machine — the tag is the trigger.

## Versioning

- Single source of truth: `[package].version` in
  `datalib/backend/Cargo.toml`.
- Must match the `version = "..."` fields in
  `datalib/backend/dag/BUILD.bazel`,
  `datalib/backend/http/BUILD.bazel` and
  `datalib/backend/runtime/BUILD.bazel` — asserted by
  `//datalib/backend:version_consistency_test`, which names the
  offending file on failure. If that test's `data` list has grown, bump
  every file it checks.
- The git tag is `vX.Y.Z` with the same number. Minor bump for
  feature releases, patch for fix-only ones — and **a change that
  moves any store's shape is a minor bump** (the `schema_inventory`
  golden, a `RENDER_VERSION`, `LAYOUT_VERSION` or
  `datalib_runs::SCHEMA_VERSION`): the downgrade guard lets a newer
  patch open a store and refuses a newer minor
  (`docs/dev/release_steps.md`).
- `datalib/tauri/tauri.conf.json`'s `"version"` is the desktop
  app's own version and is **not** part of this procedure.
- One repo outside this one pins the released version:
  `imbue-ai/default-workspace-template` (DWT), the Minds workspace
  template. Its `datalib` skill installs the musl binaries from a tag on
  first use and reads that tag's `docs/agent_user.md`. It is bumped
  *after* the release publishes (step 12), not with it.

## Procedure

0. Run the release's own steps on Linux before anything is bumped —
   `release.yml` runs the tag's tree, so a bug in a step costs a
   release. A green step 0 is only as good as what the test's tree has
   in it (`tools/check_stage_tarball.sh` stages a read-only `git-hash`
   because the release overwrites one):

   ```sh
   bazelisk run //tools:release_steps_docker
   ```

   It runs `//tools:stage_runtime_test` and `//tools:stage_tarball_test`
   in the devcontainer; the first run builds the image and the Linux
   binaries cold (the better part of an hour), later ones take minutes.
   `docs/dev/release_steps.md` says what it covers and what stays
   release-only. A mac's `bazel test //...` has already run the same
   tests under macOS and under `/bin/bash` 3.2.
1. Start from a clean, current main:
   `git fetch origin && git checkout -b release-vX.Y.Z origin/main`.
2. Pick the version by reviewing what's shipping:
   `git log --first-parent v<last>..origin/main --oneline` (find `<last>` with
   `git tag | sort -V | tail -1` — fetch tags first).
3. Bump all four version fields: `Cargo.toml` and the three
   `BUILD.bazel`. Then record the raw-store shapes this release leaves
   behind, so later builds are tested migrating them: run
   `bazel run //datalib/backend/datalib_step:raw_shapes.update`, and if
   `datalib/backend/datalib_step/raw_shapes/current.json` differs from
   the newest `<version>.json` beside it, copy it to `X.Y.Z.json`
   (`raw_shapes/README.md`).
4. Run `tools/repin_cargo.sh` to refresh
   `datalib/backend/Cargo.lock`. Do **not** rely on
   `CARGO_BAZEL_REPIN=1 bazel test //...` for this — when every target
   cache-hits, the repin never runs and Cargo.lock silently stays at
   the old version (the script's header comment tells the war story).
5. Refresh the tauri lockfile:
   `(cd datalib/tauri && cargo metadata --format-version=1 >/dev/null)`.
   Usually a no-op: the shell depends on no backend crate.
6. Verify:
   `CARGO_BAZEL_REPIN=1 bazel test //datalib/backend:version_consistency_test //datalib/backend:cargo_lock_versions_test`.
   (A one-off "FAILED TO BUILD" from bazel's test-xml generator is a
   known local flake — rerun before believing it.)
   Then refresh `MODULE.bazel.lock`. It records content hashes of
   `datalib/backend/Cargo.{toml,lock}`, which steps 3–4 just changed —
   but **the two tests above will not update it**. Both are shell tests,
   so nothing in that invocation needs the `crate_universe` extension,
   and the stale hashes sit there behind a green result. Build one Rust
   target to force the re-resolve, then check explicitly rather than
   waiting to notice it in the diff:

   ```sh
   CARGO_BAZEL_REPIN=1 bazelisk build //datalib/backend/table:datalib_table
   bazelisk build //datalib/backend/table:datalib_table   # again, WITHOUT the env var
   git status --porcelain MODULE.bazel.lock
   ```

   The second build is not redundant. The lock records the *value* of
   `CARGO_BAZEL_REPIN` as one of its facts, so a lock written by the
   repin run says `1` and every ordinary build afterwards rewrites it to
   `\0` — leaving a dirty file in every developer's tree and tripping
   `lint_repo`'s check 3. Commit the state a plain build produces, which
   is the one almost every build will produce.

   A lock left in the `1` state is worse than a dirty file: CI runs
   `--lockfile_mode=error`, so it aborts during module resolution before
   a single test, with *"environment variable CARGO_BAZEL_REPIN changed:
   '1' -> <unset>"*. That reds `main` and every open PR with it, since a
   PR is tested as its branch merged into `main`.

   To repair one already pushed, run the plain build above — it rewrites
   the one line. **Not** bazel's suggested
   `bazel mod deps --lockfile_mode=update`: here that aborts on an
   unrelated transitive module (*"crate_universe extension call `crates`
   is in a non-root module but has no lockfile"*) after part-writing the
   file. Confirm the way CI will:

   ```sh
   bazelisk build --lockfile_mode=error //datalib/backend/table:datalib_table
   ```

   Don't be alarmed by its size: it is a **3-line** diff that prints as
   ~360 KB, because one of those lines is a 17 KB single-line generated
   blob. That is also why it falls straight through a targeted `git add`.
   A miss is not silent — CI runs `--lockfile_mode=error` and
   `scripts/lint_repo.py`'s check 3 fails locally — but neither of those
   is the version test you just ran, so stage it whenever that command
   prints anything.
7. Commit as `chore(release): bump version X.Y.Z → X.Y'.Z'` with a
   short summary of what the release carries (see commits `835946a9`
   and `c05fa424` for the shape). Expected files: `Cargo.toml`,
   `Cargo.lock`, the three `BUILD.bazel`, a new
   `raw_shapes/X.Y.Z.json` when step 3 made one, and
   `MODULE.bazel.lock` (per step 6 — expect it, don't treat it as a
   surprise), plus possibly `datalib/tauri/Cargo.lock`.
   Sanity-check before pushing: re-run step 6's *second* (env-var-free)
   build and confirm `git status --porcelain MODULE.bazel.lock` is now
   empty. If it still isn't, the lock didn't converge and the next
   person to build inherits the dirty file.
8. Land the bump on main through a PR. `main` carries a ruleset ("CI
   green before merge") that requires the `bazel test //...` and
   `bazel build :dist (musl static)` checks, so a direct
   `git push origin release-vX.Y.Z:main` is rejected with `GH013`:

   ```sh
   git push -u origin release-vX.Y.Z
   gh pr create --base main --head release-vX.Y.Z \
     --title "chore(release): bump version X.Y.Z → X.Y'.Z'"
   gh pr checks <n> --watch --fail-fast
   gh pr merge <n> --merge
   ```

   The PR lands as a merge commit (that is how every PR here lands),
   so the commit to tag is that merge on `main`, not the bump commit
   you pushed.
9. Tag the merge commit on main and push the tag:
   `git fetch origin && git tag vX.Y.Z origin/main && git push origin vX.Y.Z`.
   Check first that `origin/main` is that commit and nothing landed on
   top of it (`git log --oneline -3 origin/main`); tag it by sha if
   something did.
10. Watch the workflow to completion:
    `gh run list --workflow=release.yml --limit 1`, then
    `gh run watch <run-id> --exit-status`. It's slow (multi-platform
    matrix + notarization). `fail-fast` is off, so one broken leg
    doesn't cancel the others — a partial release can be repaired by
    re-running just the failed job.
11. Confirm the artifacts landed: `gh release view vX.Y.Z` should list
    the per-triple tarballs and the macOS dmg. Tarball filenames are
    stable/un-versioned on purpose — the install script fetches
    `releases/latest/download/<name>`.
12. Bump the pin in DWT, `imbue-ai/default-workspace-template`
    (§"Updating DWT"). Only after step 11 — every pin there is a
    tag-relative URL that has to resolve when a workspace first uses
    datalib. This step is the one that gets dropped, so start by checking
    what DWT actually pins rather than assuming it is one behind.

## Updating DWT

`imbue-ai/default-workspace-template` is the template every Minds
workspace starts from. Nothing of datalib is installed at boot: on first
use its `datalib` skill runs `scripts/install_datalib.sh`, which
downloads the pinned release's musl build into
`~/.local/share/datalib/<version>/` and saves that tag's
`docs/agent_user.md` beside it, so the tools an agent has and the guide
it reads cannot drift apart.

1. Get a current clone. Don't assume one is already on the machine:

   ```sh
   gh repo clone imbue-ai/default-workspace-template   # or, in an existing clone:
   git fetch origin && git checkout -b bump-datalib-vX.Y.Z origin/main
   ```
2. Replace the `v<old>` datalib pin with `vX.Y.Z` in exactly two files:
   `.agents/skills/datalib/scripts/install_datalib.sh` (`PINNED_VERSION`,
   the one place the binaries and the guide are fetched from) and
   `.agents/skills/datalib/SKILL.md` (the "pinned to datalib v..." prose,
   the local guide's path and the docs link).
   `system/apps/datalib/test_datalib_pin.py` checks that the skill names
   no version but the one the script installs. Find the pins by grepping
   for the literal old version:

   ```sh
   git grep -n v<old> -- ':!.agents/changelog' ':!system/changelog'
   ```

   Anything it prints outside those two files is either a new pin (bump
   it, and add the file to this list) or history. The notes under
   `.agents/changelog/` and `system/changelog/` are history: a `v<old>`
   there records what that change did.
3. Read the release's commits against the skill's prose and fix what
   went stale — the pin is not the whole contract. The skill describes
   real behaviour (which sources work and how one is spelled in the
   config, where the store lives), so a change to any of that lands here
   even when nothing about the install moved.
4. Add the branch's note, `.agents/changelog/<branch>.md`: one paragraph
   saying which datalib version the skill now installs and what it
   brings that an agent or a person would notice.
5. Run the pin test (`uv run pytest system/apps/datalib/test_datalib_pin.py`),
   commit as `datalib: bump the pinned version to vX.Y.Z`, push the
   branch and open the PR against `main`:

   ```sh
   git push -u origin bump-datalib-vX.Y.Z
   gh pr create --repo imbue-ai/default-workspace-template --base main \
     --head bump-datalib-vX.Y.Z
   ```

   Check that the previous bump's PR was merged before assuming the pin
   moved, and close a superseded one when you open its successor.
