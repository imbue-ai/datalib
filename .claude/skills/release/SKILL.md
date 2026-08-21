---
name: release
description: Cut a new datalib release — bump the workspace version, repin lockfiles, run the consistency tests, push to main, tag vX.Y.Z (which triggers release.yml), watch the release publish, and bump the pin in the qi-imbue/datalib-inspiration repo. Use when asked to "make a new release", "cut a release", or "bump the version".
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
- One repo outside this one pins the released version:
  `qi-imbue/datalib-inspiration`, the published Minds inspiration,
  which installs the musl binaries from a tag and links agents at that
  tag's `docs/agent_user.md`. It is bumped *after* the release
  publishes (step 12), not with it.

## Procedure

1. Start from a clean, current main:
   `git fetch origin && git checkout -b release-vX.Y.Z origin/main`.
2. Pick the version by reviewing what's shipping:
   `git log v<last>..origin/main --oneline` (find `<last>` with
   `git tag | sort -V | tail -1` — fetch tags first).
3. Bump all four version fields: `Cargo.toml`, the two `BUILD.bazel`,
   and `ARG PROD_IMAGE_TAG` in `.devcontainer/Dockerfile`.
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
   (A one-off "FAILED TO BUILD" from bazel's test-xml generator is a
   known local flake — rerun before believing it.)
   Then check `MODULE.bazel.lock` **explicitly** — don't wait to notice
   it in the diff:
   `git status --porcelain MODULE.bazel.lock`. That file records content
   hashes of `datalib/backend/Cargo.{toml,lock}`, which steps 3–4 just
   changed, so the bazel run above rewrites it. Don't be alarmed by its
   size: it is a **3-line** diff that prints as ~360 KB, because one of
   those lines is a 17 KB single-line generated blob. That is also why it
   falls straight through a targeted `git add` — the last four release
   bumps (`d5f2aa47`, `75abc2a7`, `786b628f`, `71c315d4`) all shipped
   without it, leaving the lock stuck on the v0.25.0 state until it was
   refreshed wholesale. Nothing breaks when it's missed (`lockfile_mode`
   is unset, so bazel's default is to re-resolve and rewrite silently
   rather than fail) — which is exactly why four releases passed without
   anyone noticing, and why this is a step rather than a test. Stage it
   whenever that command prints anything.
7. Commit as `chore(release): bump version X.Y.Z → X.Y'.Z'` with a
   short summary of what the release carries (see commits `835946a9`
   and `c05fa424` for the shape). Expected files: `Cargo.toml`,
   `Cargo.lock`, the two `BUILD.bazel`, `.devcontainer/Dockerfile`, and
   `MODULE.bazel.lock` (per step 6 — expect it, don't treat it as a
   surprise), plus possibly `datalib/tauri/Cargo.lock`.
   Sanity-check before pushing: re-run the step 6 bazel command and
   confirm `git status --porcelain MODULE.bazel.lock` is now empty. If
   it still isn't, the lock didn't converge and the next person to build
   inherits the dirty file.
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
12. Bump the pin in the published inspiration repo,
    `qi-imbue/datalib-inspiration`. Only after step 11 — every pin there
    is a tag-relative URL that has to resolve for a fresh mind to boot.

## Updating the inspiration repo

`qi-imbue/datalib-inspiration` is a bootable snapshot of a Minds agent
that mirrors your data with datalib. Its `datalib` skill installs the
fully-static musl binaries from a pinned tag and sends the agent to that
same tag's `docs/agent_user.md` — deliberately, so the tools an agent has
and the guide it reads can't drift apart.

1. Get a current clone. Don't assume one is already on the machine —
   this step used to name `~/on/datalib-inspiration`, a path that
   existed only for whoever wrote it (#166):

   ```sh
   gh repo clone qi-imbue/datalib-inspiration   # or, in an existing clone:
   git checkout main && git pull
   ```
2. Replace every `v<old>` datalib pin with `vX.Y.Z` in exactly three
   files — `.agents/skills/datalib/SKILL.md`, `README.md`, and
   `inspiration-datalib.md`. That covers the `install.sh` raw URL, the
   `DATALIB_VERSION` env var, the `docs/agent_user.md` links (including
   the relative-link base), and the "pinned to datalib v..." prose. Grep
   the three files for the literal old version — `grep -n v<old> README.md
   inspiration-datalib.md .agents/skills/datalib/SKILL.md` — rather than
   for a URL shape; the pins are spelled several different ways.
3. Leave two things alone:
   - `system/vendor/mngr/**`, which is vendored from mngr. Its
     `DATALIB_CURL_VERSION` pins the datalib *curl* release the latchkey
     gateway ships and moves on mngr's own cadence, not this one.
   - "as of datalib v..." capability notes (e.g. which providers work
     inside Minds). Those record when a fact became true and are only
     touched when the fact changes.
4. Read the release's commits against the inspiration's prose and fix
   what went stale — the pin is not the whole contract. The two files
   describe real behaviour (which sources work and under what
   conditions, where the store lives, what needs a recent Minds app), so
   a change to any of that lands here even when nothing about the
   install or the config format moved.
5. Commit as `datalib inspiration: bump pinned version to vX.Y.Z` and
   push straight to `main`. The repo is unprotected and these land
   directly, no PR.
