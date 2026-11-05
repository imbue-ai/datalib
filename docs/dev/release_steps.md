# The release's steps, run before a tag runs them

The code is
[`scripts/release/`](../../scripts/release/), the tests are in
[`tools/BUILD.bazel`](../../tools/BUILD.bazel) under "release.yml's
steps", and the workflow is
[`.github/workflows/release.yml`](../../.github/workflows/release.yml).

`release.yml` runs only when a `v*` tag is pushed, and a tag's run
executes the tag's own tree — a re-run cannot pick up a fix. So a step
that is wrong the first time it runs costs a release (v0.35.0 and
v0.35.1 each lost their tarballs that way, to bugs in inline `run:`
blocks no test ran). This page says what runs before a tag and what
cannot.

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
| `build` job, "Stage tarball" | `scripts/release/stage_tarball.sh <ref> <triple>` | `datalib-<triple>.tar.gz` and its sidecar: the `//datalib/backend:bin` binaries and, except on musl, `lib/libdoltlite`, the `latchkey` launcher, `git-hash`, `runtime.manifest`, every doc `datalib-step help docs` lists, at its path in the repo (written by the staged `datalib-step help <topic>`: `docs/agent_user.md`, `docs/user/raw_stores.md`, `configs/dag_example.toml`, …), `licenses/` |

Each script writes into the current directory, prints what it wrote,
and takes what a test cannot supply through environment variables
(the Bazel outputs as a runfiles tree, the runtime assets as local
files, the commit, a stand-in for `cargo-about`). The header of each
script lists them. A script calls another through `"$BASH"`, so the
bash that runs the outer one runs the inner one too.

The publish steps — `gh release create` / `gh release upload` — stay
out of the scripts, in `.github/actions/publish-release-assets`. They
are the part that cannot run anywhere else.

## Who holds what

A release compiles a great deal of other people's code: on a cache miss
`bazel build` runs every crate's build script, and `cargo` runs the
desktop shell's build scripts and proc macros. Any of them could be
compromised, so none of them runs where it could take a write token or
the signing key.

The boundary has to be a job. GitHub grants a token's permissions per
job and never per step, and a hosted runner has passwordless sudo, so
any process in a job can read the runner's memory, where every secret
that job holds is kept. `id-token: write` also puts the variables that
mint an OIDC token into every step's environment, so a build script in
a job with that grant could get the Vault token itself, with no export
to wait for.

| job | runs on it | holds |
|---|---|---|
| `runtime`, `build` | Bazel, crate build scripts, the staged Node runtime | `contents: read` |
| `publish-runtime`, `publish-tarballs`, `publish-macos-app` | `gh release`, nothing built | `contents: write` |
| `macos-app-compile` | Bazel, the shell's crates, cargo-about | `contents: read` |
| `macos-app-sign` | `codesign`, the Tauri CLI's bundler, `notarytool`, the dmg maker | `id-token: write`, then the certs |
| `macos-app-verify` | the signed app: its Node, qmd, latchkey | `contents: read` |
| `docker-image` | `docker buildx`, whose `RUN` steps see no credentials | `packages: write` |
| `docker-publish` | `docker buildx imagetools`, the image's walkthrough | `packages: write` |

A build job hands what it made to the next through an artifact.
`scripts/lint_repo.py` check 21 holds each grant to its job, refuses a
build command in a job that holds one, and requires every
`actions/checkout` to say `persist-credentials: false` so the token is
not left in `.git/config`. `cargo-update.yml`'s `pull-request` job is
the one checkout that keeps it, because it pushes.

**Compile, sign and verify are phases of
`datalib/tauri/build-signed-app.sh`**, and `compile` and `verify` refuse
to start with signing material in their environment
(`//tools:build_signed_app_test`). The split rests on what the Tauri CLI
measurably does (2.12.4):

- `tauri build` always runs `cargo build --bins --features
  tauri/custom-protocol --release`. After a plain `cargo build --release`
  that recompiles 24 crates, `tauri`, `wry` and `tauri-macros` among
  them. With the flags matched it still recompiles `datalib-tauri`, and
  so runs `tauri-build` and the `tauri-macros` proc macro, because the
  shell's `build.rs` deletes a file the build then finds changed.
- `tauri bundle` runs no compiler at all. Its only calls are `cargo
  metadata --no-deps` and `rustc -vV`. So `macos-app-sign` signs the
  staged tree with `sign-staged.sh` and runs `tauri bundle`, and has no
  way to compile.

What can still run with the certs in reach: the Tauri CLI at the version
`pnpm-lock.yaml` names, Apple's tools, and `imbue-ai/use-vault-secrets`.
A compromised build can still put bad code in what it builds, and
`macos-app-sign` would sign it; that is true of any signed build. What
the jobs prevent is the build taking the key, or rewriting a release.

No job holds a cache key or restores a cache (`ci.md` §"Two caches",
[`threat_models.md`](threat_models.md)), so nothing `main`'s CI saved
reaches what a release builds, and a build script in a release has no
cache to leave anything in.

## What runs on every `bazel test //...`

- **`//tools:stage_runtime_test`** runs `stage_runtime_asset.sh` from
  a directory that is not the repo, with the relative `runtime` the
  workflow passes, then unpacks the asset and runs qmd out of it,
  checking it reports the pinned `DEFAULT_QMD_VERSION`. On Linux CI
  this is where node-llama-cpp's fork probe runs.
- **`//tools:stage_tarball_test`** runs `stage_tarball.sh` for four
  triples (gnu with the CUDA overlay, the two musl ones naming their
  gnu sibling, mac) against the built binaries, unpacks each tarball
  and checks every binary is a regular executable, the library, the launcher, the
  commit, the manifest line for this triple's runtime asset, and the
  six notice files. `cargo-about` cannot run in the sandbox (it needs
  `cargo` and the crate sources), so the test hands
  `third_party_notices.sh` a stub, `tools/fake_cargo_about.sh`, that
  writes the file `-o` names from the directory it is run in, which is
  the behaviour a wrong working directory breaks.
- **`…_bash32_test`** — on a mac, both again under `/bin/bash`, the
  3.2 the macOS runner has, where an empty array is "unbound
  variable" under `-u`. The runtime's also runs under `/usr/bin/rsync`,
  which a laptop's Homebrew rsync would otherwise stand in for. Bazel
  skips them on Linux (`target_compatible_with`);
  `scripts/lint_repo.py` check 10 refuses an empty bash array in any
  workflow step, since CI has no mac leg.

The workflow's other checks are assertions over a built artifact, made
on its own runner before anything is published:

- **`scripts/release/check_binaries.sh <tag> <dir>`** refuses backend
  binaries that were not built `-c opt` or whose `datalib-dag
  --version` is not the tag. Bazel's default mode leaves the Rust
  unoptimized and a sync about twice as slow, and the app shipped that
  way through v0.47.0, unstamped too. The `build` job runs it on the
  tarball's binaries; the `macos-app-verify` job, through
  `build-signed-app.sh`, on the ones inside `Datalib.app`. It tells the
  mode from the Bazel output directory (`darwin_arm64-opt`,
  `k8-fastbuild`) that generated-source paths compile into a binary,
  and fails when it finds none. `//tools:check_binaries_test` (and its
  `_bash32` twin) runs it against stand-ins.
- The musl binaries are fully static: inline in the workflow.

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
part of an hour; with the volumes warm, a few minutes. If your
`.bazelrc.user` is the symlink [`ci.md`](ci.md#locally-you-are-probably-not-on-the-remote-cache)
sets up, the script mounts its target too, so the container reads and
writes the contributor BuildBuddy cache your laptop uses; the release
org stays CI's alone. What it shares is with other Apple-silicon runs
of this script, not with PR runs, which build on x86_64. Set
`DATALIB_DEVCONTAINER_IMAGE` to use an image you already have (on an
x86_64 host, `ghcr.io/imbue-ai/datalib_devcontainer:latest`, the one
CI runs in).

The release procedure (`.claude/skills/release/SKILL.md`) runs this
before the version bump.

## The mac legs' runner image

The mac legs name one GitHub-hosted image, `macos-26` (Xcode 26.6), and
GitHub retires an image about once a year. `latchkey-auth.yml` runs on
the same one, so the sign-in suite tries it every night. A tag runs its
own tree, so a retired label costs that release its mac assets and a
re-run cannot fix it. What the image decides:

- **The SDK our own binaries link against**, which sets what AppKit and
  WebKit do for the app: 26.5 on this image. `xcrun vtool -show-build
  <binary>` prints it. The minimum macOS (11.0) is not the image's: it
  comes from `.bazelrc`, and rustc's floor for Apple Silicon for the
  Tauri shell. The two curl shims and Node arrive prebuilt, so their
  SDK is whatever built them.
- **The system tools the scripts call**: `/usr/bin/rsync`, which since
  macOS 15 is openrsync and accepts `--chmod` but ignores it on a tree;
  `/bin/bash` 3.2; `hdiutil`, `codesign` and `notarytool`.
- **The Chrome `datalib/tauri/check-app.sh` needs.**

Rust and Bazel are pinned by the repo, whatever the image carries.

To move to another image, try these steps first in a throwaway workflow
triggered by a push to a branch, with no write grants. **Run it with no
cache key.** A darwin action's cache key does not cover the image, so
the contributor cache replays another image's compiles and test results:
`stage_runtime_test` reported `(cached) PASSED` on the first image with
openrsync, in the same job whose runtime step had just failed, and a
binary whose link is replayed would carry the other image's SDK. A tag
reads no cache (`ci.md` §"Two caches"), so a cold run is the one that
matches.

## What stays release-only

- The `runtime` legs on the real runners, with their own bash and
  their own filesystem — the tests make the same calls, not the same
  machine.
- `cargo-about` for real: `scripts/third_party_notices.sh <dir>` builds
  the pinned binary (`//third-party/cargo-about`) and generates
  `licenses/rust-crates.md` from the crate sources, so it needs `cargo`
  and the network. The release runner and the tauri bundle both make
  this same call.
- Signing, notarization, and the asset uploads. `macos-app-sign` can
  be tried on a branch with an ad-hoc identity (`-`), which proves the
  artifact hand-off and that the bundle compiles nothing; Vault refuses
  anything but a tag, so the real certificate and `notarytool` are first
  tried by the next release.
- The Docker jobs' pushes. Their builds and the tagging can be tried on a
  branch with a release's published tarballs and `imagetools create
  --dry-run`, but only a tag pushes. Their Docker Hub images come through
  `mirror.gcr.io` (`ci.md` §"What runs where").
