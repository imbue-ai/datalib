# Doing a release

This is the playbook for cutting a new `frankweiler` release: **bump → test →
commit → tag → push**. Pushing the `vX.Y.Z` tag is what kicks off the CI
release (`.github/workflows/release.yml`) — it builds per-platform tarballs,
attaches them to a GitHub Release, and publishes the docker + devcontainer
images to ghcr.io. Everything before the tag push is local and reversible.

We use [SemVer](https://semver.org/) `vMAJOR.MINOR.PATCH`. Pick the bump:
patch for fixes, **minor** for new features / the usual cadence, major for
breaking changes.

## 0. Start clean

```sh
git checkout main
git pull
git status   # must be clean before you start
```

## 1. Bump the version

There are **two** source-of-truth files that must agree, plus two lock files
that follow. The release CI and a local bazel test both assert they match, so
a mismatch fails fast rather than shipping a mislabeled binary.

Say we're going `0.13.0 → 0.14.0`.

1. **`frankweiler/backend/Cargo.toml`** — `[workspace.package].version`. This
   is the canonical version; all ~33 workspace crates inherit it via
   `version.workspace = true`.

   ```toml
   [workspace.package]
   version = "0.14.0"
   ```

2. **`frankweiler/backend/sync/BUILD.bazel`** — the `version = "…"` attr on the
   sync binary (Bazel is the sole build, so this is hand-maintained).
   `//frankweiler/backend:version_consistency_test` asserts it equals the
   Cargo.toml version.

   ```python
   version = "0.14.0",
   ```

3. **`frankweiler/backend/Cargo.lock`** — bump the version of **our workspace
   crates only**. `//frankweiler/backend:cargo_lock_versions_test` asserts they
   match the workspace version.

   > ⚠️ **Gotcha:** do **not** blanket-replace every `version = "0.13.0"`.
   > Third-party deps can sit at the same version as ours (e.g. `itertools`
   > was at `0.13.0`), and a blanket sed bumps them too — creating a
   > duplicate (`package itertools is specified twice in the lockfile`) that
   > breaks the next crate_universe repin. Match on the crate **name**:

   ```sh
   awk '
     /^name = / {name=$3}
     /^version = "0.13.0"$/ && (name ~ /^"frankweiler-/ || name == "\"app-schema\"") \
       {print "version = \"0.14.0\""; next}
     {print}
   ' frankweiler/backend/Cargo.lock > /tmp/lock && mv /tmp/lock frankweiler/backend/Cargo.lock
   ```

   (The workspace crates are `frankweiler-*` plus `app-schema`. Sanity-check
   afterward: `grep -A1 'name = "itertools"' Cargo.lock` should still show its
   original distinct versions.)

4. **`MODULE.bazel.lock`** — don't edit by hand. The `bazel test` in the next
   step triggers a crate_universe repin that updates its Cargo.lock/Cargo.toml
   checksums for you.

## 2. Test

```sh
bazel test //...
```

Must be fully green. In particular this runs `version_consistency_test`
(tag-less: Cargo.toml == BUILD.bazel) and `cargo_lock_versions_test` (workspace
crates == workspace version). If a crate_universe repin error shows up here,
re-read the Cargo.lock gotcha above — that's almost always the cause.

## 3. Commit

Commit the four files together (mirrors the previous release commit):

```sh
git add MODULE.bazel.lock \
        frankweiler/backend/Cargo.lock \
        frankweiler/backend/Cargo.toml \
        frankweiler/backend/sync/BUILD.bazel
git commit -m "chore(release): bump version 0.13.0 → 0.14.0"
```

## 4. Tag

Annotated tag, `vX.Y.Z`, message `Release vX.Y.Z` (matches every prior tag):

```sh
git tag -a v0.14.0 -m "Release v0.14.0"
```

## 5. Push

Push the commit first, then the tag. The tag push is the trigger — only do it
once `main` is up:

```sh
git push origin main
git push origin v0.14.0
```

## 6. Watch the release CI

The tag push starts `.github/workflows/release.yml`. It:

- re-asserts the tag matches `Cargo.toml` (defense against tagging the wrong
  commit),
- builds `//frankweiler/backend:dist` stamped (`--config=release`) for
  aarch64-darwin, x86_64-linux, aarch64-linux, and asserts each binary's
  `--version` prints exactly the tag,
- publishes per-triple tarballs (`frankweiler-<triple>.tar.gz` + `.sha256`) to
  the GitHub Release,
- builds + pushes the multi-arch prod image and the devcontainer image to
  ghcr.io.

```sh
gh run watch "$(gh run list --workflow=release.yml --limit 1 --json databaseId --jq '.[0].databaseId')"
gh release view v0.14.0
```

## If something goes wrong

- **CI fails the `Assert tag matches Cargo.toml` step** — you tagged before
  bumping, or tagged the wrong commit. Delete and recreate the tag on the
  right commit:

  ```sh
  git push origin :refs/tags/v0.14.0   # delete remote tag
  git tag -d v0.14.0                   # delete local tag
  # fix the version commit, then re-tag + re-push
  ```

- **Binary reports `dev` / wrong version** — build stamping was off; check
  `.bazelrc`'s `build:release --stamp` and that the dist build used
  `--config=release`. (This is exactly what the post-build assertion guards.)

- **A single platform leg fails** — `fail-fast: false`, so the other tarballs
  still publish. Investigate the broken triple and, if needed, re-run just that
  job from the Actions UI; the release already exists, so a re-run attaches the
  missing tarball.
