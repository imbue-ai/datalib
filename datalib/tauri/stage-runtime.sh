#!/usr/bin/env bash
#
# Stage the bundled Node runtime + JS package trees (`latchkey`, `qmd`)
# into `datalib/tauri/runtime/`, which tauri.conf.json ships under
# the .app's `Contents/Resources/runtime/`. With it in place the
# packaged app needs NO Node/npm/npx on the host: the backend binaries
# resolve `runtime/node/bin/node` + the staged trees via
# `datalib_core::node_runtime` and only fall back to `npx` when the
# tree is missing (dev runs, bazel tests).
#
# Layout staged here (and expected by the Rust resolver — keep in sync):
#
#   runtime/
#     node/bin/node                                    pinned Node
#     latchkey/<v>/node_modules/latchkey/dist/src/cli.js
#     qmd/<v>/node_modules/@tobilu/qmd/dist/cli/qmd.js   (one tree per
#                                                         distinct pin)
#   binaries/latchkey      user-facing launcher (latchkey-wrapper.sh):
#                          bundled node + tree + LATCHKEY_CURL shim
#
# Everything staged here comes out of Bazel. That is the whole design:
# this script downloads nothing and resolves nothing. It used to do
# both — curl a Node tarball from nodejs.org against four
# hand-maintained sha256s, then run `npm install` against the live
# registry with no lockfile and no integrity checking — and then
# codesign the result with our Developer ID and notarize it. Three
# Bazel targets replace all of that:
#
#   //datalib/tauri:bundled_node             the rules_nodejs toolchain's
#                                            Node, NODE_VERSION in MODULE.bazel
#   //third-party/qmd/runtime:qmd_tree       lockfile-pinned, sha512 per tarball
#   //third-party/latchkey/runtime:latchkey_tree            likewise
#
# The version pins are still grepped out of the Rust sources that spawn
# the tools, because they name the staged DIRECTORIES and the resolver
# looks those up by the Rust constant:
#   * latchkey  — LATCHKEY_VERSION in backend/runtime/src/node_runtime.rs
#   * qmd       — DEFAULT_QMD_VERSION in backend/runtime/src/qmd.rs
# `//tools:version_pins_test` holds each equal to the package.json that
# its Bazel tree is built from, so a pin that moves in one place fails
# the build rather than staging a directory nothing will look in.
#
# Build-host requirements: bazelisk and rsync. No Node, no npm, no C
# toolchain — the native modules arrive prebuilt inside their npm
# tarballs, which is what made this possible (see the better-sqlite3 13
# note in MODULE.bazel). Trees are staged for the HOST platform; cross
# builds are not supported (same restriction as the rest of the tauri
# build).
#
# Signing: when $APPLE_SIGNING_IDENTITY is set (same convention as
# tauri.conf.json's beforeBuildCommand), the node binary and every
# native library in the trees is codesigned with the hardened runtime.
# `node` additionally keeps the JIT entitlements extracted from the
# upstream-signed binary — V8 won't start under the hardened runtime
# without them.

set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
backend_dir="$script_dir/../backend"
runtime_dir="$script_dir/runtime"

log() { printf '>>> stage-runtime: %s\n' "$*" >&2; }
fail() { printf 'stage-runtime: error: %s\n' "$*" >&2; exit 1; }

command -v bazelisk >/dev/null 2>&1 || fail "bazelisk not found on PATH"
command -v rsync >/dev/null 2>&1 || fail "rsync not found on PATH"

# ---------------------------------------------------------------------------
# Version pins, grepped from the Rust sources (see header).
# ---------------------------------------------------------------------------

extract_pin() { # file, pattern of the const line
    local v
    v="$(grep -E "$2" "$1" | sed -E 's/.*"([^"]+)".*/\1/' | head -n1)"
    [[ -n "$v" ]] || fail "could not extract version pin from $1 (pattern: $2)"
    printf '%s' "$v"
}

latchkey_version="$(extract_pin "$backend_dir/runtime/src/node_runtime.rs" \
    '^pub const LATCHKEY_VERSION:')"
qmd_version="$(extract_pin "$backend_dir/runtime/src/qmd.rs" \
    '^pub const DEFAULT_QMD_VERSION:')"

log "pins: latchkey=$latchkey_version qmd=$qmd_version"

# ---------------------------------------------------------------------------
# Build the three Bazel targets and locate their outputs.
# ---------------------------------------------------------------------------

log "building runtime targets"
bazelisk build \
    //datalib/tauri:bundled_node \
    //third-party/qmd/runtime:qmd_tree \
    //third-party/latchkey/runtime:latchkey_tree >&2

bin="$(bazelisk info bazel-bin)"

# ---------------------------------------------------------------------------
# Stage.
# ---------------------------------------------------------------------------

# rsync rather than cp: `-a` keeps the pnpm store's relative symlinks as
# symlinks (dereferencing them would triple the bundle — every package
# would be copied once per dependent), `--delete` clears whatever a
# previous stage left behind, and `--chmod` makes the copy writable
# since Bazel's outputs are read-only and codesign has to rewrite them.
stage_tree() { # kind, version, source node_modules dir
    local dest="$runtime_dir/$1/$2/node_modules"
    log "staging $1@$2"
    mkdir -p "$dest"
    rsync -a --delete --chmod=Du+wx,Fu+w "$3/" "$dest/"
}

# Drop a package we deliberately do not ship, and any symlink left
# pointing into it. Both current entries preserve what the pre-Bazel
# `npm install` already did — they are not new policy:
#
#   * typescript is qmd's only peer dependency, ~23 MB, and its CLI
#     never imports it at runtime (dev-time tsx/typechecking). The old
#     script dropped it with `npm install --omit=peer`.
#   * playwright (with playwright-core, ~17 MB of a 28 MB tree) backs
#     latchkey's browser-login flows, which datalib never invokes;
#     latchkey degrades gracefully when the import fails, the same way
#     its own bun-compiled release binaries do. The old script deleted
#     it by hand after installing it.
#
# The dangling-symlink sweep is the part worth keeping: pnpm's layout
# reaches a package through several links, and a link pointing at
# nothing is both a broken `require` and something to explain to
# codesign.
prune_pkg() { # dest root, store glob
    find "$1/.aspect_rules_js" -maxdepth 1 -name "$2" -exec rm -rf {} + 2>/dev/null || true
    find "$1" -type l ! -exec test -e {} \; -exec rm -f {} + 2>/dev/null || true
}

log "staging node"
mkdir -p "$runtime_dir/node/bin"
rsync -a --chmod=u+wx "$bin/datalib/tauri/bundled_node_bin" "$runtime_dir/node/bin/node"

stage_tree qmd "$qmd_version" "$bin/third-party/qmd/runtime/node_modules"
prune_pkg "$runtime_dir/qmd/$qmd_version/node_modules" 'typescript@*'

stage_tree latchkey "$latchkey_version" "$bin/third-party/latchkey/runtime/node_modules"
prune_pkg "$runtime_dir/latchkey/$latchkey_version/node_modules" 'playwright*'

# Assert the two entry points the Rust resolver will look for actually
# resolve. Without this the staging can be subtly wrong — a moved entry,
# a prune that took too much — and the only symptom is the packaged app
# silently falling back to `npx` on a machine that may have no Node.
for entry in \
    "$runtime_dir/qmd/$qmd_version/node_modules/@tobilu/qmd/dist/cli/qmd.js" \
    "$runtime_dir/latchkey/$latchkey_version/node_modules/latchkey/dist/src/cli.js"; do
    [[ -f "$entry" ]] || fail "staged entry missing: $entry"
done

# Drop trees whose version is no longer pinned (left behind by a bump),
# so incremental build machines don't ship dead weight.
prune_stale() { # kind, live version
    local dir
    for dir in "$runtime_dir/$1"/*/; do
        [[ -d "$dir" ]] || continue
        if [[ "$(basename "$dir")" != "$2" ]]; then
            log "pruning stale $1 tree $(basename "$dir")"
            rm -rf "$dir"
        fi
    done
}
prune_stale latchkey "$latchkey_version"
prune_stale qmd "$qmd_version"

# User-facing `latchkey` launcher: bundled node + staged tree +
# LATCHKEY_CURL pointed at the bundled shim. Lands next to the sidecar
# binaries (same dir the shim is staged into by beforeBuildCommand) so
# `.../Resources/binaries/latchkey services register …` just works.
mkdir -p "$script_dir/binaries"
install -m 0755 "$script_dir/latchkey-wrapper.sh" "$script_dir/binaries/latchkey"
log "installed latchkey wrapper at binaries/latchkey"

# ---------------------------------------------------------------------------
# Codesigning (macOS release builds only).
# ---------------------------------------------------------------------------

if [[ "$(uname -s)" == "Darwin" && -n "${APPLE_SIGNING_IDENTITY:-}" ]]; then
    log "codesigning runtime (identity: $APPLE_SIGNING_IDENTITY)"
    # Preserve the JIT entitlements the upstream node binary is signed
    # with — V8 aborts under the hardened runtime without them.
    entitlements="$(mktemp -t node-entitlements.XXXXXX)"
    if codesign -d --entitlements - --xml "$runtime_dir/node/bin/node" \
        >"$entitlements" 2>/dev/null && [[ -s "$entitlements" ]]; then
        codesign --force --options runtime --timestamp \
            --entitlements "$entitlements" \
            --sign "$APPLE_SIGNING_IDENTITY" "$runtime_dir/node/bin/node"
    else
        codesign --force --options runtime --timestamp \
            --sign "$APPLE_SIGNING_IDENTITY" "$runtime_dir/node/bin/node"
    fi
    rm -f "$entitlements"
    # Every native library in the trees must be signed for notarization.
    # *.so: node-llama-cpp names its Mach-O dylibs libggml-*.so.
    # `-type f` so the pnpm store's symlinks are signed once, through
    # the real file, rather than once per link.
    find "$runtime_dir/latchkey" "$runtime_dir/qmd" \
        \( -name '*.node' -o -name '*.dylib' -o -name '*.so' \) -type f -print0 |
        while IFS= read -r -d '' lib; do
            codesign --force --options runtime --timestamp \
                --sign "$APPLE_SIGNING_IDENTITY" "$lib"
        done
fi

log "runtime staged at $runtime_dir"
