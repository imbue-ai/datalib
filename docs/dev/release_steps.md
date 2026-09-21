# The release's steps, run before a tag runs them

**Reference, current as of 2026-09-21.** The code is
[`scripts/release/`](../../scripts/release/), the tests are in
[`tools/BUILD.bazel`](../../tools/BUILD.bazel) under "release.yml's
steps", and the workflow is
[`.github/workflows/release.yml`](../../.github/workflows/release.yml).

`release.yml` runs only when a `v*` tag is pushed, and a tag's run
executes the tag's own tree — a re-run cannot pick up a fix. So a step
that is wrong the first time it runs costs a release: v0.35.0 lost its
tarballs to three bugs in the `runtime` job, v0.35.1 to one in the
"Stage tarball" step, and none of the four was visible to `bazel test
//...` because each lived in an inline `run:` block, or in a script
only that block called in that way. This page says what now runs
before a tag and what still cannot.

## A patch release never changes a store's shape

The downgrade guard (`datalib_store_meta::guard`,
[`plans/completed/schema_migrations.md`](plans/completed/schema_migrations.md) §3.4)
compares versions by `major.minor`: a build refuses a store a newer
*minor* wrote and opens one a newer *patch* wrote. That is only safe if
a patch release never adds, renames or drops a column, table, cursor
shape or `RENDER_VERSION`. So: a change that moves the
`schema_inventory` golden, a `RENDER_VERSION`, `LAYOUT_VERSION` or
`datalib_runs::SCHEMA_VERSION` is a minor bump, whatever else is in it.

## The steps are scripts

The two steps that assemble what a release ships are scripts, and the
workflow's `run:` blocks are one line each:

| step | script | what it makes |
|---|---|---|
| `runtime` job, "Stage the runtime" | `scripts/release/stage_runtime_asset.sh <triple> <cuda>` | `runtime-<triple>.tar.gz` (+ `-cuda`) and `.sha256` sidecars, from `scripts/stage_runtime.sh` |
| `build` job, "Stage tarball" | `scripts/release/stage_tarball.sh <ref> <triple>` | `datalib-<triple>.tar.gz` and its sidecar: the `//datalib/backend:bin` binaries, the `latchkey` launcher, `git-hash`, `runtime.manifest`, `licenses/` |

Each script writes into the current directory, prints what it wrote,
and takes what a test cannot supply through environment variables
(the Bazel outputs as a runfiles tree, the runtime assets as local
files, the commit, a stand-in for `cargo-about`). The header of each
script lists them. A script calls another through `"$BASH"`, so the
bash that runs the outer one runs the inner one too.

The publish steps — `gh release create` / `gh release upload` — stay
in the workflow. They are the part that cannot run anywhere else.

## What runs on every `bazel test //...`

- **`//tools:stage_runtime_test`** runs `stage_runtime_asset.sh` from
  a directory that is not the repo, with the relative `runtime` the
  workflow passes, then unpacks the asset and runs qmd out of it. On
  Linux CI this is where node-llama-cpp's fork probe runs — the
  `--input-type=module` bug of v0.35.0 fails here.
- **`//tools:stage_tarball_test`** runs `stage_tarball.sh` for four
  triples (gnu with the CUDA overlay, the two musl ones naming their
  gnu sibling, mac) against the built binaries, unpacks each tarball
  and checks every binary is a regular executable, the launcher, the
  commit, the manifest line for this triple's runtime asset, and the
  six notice files. `cargo-about` cannot run in the sandbox (it needs
  `cargo` and the crate sources), so the test hands
  `third_party_notices.sh` a stub, `tools/fake_cargo_about.sh`, that
  writes the file `-o` names from the directory it is run in — the one
  behaviour v0.35.1 tripped on.
- **`…_bash32_test`** — on a mac, both again under `/bin/bash`, the
  3.2 the macOS runner has, where an empty array is "unbound
  variable" under `-u`. Bazel skips them on Linux
  (`target_compatible_with`); `scripts/lint_repo.py` check 10 refuses
  the one idiom that bit us in any workflow, since CI has no mac leg.

The workflow's other checks that a release still makes on its own
runner — the binary reports the tag, the musl binaries are fully
static — stayed inline: they are assertions over a built artifact,
not steps that make one.

## Before a tag: the Linux run from a mac

`bazel test //...` on a mac runs the two tests under macOS, and CI
runs them under Linux x86_64 — but a mac is where releases are cut
from, and the Linux-only behaviour (node-llama-cpp's binding probe)
is what has bitten. So run them on Linux locally, in the devcontainer:

```sh
bazelisk run //tools:release_steps_docker
```

That builds the devcontainer image from `.devcontainer/Dockerfile` for
this host's arch (the published one is amd64 only, and an emulated
node-llama-cpp is not a faithful test), bind-mounts the working tree
where `devcontainer.json` does, reuses its named cache volumes, and
runs the two tests with `bazelisk test` inside. The first run pays for
the image and a cold Linux build of the shipped binaries — the better
part of an hour; with the volumes warm, a few minutes. Set
`DATALIB_DEVCONTAINER_IMAGE` to use an image you already have (on an
x86_64 host, `ghcr.io/imbue-ai/datalib_devcontainer:latest`, the one
CI runs in).

The release procedure (`.claude/skills/release/SKILL.md`) runs this
before the version bump.

## What stays release-only

- The `runtime` legs on the real runners, with their own bash and
  their own filesystem — the tests make the same calls, not the same
  machine.
- `cargo-about` for real: the release runner installs a pinned binary
  and generates `licenses/rust-crates.md` from the crate sources;
  locally, `scripts/third_party_notices.sh <dir>` with cargo-about on
  PATH is the same call, and is what the tauri bundle runs.
- Signing, notarization, and the asset uploads.
