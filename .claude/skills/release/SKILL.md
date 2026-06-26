---
name: release
description: Cut a new frankweiler release — bump the version, run the bazel test suite, commit, tag, and push (the tag push triggers the GitHub release CI). Use when the user asks to "do a release", "cut a release", "bump the version and release", "ship vX.Y.Z", or "release a new minor/patch/major".
user-invocable: true
allowed-tools: Bash, Read, Edit, Grep, Glob
argument-hint: "[patch | minor | major | X.Y.Z]"
---

## Release frankweiler

Drive the release flow: **bump → test → commit → tag → push**. Pushing the
`vX.Y.Z` tag triggers `.github/workflows/release.yml`, which builds per-platform
tarballs + docker images. Everything before the tag push is local and
reversible; the tag push is the irreversible, outward-facing step.

The canonical prose version of this lives in `docs/dev/releasing.md` — read it
if anything here is ambiguous. This skill is the executable form.

### Step 0 — Preconditions

```sh
git checkout main && git pull
git status --short        # MUST be clean; if not, stop and ask the user
```

If the working tree isn't clean, stop and ask the user how to proceed — do not
bundle stray changes into a release.

### Step 1 — Determine the new version

Read the current version:

```sh
grep -E '^version = "' frankweiler/backend/Cargo.toml   # under [workspace.package]
```

Parse `$ARGUMENTS`:
- `patch` → bump Z (`0.13.0` → `0.13.1`)
- `minor` → bump Y, reset Z (`0.13.0` → `0.14.0`)
- `major` → bump X, reset Y and Z (`0.13.0` → `1.0.0`)
- an explicit `X.Y.Z` → use it verbatim
- empty → ask the user which bump (default suggestion: **minor**)

Call the current version `$OLD` and the new one `$NEW` below.

### Step 2 — Bump the version (4 files)

Two source-of-truth files must agree; two lock files follow.

1. **`frankweiler/backend/Cargo.toml`** — `[workspace.package].version` → `$NEW`.
2. **`frankweiler/backend/sync/BUILD.bazel`** — the `version = "…"` attr → `$NEW`
   (`version_consistency_test` asserts it equals Cargo.toml).
3. **`frankweiler/backend/Cargo.lock`** — bump **only our workspace crates**.

   > ⚠️ Do NOT blanket-replace `version = "$OLD"`. Third-party deps can sit at
   > the same version (e.g. `itertools` was at `0.13.0`); bumping them creates a
   > duplicate that breaks the next crate_universe repin
   > (`package <x> is specified twice in the lockfile`). Match on crate **name**
   > (`frankweiler-*` and `app-schema`):

   ```sh
   awk -v old="$OLD" -v new="$NEW" '
     /^name = / {name=$3}
     $0 == "version = \"" old "\"" && (name ~ /^"frankweiler-/ || name == "\"app-schema\"") \
       {print "version = \"" new "\""; next}
     {print}
   ' frankweiler/backend/Cargo.lock > /tmp/lock && mv /tmp/lock frankweiler/backend/Cargo.lock
   ```

   Sanity check: `grep -A1 'name = "itertools"' frankweiler/backend/Cargo.lock`
   should still show its original distinct versions (not two of `$NEW`).
4. **`MODULE.bazel.lock`** — leave it; the bazel test in Step 3 repins it.

### Step 3 — Test

```sh
bazel test //...
```

Redirect to a file (don't pipe through `head`/`tail`) and confirm it's fully
green, especially `version_consistency_test` and `cargo_lock_versions_test`. A
crate_universe repin error here almost always means the Cargo.lock gotcha above
— fix and re-run. Do not proceed until green.

### Step 4 — Commit

```sh
git add MODULE.bazel.lock \
        frankweiler/backend/Cargo.lock \
        frankweiler/backend/Cargo.toml \
        frankweiler/backend/sync/BUILD.bazel
git commit -m "chore(release): bump version $OLD → $NEW"
```

(Include the repo's standard commit trailers.)

### Step 5 — Tag

Annotated, `vX.Y.Z`, message `Release vX.Y.Z`:

```sh
git tag -a "v$NEW" -m "Release v$NEW"
```

### Step 6 — CONFIRM, then push

**Stop and confirm with the user before pushing** — the tag push publishes a
real release (tarballs + docker images) and is not cleanly reversible. Show them
the commit and tag you're about to push. Once they approve:

```sh
git push origin main
git push origin "v$NEW"
```

### Step 7 — Report

Point the user at the running release workflow:

```sh
gh run list --workflow=release.yml --limit 1
```

Tell them it builds the dist tarballs + docker/devcontainer images, and they can
watch it with `gh run watch <id>` or `gh release view v$NEW`.

### If something goes wrong

- **CI's "Assert tag matches Cargo.toml" fails** — tagged the wrong commit or
  before bumping. Delete and recreate:
  `git push origin :refs/tags/v$NEW && git tag -d v$NEW`, fix, re-tag, re-push.
- **Binary reports `dev`/wrong version** — stamping was off; see
  `.bazelrc` `build:release --stamp`.
- **One platform leg fails** — `fail-fast: false`, so other tarballs still
  publish; re-run just the broken job from the Actions UI.
