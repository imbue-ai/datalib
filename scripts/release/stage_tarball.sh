#!/usr/bin/env bash
# The `build` job's "Stage tarball" step of release.yml, minus the
# upload: lay out `datalib-<version>-<triple>/` from the binaries bazel
# built, with the `latchkey` launcher, the commit hash, the manifest of
# the runtime asset the binaries fetch on first use, and the third-party
# notices; then tar it. Writes `datalib-<triple>.tar.gz` and its
# `.sha256` sidecar into the current directory and prints the tarball's
# name.
#
#   scripts/release/stage_tarball.sh <ref-name> <triple>
#
# The binaries are read from `bazel-bin` as `//datalib/backend:bin` laid
# them out — one list of shipped names, kept there — so the caller has
# built that target first, in the configuration it means to ship.
#
# A script rather than a `run:` block so the same lines run under
# //tools:stage_tarball_test — from a foreign directory, with the
# relative paths the workflow passes, and on a mac's /bin/bash 3.2 —
# before a tag ever runs them. What the test cannot do it swaps in
# through these variables:
#
#   RELEASE_BAZEL_BIN    a tree laid out like bazel-bin (the test's runfiles)
#   RELEASE_ASSETS_DIR   the runtime assets and their .sha256 sidecars as
#                        local files, in place of `gh release` on the
#                        published release
#   GITHUB_SHA           the commit, in place of `git rev-parse HEAD`
#   GITHUB_REPOSITORY    where the manifest's URLs point (default
#                        imbue-ai/datalib)
set -euo pipefail

if [[ $# -ne 2 ]]; then
    echo "usage: $0 <ref-name> <triple>" >&2
    exit 2
fi
ref_name="$1"
triple="$2"
version="${ref_name#v}"

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../.." && pwd -P)"
bin="${RELEASE_BAZEL_BIN:-$repo_root/bazel-bin}"
repository="${GITHUB_REPOSITORY:-imbue-ai/datalib}"

log() { printf '>>> stage_tarball: %s\n' "$*" >&2; }
fail() { printf 'stage_tarball: error: %s\n' "$*" >&2; exit 1; }

checksum() {
    if command -v sha256sum >/dev/null; then
        sha256sum "$1" > "$1.sha256"
    else
        shasum -a 256 "$1" > "$1.sha256"
    fi
}

stage="datalib-${version}-${triple}"
rm -rf "$stage"
mkdir -p "$stage"

# `//datalib/backend:bin` is the shipped binaries under their public
# names — `datalib-dag`, `datalib-doltlite`, the two curl shims — so
# the list lives there and nowhere else. `-L`: bazel-bin (and a runfiles
# tree all the more) may hold them as links, and the tarball holds
# files. `datalib-dag`/`datalib-step` keep those exact names:
# datalib-http resolves them via sibling lookup (http/src/worker.rs
# resolve_bin), datalib-step looks for `latchkey-curl-router` next to
# itself (datalib/backend/etl/src/latchkey.rs) and the router for
# `curl-impersonate` next to itself.
[[ -d "$bin/datalib/backend/bin" ]] || fail "no $bin/datalib/backend/bin — build //datalib/backend:bin first"
log "copying the binaries from $bin/datalib/backend/bin"
cp -RL "$bin/datalib/backend/bin/." "$stage/"
chmod +x "$stage"/*
for required in datalib-dag datalib-step datalib-http datalib-doltlite latchkey-curl-router curl-impersonate; do
    [[ -x "$stage/$required" ]] || fail "$required missing from //datalib/backend:bin"
done

# The `latchkey` launcher: runs the bundled latchkey with the router
# curl beside it, from `runtime/` when one is staged beside it (the
# docker image) and else from the fetched tree in the user's cache.
install -m 0755 "$repo_root/scripts/latchkey-wrapper.sh" "$stage/latchkey"

# The commit beside the binaries, for `datalib_runtime::build_id`: what
# a log line's file and line number are relative to. `:bin` stages one
# already (a stamped genrule, not a rustc stamp — .bazelrc §stamping);
# the release writes the workflow's own `GITHUB_SHA` over it, the same
# value from the source the job trusts, and the test's tree has none.
if [[ -n "${GITHUB_SHA:-}" ]]; then
    echo "$GITHUB_SHA" > "$stage/git-hash"
else
    git -C "$repo_root" rev-parse HEAD > "$stage/git-hash"
fi

# `runtime.manifest`: which runtime asset of THIS release the binaries
# fetch on first use, with its sha256 and size read back from what the
# `runtime` job published. The musl legs name their gnu sibling's
# asset: the Node in it is a glibc build (nodejs.org ships no musl
# one), and a musl datalib on a glibc host — the common case — runs it
# fine; a musl host is refused by the resolver with the reason named.
# The format is datalib/backend/runtime/src/runtime_manifest.rs's.
runtime_triple="${triple/-musl/-gnu}"
base_url="https://github.com/${repository}/releases/download/${ref_name}"
asset_sha_and_bytes() { # asset name -> "<sha> <bytes>"
    local name="$1" sha bytes
    if [[ -n "${RELEASE_ASSETS_DIR:-}" ]]; then
        sha="$(cut -d' ' -f1 "$RELEASE_ASSETS_DIR/$name.sha256")"
        bytes="$(wc -c < "$RELEASE_ASSETS_DIR/$name" | tr -d ' ')"
    else
        gh release download "$ref_name" --pattern "$name.sha256" --clobber --dir manifest-src
        sha="$(cut -d' ' -f1 "manifest-src/$name.sha256")"
        bytes="$(gh release view "$ref_name" --json assets \
            --jq ".assets[] | select(.name == \"$name\") | .size")"
    fi
    [[ "$sha" =~ ^[0-9a-f]{64}$ ]] || fail "bad sha256 for $name: '$sha'"
    [[ "$bytes" =~ ^[0-9]+$ ]] || fail "no size for $name on the release"
    echo "$sha $bytes"
}
manifest_line() { # kind, asset name
    local kind="$1" name="$2" sha_bytes
    sha_bytes="$(asset_sha_and_bytes "$name")"
    echo "$kind $name $sha_bytes $base_url/$name"
}
log "writing runtime.manifest for runtime-${runtime_triple}"
{
    echo "# The Node runtime these binaries fetch on first use — docs/dev/runtime_fetch.md"
    manifest_line cpu "runtime-${runtime_triple}.tar.gz"
    if [[ "$runtime_triple" == "x86_64-unknown-linux-gnu" ]]; then
        manifest_line cuda "runtime-${runtime_triple}-cuda.tar.gz"
    fi
} > "$stage/runtime.manifest"
cat "$stage/runtime.manifest" >&2

# Third-party notices beside the binaries. Without an override the
# script runs a default-config bazel build of its own, which repoints
# bazel-bin — every copy out of bazel-bin above is already done.
"$BASH" "$repo_root/scripts/third_party_notices.sh" "$stage/licenses"

tarball="datalib-${triple}.tar.gz"
tar -czf "$tarball" "$stage"
checksum "$tarball"
ls -la "$tarball" >&2
echo "$tarball"
