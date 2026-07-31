---
name: release
description: Cut a new datalib release — bump the workspace version, repin lockfiles, run the consistency tests, push to main, tag vX.Y.Z (which triggers release.yml), and watch the release publish. Use when asked to "make a new release", "cut a release", or "bump the version".
---

# Release

A release is cut by pushing a `v*` tag. `.github/workflows/release.yml`
then builds the per-triple dist tarballs (incl. fully-static musl
variants), the signed + notarized macOS .app/dmg (Tauri), and the
docker images, and attaches everything to a GitHub Release. Nothing is
published from a local machine — the tag is the trigger.

## Versioning

- Single source of truth: `[workspace.package].version` in
  `datalib/backend/Cargo.toml`.
- Must match the `version = "..."` fields in
  `datalib/backend/dag/BUILD.bazel` and
  `datalib/backend/http/BUILD.bazel`, and `ARG PROD_IMAGE_TAG` in
  `.devcontainer/Dockerfile` — asserted by
  `//datalib/backend:version_consistency_test`, which names the
  offending file on failure. If that test's `data` list has grown, bump
  every file it checks.
- `ARG PROD_IMAGE_TAG` selects the prod image a LOCAL devcontainer
  builds FROM, so between this bump and `release.yml` publishing the new
  tag it points at an image that does not exist yet. That window closes
  when the tag build finishes.
- The git tag is `vX.Y.Z` with the same number. Minor bump for
  feature releases, patch for fix-only ones.
- `datalib/tauri/tauri.conf.json`'s `"version"` is the desktop
  app's own version and is **not** part of this procedure.

## Procedure

1. Start from a clean, current main:
   `git fetch origin && git checkout -b release-vX.Y.Z origin/main`.
2. Pick the version by reviewing what's shipping:
   `git log v<last>..origin/main --oneline` (find `<last>` with
   `git tag | sort -V | tail -1` — fetch tags first).
3. Bump all three version fields (Cargo.toml + the two BUILD.bazel).
4. Run `tools/repin_cargo.sh` to refresh
   `datalib/backend/Cargo.lock`. Do **not** rely on
   `CARGO_BAZEL_REPIN=1 bazel test //...` for this — when every target
   cache-hits, the repin never runs and Cargo.lock silently stays at
   the old version (the script's header comment tells the war story).
5. Refresh the tauri lockfile:
   `(cd datalib/tauri && cargo metadata --format-version=1 >/dev/null)`.
   Usually a no-op since the shell stopped depending on backend crates.
6. Verify:
   `CARGO_BAZEL_REPIN=1 bazel test //datalib/backend:version_consistency_test //datalib/backend:cargo_lock_versions_test`.
   If `MODULE.bazel.lock` changed under you, commit it too. (A one-off
   "FAILED TO BUILD" from bazel's test-xml generator is a known local
   flake — rerun before believing it.)
7. Commit as `chore(release): bump version X.Y.Z → X.Y'.Z'` with a
   short summary of what the release carries (see commits `835946a9`
   and `c05fa424` for the shape). Expected files: `Cargo.toml`,
   `Cargo.lock`, the two `BUILD.bazel`, plus possibly
   `datalib/tauri/Cargo.lock` and `MODULE.bazel.lock`.
8. Push the bump straight to main (release bumps land directly, not
   via PR): `git push origin release-vX.Y.Z:main`.
9. Tag that commit and push the tag:
   `git tag vX.Y.Z <commit> && git push origin vX.Y.Z`.
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
