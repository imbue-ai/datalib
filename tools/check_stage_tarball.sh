#!/usr/bin/env bash
# Runs release.yml's "Stage tarball" step, minus the upload, the way the
# step runs it: scripts/release/stage_tarball.sh from a directory that
# is not the repo, with the binaries `//datalib/backend:bin` built and
# the relative paths the workflow passes — which is how v0.35.1's
# notices landed in the wrong directory. Then unpacks the tarball and
# checks what a release ships: every binary, the launcher, the commit,
# the manifest naming this triple's runtime asset (the gnu one for a
# musl triple) and the notices. RELEASE_SHELL picks the bash the script
# runs under; the macOS variant sets /bin/bash, the 3.2 the mac runner
# has.

# --- begin runfiles.bash initialization v3 ---
# Copy-pasted from the Bazel Bash runfiles library v3.
set -uo pipefail; set +e
f=bazel_tools/tools/bash/runfiles/runfiles.bash
# shellcheck disable=SC1090
source "${RUNFILES_DIR:-/dev/null}/$f" 2>/dev/null || \
  source "$(grep -sm1 "^$f " "${RUNFILES_MANIFEST_FILE:-/dev/null}" | cut -f2- -d' ')" 2>/dev/null || \
  source "$0.runfiles/$f" 2>/dev/null || \
  source "$(grep -sm1 "^$f " "$0.runfiles_manifest" | cut -f2- -d' ')" 2>/dev/null || \
  { echo>&2 "ERROR: cannot find $f"; exit 1; }; f=; set -e
# --- end runfiles.bash initialization v3 ---

script="$(rlocation _main/scripts/release/stage_tarball.sh)"
[[ -f "$script" ]] || { echo "ERROR: stage_tarball.sh not in runfiles at $script" >&2; exit 1; }
fake_cargo_about="$(rlocation _main/tools/fake_cargo_about.sh)"
# The runfiles tree lays the Bazel outputs out as bazel-bin does, except
# that the binaries come as `:bin_unstamped` (so a commit does not re-run
# this test); the script wants them at `datalib/backend/bin`, so build
# that tree out of links.
runfiles="${script%/scripts/release/stage_tarball.sh}"
tree="$TEST_TMPDIR/bazel-bin"
mkdir -p "$tree/datalib/backend"
ln -s "$runfiles/datalib/backend/bin_unstamped" "$tree/datalib/backend/bin"
ln -s "$runfiles/datalib/ui" "$tree/datalib/ui"
ln -s "$runfiles/third-party" "$tree/third-party"

shell="${RELEASE_SHELL:-bash}"
echo ">>> running under $("$shell" -c 'echo "$BASH_VERSION"')"

work="$TEST_TMPDIR/work"
mkdir -p "$work"
cd "$work"

# The runtime assets the manifest describes, as the `runtime` job would
# have published them: any bytes, with the sidecar sha256sum writes.
mkdir assets
for name in runtime-x86_64-unknown-linux-gnu.tar.gz runtime-x86_64-unknown-linux-gnu-cuda.tar.gz \
            runtime-aarch64-unknown-linux-gnu.tar.gz runtime-aarch64-apple-darwin.tar.gz; do
    printf 'not a runtime: %s\n' "$name" > "assets/$name"
    ( cd assets && if command -v sha256sum >/dev/null; then sha256sum "$name" > "$name.sha256"; else shasum -a 256 "$name" > "$name.sha256"; fi )
done

sha_of() { cut -d' ' -f1 "assets/$1.sha256"; }

# One triple per manifest shape: gnu with the CUDA overlay, musl naming
# its gnu sibling, mac.
check() { # triple, expected runtime triple, expect cuda line (yes/no)
    local triple="$1" runtime_triple="$2" cuda="$3"
    echo ">>> staging $triple"
    local tarball
    tarball="$(RELEASE_BAZEL_BIN="$tree" RELEASE_ASSETS_DIR="$PWD/assets" \
        GITHUB_SHA=0123456789abcdef0123456789abcdef01234567 \
        THIRD_PARTY_NOTICES_BAZEL_BIN="$tree" CARGO_ABOUT="$fake_cargo_about" \
        "$shell" "$script" v0.0.0-test "$triple")"
    [[ "$tarball" == "datalib-$triple.tar.gz" ]] || { echo "ERROR: printed '$tarball'" >&2; exit 1; }
    [[ -f "$tarball" && -f "$tarball.sha256" ]] || { echo "ERROR: $tarball or its sidecar not written" >&2; exit 1; }
    if command -v sha256sum >/dev/null; then sha256sum -c "$tarball.sha256"; else shasum -a 256 -c "$tarball.sha256"; fi

    rm -rf unpacked && mkdir unpacked
    tar -xzf "$tarball" -C unpacked
    local dir="unpacked/datalib-0.0.0-test-$triple"
    [[ -d "$dir" ]] || { echo "ERROR: the tarball does not unpack to datalib-0.0.0-test-$triple" >&2; ls unpacked >&2; exit 1; }

    local f
    for f in datalib-dag datalib-step datalib-http datalib-applet datalib-migrate-config datalib-doltlite \
             datalib-fsindex datalib-dirtree-diff latchkey-curl-router curl-impersonate latchkey; do
        [[ -f "$dir/$f" && -x "$dir/$f" && ! -L "$dir/$f" ]] || { echo "ERROR: $f is not a regular executable in the tarball" >&2; ls -la "$dir" >&2; exit 1; }
    done
    [[ "$(cat "$dir/git-hash")" == 0123456789abcdef0123456789abcdef01234567 ]] || { echo "ERROR: git-hash is $(cat "$dir/git-hash")" >&2; exit 1; }
    for f in README.md rust-crates.md ui-bundle.md doltlite/LICENSE.md node/LICENSE curl-impersonate/LICENSE-curl; do
        [[ -f "$dir/licenses/$f" && ! -L "$dir/licenses/$f" ]] || { echo "ERROR: licenses/$f missing from the tarball" >&2; find "$dir/licenses" >&2; exit 1; }
    done

    local manifest="$dir/runtime.manifest"
    local cpu="runtime-$runtime_triple.tar.gz"
    local expected_cpu="cpu $cpu $(sha_of "$cpu") $(wc -c < "assets/$cpu" | tr -d ' ') https://github.com/imbue-ai/datalib/releases/download/v0.0.0-test/$cpu"
    grep -qxF "$expected_cpu" "$manifest" || { echo "ERROR: manifest lacks the line: $expected_cpu" >&2; cat "$manifest" >&2; exit 1; }
    if [[ "$cuda" == yes ]]; then
        grep -q "^cuda runtime-$runtime_triple-cuda.tar.gz " "$manifest" || { echo "ERROR: manifest lacks the cuda line" >&2; cat "$manifest" >&2; exit 1; }
    else
        ! grep -q '^cuda ' "$manifest" || { echo "ERROR: manifest has a cuda line for $triple" >&2; cat "$manifest" >&2; exit 1; }
    fi
    echo ">>> $triple: ok"
}

check x86_64-unknown-linux-gnu x86_64-unknown-linux-gnu yes
check x86_64-unknown-linux-musl x86_64-unknown-linux-gnu yes
check aarch64-unknown-linux-musl aarch64-unknown-linux-gnu no
check aarch64-apple-darwin aarch64-apple-darwin no
