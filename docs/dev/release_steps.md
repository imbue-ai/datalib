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

Publishing is a script too, `scripts/release/publish_release.py`, and
`//:publish_release_test` runs it against a stand-in for GitHub's API. Only
a tag's run shows that the real API takes its calls.

## Draft, then publish

No release exists until every other job has succeeded. Then
`publish-release`, the one job that can write to a release, does all of this
in one step:

1. **Takes each asset from the artifact that should hold it.** Each artifact
   has an exact list of files, and anything missing or extra stops the job
   before a release exists. Without this, a build's artifact could carry a
   file named like the dmg and replace it on the release. It also checks each
   `.sha256` against its file, and the dmg against the hash `macos-app-verify`
   reported, because a later upload can replace an artifact but cannot change
   another job's output.
2. **Removes any draft already on this tag**, a failed attempt's or anyone
   else's, and stops if the tag already has a published release.
3. **Creates a draft and uploads**, trying once more if the upload fails.
4. **Reads the draft back and compares digests.** GitHub reports each asset's
   digest while it is still a draft (measured on v0.51.0's). A name or digest
   that differs, including an asset nobody here made, stops the job and leaves
   the draft unpublished.
5. **Publishes it** and marks it latest.
6. **Reads it back again**, and fails loudly if an asset differs, if it is not
   immutable, or if `releases/latest` is not this tag. It cannot undo a
   publish, so this is an alarm rather than a guard.

So a release is writable by us only for as long as the upload takes, rather
than for the whole run (50 minutes for v0.50.0), and a failed job leaves
nothing open.

The repository's immutable-releases setting is on, so from step 5 GitHub
refuses any change to the assets or the tag, to anyone. Nothing can be added
later, so:

- **A failed job holds the whole release back.** `releases/latest` still
  points at the previous release. Re-run the failed jobs (`gh run rerun <id>
  --failed`), and `publish-release` runs after them.
- **A leg that cannot be fixed soon** means cutting a patch release. A tag's
  run executes the tag's own tree, so a fix needs a new tag either way.
- **The title and notes stay editable**, as GitHub's docs say, so the notes
  can still be written after publishing.

A job reads what an earlier one made from this run's artifacts, never from the
release. The manifest's URLs do name the tag, so a runtime fetch works only
once the release is published, and nothing in the workflow fetches one: the
Docker image and the Mac app carry theirs.

### Pinning what people get by default

`scripts/install.sh` and `docs/user/docker.md` name a release by version and
hash, not by `latest`, for the reason in
[`security_posture.md`](security_posture.md), and `test.yml` and
`cargo-update.yml` name the devcontainer image by digest for the same reason.
After a release publishes, `uv run python scripts/release/pin_release.py vX.Y.Z`
rewrites all four from what GitHub reports, which needs `gh` to hold
`read:packages` (`gh auth refresh -s read:packages`). Wait for that tag's
`devcontainer.yml` first, or it stops for want of the image. It refuses a release that is a draft, is not immutable, or has no
successful `release.yml` run on its tag's commit. Until that PR lands, people
get the previous release, which still works, and CI stays on the previous image.

### What one failed job costs

Nothing is published until everything has succeeded, because once published a
missing asset can never be added.

| what failed | Docker image | `releases/latest` |
|---|---|---|
| the mac leg of `runtime` or `build` | tagged | previous release |
| a musl leg of `build` | tagged | previous release |
| a gnu leg of `build` or `runtime` | not tagged | previous release |
| a `macos-app-*` job | tagged | previous release |

The Docker image does not wait for the release. It carries its own runtime, and
`docker-publish` checks the tagged image against the walkthrough. So
`ghcr.io/…:latest` can be a version ahead of `releases/latest` until the release
is published. An image needs both gnu legs, so losing either tags nothing.

### What a re-run does

`gh run rerun --failed` re-runs each failed job and every job downstream of
it, including jobs that passed. A matrix job downstream runs all its legs;
a matrix job that failed runs only its failed legs.

| what failed | what runs again |
|---|---|
| a `build` leg | that leg, both `docker-image` legs, `docker-publish`, `publish-release` |
| a `runtime` leg | that leg, all five `build` legs, and the rest of the row above |
| a `macos-app-*` job | the rest of that chain, `publish-release` |
| `publish-release` | it alone, which removes its own draft and starts again |

So re-running a failed `build` leg rebuilds both images, and the version tag
and `:latest` move to the new ones. A job that a re-run repeats also uploads
its artifact again, and `download-artifact` takes the same-named artifact
with the highest id, which is not always the newest. So the uploads a re-run
can repeat, `tarball-*`, `docker-digest-*` and `docker-image-digest`, say
`overwrite: true`.

### Trying the job graph on a branch

The tables above are what GitHub did with a copy of this job graph, and that
copy needs no tag. Push a throwaway branch with a workflow that triggers on
that branch, has the same `needs` and `if` lines and the same artifact
uploads and downloads, and has steps that only echo, with no write grants.
Make one leg `exit 1` when `github.run_attempt` is 1, then run `gh run rerun
--failed`. Delete the branch afterwards. Try it this way before changing a
`needs` or an `if` here: a job's `if` is not checked by anything else, and
the rule that catches people is that a job with no `if` is skipped when
anything up its chain failed, not only the jobs it names.

## The reader wheels

`imbue-datalib-reader` puts [a small reader module](doltlite.md#calling-the-library-directly)
and `libdoltlite` on PyPI, so that `uv add imbue-datalib-reader` works on Linux and
an Apple-silicon Mac.
It has its own workflow,
[`reader-wheels.yml`](../../.github/workflows/reader-wheels.yml), and a tag starts
it beside `release.yml`, for two reasons. `pin_release.py` refuses a tag whose
`release.yml` run is not green, so a PyPI failure in that run would block pinning
a release that is fine. And a run's artifacts reach only its own jobs, so nothing
`release.yml` compiles can plant a wheel here. A failure here costs PyPI that
version and costs the release nothing.

There are five wheels: one per architecture (x86_64, aarch64) and libc on Linux,
and one for an Apple-silicon Mac:

| wheel | built in | runs on |
|---|---|---|
| `manylinux_2_17` (also tagged `manylinux2014`) | `quay.io/pypa/manylinux2014_<arch>`, glibc 2.17 | any glibc from 2.17 on |
| `musllinux_1_2` | `quay.io/pypa/musllinux_1_2_<arch>`, musl 1.2 | Alpine and other musl from 1.2 on |
| `macosx_11_0_arm64` | Bazel on `macos-26`, the library the mac tarball ships | macOS 11 or later on Apple silicon |

On Linux, a library's symbol versions come from the libc it is linked against,
which is why each one is compiled in an old image rather than by Bazel on the
runner. The source and flags still come from Bazel
(`//third-party/doltlite:rename_amalgamation` and `:library_copts`), so there is no
second list of them. A Mac's floor is `--macos_minimum_os` in `.bazelrc`, whatever
SDK the runner has, so the same build serves. It has no Intel wheel, as a release
has no Intel tarball.

| step | script | what it does |
|---|---|---|
| `wheels` job, each leg | `scripts/release/stage_reader_wheels.sh <linux\|macos> <out-dir>` | On Linux, compiles in each image with no network and fails unless that image's `auditwheel` agrees with the tag the wheel claims. On a Mac, which has no `auditwheel`, fails unless the library is arm64 alone, says macOS 11.0, is signed and links only the system's. Both write the wheel with `make_reader_wheel.py` (the standard library, no build backend) |
| the same job | `scripts/release/check_reader_wheels.sh <linux\|macos> <wheel-dir>` | Installs with `pip` on glibc 2.17, Debian, Alpine and `/usr/bin/python3` on a Mac, and with `uv add` under uv's own Python on Debian, Alpine and a Mac. On Linux each gets both libcs' wheels to choose from. Each reads a store the pinned shell wrote, read-only, with `sqlite3` beside it and, on Debian, Alpine and `/usr/bin/python3`, stock SQLite loaded globally |
| `publish` job, on a tag | `pypa/gh-action-pypi-publish` | Uploads all five, with attestations. PyPI never lets a file be replaced; a re-run uploads only what is missing |

It runs on a PR that touches the module, these scripts, the workflow or the
doltlite pin, and by hand with `gh workflow run reader-wheels.yml`. On a tag, the
script stops unless the tag is the version in `datalib/backend/Cargo.toml`.
`//datalib/reader:reader_wheel_test` runs on every `bazel test //...`: the wheel's
layout and `RECORD`, the same bytes from the same inputs, and a read through the
unpacked package. `//datalib/reader:host_wheel` is such a wheel around the host's
library, which `python_reader_test`, `python_package_test` and
`step_protocol_example_test` unpack and read through.

To run the two scripts on a mac, for the mac wheel, and then `linux` for the
wheels of its own architecture, whose arm64 images run natively:

```sh
BAZEL=bazelisk scripts/release/stage_reader_wheels.sh macos /tmp/wheels
```

```sh
BAZEL=bazelisk scripts/release/check_reader_wheels.sh macos /tmp/wheels
```

With `linux`, `READER_ARCH=x86_64` gives the other architecture, which runs under
an emulator and takes far longer to compile. The mac check needs `uv`.

**How PyPI is set up.** This is done, and nothing here can change it. On pypi.org,
under an account the team shares, `imbue-datalib-reader` names one *trusted
publisher*, the only place PyPI takes an upload from: owner `imbue-ai`, repository
`datalib`, workflow `reader-wheels.yml`, environment `pypi`. If that workflow or
environment is renamed, change the publisher to match, or the next tag's upload is
refused. That GitHub environment is restricted to `v*` tags, which
[`security_posture.md`](security_posture.md) §"The settings this rests on" checks.

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
| `publish-release` | `gh`, nothing built | `contents: write` |
| `macos-app-compile` | Bazel, the shell's crates, cargo-about | `contents: read` |
| `macos-app-sign` | `codesign`, the Tauri CLI's bundler, `notarytool`, the dmg maker | `id-token: write`, then the certs |
| `macos-app-verify` | the signed app: its Node, qmd, latchkey | `contents: read` |
| `docker-image` | `docker buildx`, whose `RUN` steps see no credentials | `packages: write` |
| `docker-publish` | `docker buildx imagetools`, the image's walkthrough | `packages: write` |
| `reader-wheels.yml`'s `wheels` | Bazel fetching and, on a Mac, compiling doltlite, gcc in the wheel images, the `pip` and `uv` checks | `contents: read` |
| `reader-wheels.yml`'s `publish` | `pypa/gh-action-pypi-publish`, no checkout | `id-token: write`, which only PyPI takes |

A build job hands what it made to the next through an artifact.
`scripts/lint_repo.py` check 21 holds each grant to its job, in `release.yml`
and `reader-wheels.yml` both, refuses a
build command in a job that holds one, and requires every
`actions/checkout` to say `persist-credentials: false` so the token is
not left in `.git/config`. `cargo-update.yml`'s `pull-request` job is
the one checkout that keeps it, because it pushes.

**An artifact is not trusted to hold what its name says.** A build script
that reads the runner's memory has the token that uploads artifacts, so it can
upload under any name. So check 21 requires every download in `release.yml` to
land under `runner.temp`: in the checkout, a planted file would replace the
script a later step runs. The sign job's archive goes through
`scripts/release/unpack_compiled_app.py`, never `tar`: it reads the whole
archive first, takes only `binaries/`, `runtime/`, `licenses/` and
`target/release/datalib-tauri`, refuses a link or anything else that is not a
plain file or directory, and places nothing if any entry fails. An archive
unpacked by plain `tar` replaced both `build-signed-app.sh` and the Tauri CLI
in a test on bsdtar; the attack itself has not been run on a runner.

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
[`security_posture.md`](security_posture.md)), so nothing `main`'s CI saved
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
x86_64 host, `ghcr.io/imbue-ai/datalib_devcontainer:latest`, the last
release's).

The release procedure (`.claude/skills/release/SKILL.md`) runs this
before the version bump.

## The mac legs' runner image

The mac legs, `reader-wheels.yml`'s among them, name one GitHub-hosted image,
`macos-26` (Xcode 26.6), and
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

- The upload to PyPI, in `reader-wheels.yml`. PRs build and check the same
  wheels, but only a tag's run can publish, so a problem with the upload itself
  first shows on a tag.
- The `runtime` legs on the real runners, with their own bash and
  their own filesystem — the tests make the same calls, not the same
  machine.
- `cargo-about` for real: `scripts/third_party_notices.sh <dir>` builds
  the pinned binary (`//third-party/cargo-about`) and generates
  `licenses/rust-crates.md` from the crate sources, so it needs `cargo`
  and the network. The release runner and the tauri bundle both make
  this same call.
- Signing, notarization, and the real API's answers to
  `publish_release.py`. `macos-app-sign` can
  be tried on a branch with an ad-hoc identity (`-`), which proves the
  artifact hand-off and that the bundle compiles nothing; Vault refuses
  anything but a tag, so the real certificate and `notarytool` are first
  tried by the next release.
- Publishing and `releases/latest` moving to it: these need a real release,
  and a tag's run is the only one this workflow makes. A mistake there fails
  `publish-release`, and since a tag's run executes the tag's own tree, the fix
  takes a new tag. The script's last step checks the result.
- The Docker jobs' pushes. Their builds and the tagging can be tried on a
  branch with a release's published tarballs and `imagetools create
  --dry-run`, but only a tag pushes. Their Docker Hub images come through
  `mirror.gcr.io` (`ci.md` §"What runs where").
