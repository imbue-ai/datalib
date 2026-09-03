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
# Package versions are NOT pinned in this file: they're grepped out of
# the Rust sources that spawn the tools (single source of truth, can't
# drift):
#   * latchkey  — LATCHKEY_VERSION in backend/core/src/node_runtime.rs
#                 (the ONE canonical latchkey pin; etl re-exports it)
#   * qmd       — DEFAULT_QMD_VERSION in backend/unified_index/src/qmd/mod.rs
#                 (the ONE canonical qmd pin; indexer re-exports it,
#                 //tools:version_pins_test guards the rest)
#
# Build-host requirements: curl, tar, and (for qmd's native deps —
# better-sqlite3, tree-sitter grammars) a C/C++ toolchain + python3.
# npm itself comes from the downloaded Node dist, so the host needs no
# Node install. Native modules are built for the HOST platform — cross
# builds are not supported (same restriction as the rest of the tauri
# build).
#
# Signing: when $APPLE_SIGNING_IDENTITY is set (same convention as
# tauri.conf.json's beforeBuildCommand), the node binary and every
# native library in the trees is codesigned with the hardened runtime.
# `node` additionally keeps the JIT entitlements extracted from the
# upstream-signed binary — V8 won't start under the hardened runtime
# without them.
#
# Idempotent: each component is stamped and re-staged only when its
# pinned version changes. Delete runtime/ to force a full re-stage.

set -euo pipefail

# Node LTS to bundle. qmd needs >=22, latchkey >=20.
#
# The sha256 of every dist we fetch is pinned in the platform `case`
# below and verified before extraction. This is not a normal build
# dependency: `node` is codesigned with the Developer ID further down
# and ships inside Datalib.app, so an unverified download would go out to
# users under our signature, notarized (issue #141).
#
# Pinned digests rather than a fetched SHASUMS256.txt on purpose — that
# file comes from the same origin as the tarball, so it adds nothing
# against a compromised origin. Same discipline as
# `hack/doltlite_fork_bug/run.sh`.
#
# Bumping NODE_VERSION means re-pinning ALL FOUR digests. Get them with:
#   curl -fsSL https://nodejs.org/dist/<version>/SHASUMS256.txt \
#     | grep -E '(darwin-(arm64|x64)|linux-(arm64|x64))\.tar\.gz$'
NODE_VERSION="v22.23.1"

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
backend_dir="$script_dir/../backend"
runtime_dir="$script_dir/runtime"
cache_dir="$script_dir/.runtime-cache"

log() { printf '>>> stage-runtime: %s\n' "$*" >&2; }
fail() { printf 'stage-runtime: error: %s\n' "$*" >&2; exit 1; }

command -v curl >/dev/null 2>&1 || fail "curl not found on PATH"
command -v tar >/dev/null 2>&1 || fail "tar not found on PATH"

# ---------------------------------------------------------------------------
# Version pins, grepped from the Rust sources (see header).
# ---------------------------------------------------------------------------

extract_pin() { # file, pattern of the const line
    local v
    v="$(grep -E "$2" "$1" | sed -E 's/.*"([^"]+)".*/\1/' | head -n1)"
    [[ -n "$v" ]] || fail "could not extract version pin from $1 (pattern: $2)"
    printf '%s' "$v"
}

latchkey_version="$(extract_pin "$backend_dir/core/src/node_runtime.rs" \
    '^pub const LATCHKEY_VERSION:')"
qmd_version="$(extract_pin "$backend_dir/unified_index/src/qmd/mod.rs" \
    '^pub const DEFAULT_QMD_VERSION:')"

log "pins: node=$NODE_VERSION latchkey=$latchkey_version qmd=$qmd_version"

# ---------------------------------------------------------------------------
# Platform → Node dist name.
# ---------------------------------------------------------------------------

# Platform -> (dist name, sha256 of that dist's tarball for
# $NODE_VERSION). A `case` rather than an associative array so this keeps
# working under the bash 3.2 that macOS still ships.
case "$(uname -s)-$(uname -m)" in
    Darwin-arm64)
        node_platform="darwin-arm64"
        node_sha256="ef28d8fab2c0e4314522d4bb1b7173270aa3937e93b92cb7de79c112ac1fa953"
        ;;
    Darwin-x86_64)
        node_platform="darwin-x64"
        node_sha256="b8da981b8a0b1241b70249204916da76c63573ddf5814dbd2d1e41069105cb81"
        ;;
    Linux-aarch64 | Linux-arm64)
        node_platform="linux-arm64"
        node_sha256="543fa39e57d4c07855939459a323f4deb9a79dd1bb45e6e99458b0f2de10db8d"
        ;;
    Linux-x86_64)
        node_platform="linux-x64"
        node_sha256="7a8cb04b4a1df4eaf432125324b81b29a088e73570a23259a8de1c65d07fc129"
        ;;
    *) fail "unsupported platform: $(uname -s)-$(uname -m)" ;;
esac

# sha256 of a file, via whichever tool this host has. macOS ships
# `shasum`; most Linux images ship `sha256sum`; CI images vary.
sha256_of() {
    if command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$1" | awk '{print $1}'
    elif command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | awk '{print $1}'
    else
        fail "neither shasum nor sha256sum found; cannot verify downloads"
    fi
}

# ---------------------------------------------------------------------------
# Node dist: download once into the cache, keep npm there for installs,
# ship only bin/node.
# ---------------------------------------------------------------------------

node_dist="node-$NODE_VERSION-$node_platform"
node_dist_dir="$cache_dir/$node_dist"
if [[ ! -x "$node_dist_dir/bin/node" ]]; then
    mkdir -p "$cache_dir"
    tarball="$cache_dir/$node_dist.tar.gz"
    log "downloading $node_dist"
    curl -fsSL -o "$tarball" "https://nodejs.org/dist/$NODE_VERSION/$node_dist.tar.gz"

    # Fail closed, and BEFORE extracting: this binary gets codesigned and
    # shipped. Leave the tarball in place on mismatch so it can be
    # inspected rather than silently re-downloaded on the next run.
    got_sha256="$(sha256_of "$tarball")"
    if [[ "$got_sha256" != "$node_sha256" ]]; then
        fail "$(printf 'sha256 mismatch for %s\n  want: %s\n  got:  %s\nLeft at %s. Do NOT stage or sign this. If nodejs.org re-rolled the release in place, verify against SHASUMS256.txt and update the pin in this script.' \
            "$node_dist.tar.gz" "$node_sha256" "$got_sha256" "$tarball")"
    fi
    log "verified $node_dist.tar.gz (sha256 $node_sha256)"

    tar -xzf "$tarball" -C "$cache_dir"
    rm -f "$tarball"
    [[ -x "$node_dist_dir/bin/node" ]] || fail "node dist extraction failed"
fi
node_bin="$node_dist_dir/bin/node"
npm_cli="$node_dist_dir/lib/node_modules/npm/bin/npm-cli.js"
[[ -f "$npm_cli" ]] || fail "npm not found in node dist at $npm_cli"

node_stamp="$runtime_dir/node/VERSION"
if [[ ! -f "$node_stamp" || "$(cat "$node_stamp")" != "$NODE_VERSION-$node_platform" ]]; then
    log "staging node $NODE_VERSION ($node_platform)"
    rm -rf "$runtime_dir/node"
    mkdir -p "$runtime_dir/node/bin"
    cp -f "$node_bin" "$runtime_dir/node/bin/node"
    chmod 755 "$runtime_dir/node/bin/node"
    printf '%s' "$NODE_VERSION-$node_platform" >"$node_stamp"
else
    log "node $NODE_VERSION already staged"
fi

# ---------------------------------------------------------------------------
# npm package trees.
# ---------------------------------------------------------------------------

# npm_install <tree_dir> <pkg_spec> [extra npm-install flags...]
npm_install() {
    local tree="$1" spec="$2"
    shift 2
    rm -rf "$tree"
    mkdir -p "$tree"
    printf '{"private": true}\n' >"$tree/package.json"
    # PATH: postinstall scripts (node-gyp, prebuild-install) must find
    # the staging node, not whatever the host has.
    # --omit=peer: npm 7+ auto-installs peer deps; the only one in
    # these trees is qmd's `typescript` (~23MB), which its CLI never
    # imports at runtime (dev-time tsx/typechecking convenience).
    (cd "$tree" && PATH="$(dirname "$node_bin"):$PATH" \
        "$node_bin" "$npm_cli" install \
        --omit=dev --omit=peer --no-save --no-package-lock --no-audit --no-fund \
        --loglevel=error "$@" "$spec")
}

stage_tree() { # kind, version, pkg_spec, entry_rel, extra flags...
    local kind="$1" version="$2" spec="$3" entry_rel="$4"
    shift 4
    local tree="$runtime_dir/$kind/$version"
    local stamp="$tree/.staged"
    if [[ -f "$stamp" && "$(cat "$stamp")" == "$spec" ]]; then
        log "$spec already staged"
        return
    fi
    log "staging $spec"
    npm_install "$tree" "$spec" "$@"
    [[ -f "$tree/$entry_rel" ]] || fail "$spec staged but entry $entry_rel is missing"
    printf '%s' "$spec" >"$stamp"
}

# latchkey: --ignore-scripts skips playwright's browser download (and
# nothing in the tree needs an install script — @napi-rs/keyring ships
# prebuilt platform packages). Playwright itself is pruned below: the
# only latchkey features that need it are browser-login flows
# (`latchkey auth browser`), which datalib never invokes, and
# latchkey degrades gracefully when the import fails (same behavior as
# its own bun-compiled release binaries, which mark playwright
# external).
stage_tree latchkey "$latchkey_version" "latchkey@$latchkey_version" \
    "node_modules/latchkey/dist/src/cli.js" --ignore-scripts
latchkey_tree="$runtime_dir/latchkey/$latchkey_version"
if [[ -d "$latchkey_tree/node_modules/playwright" || -d "$latchkey_tree/node_modules/playwright-core" ]]; then
    log "pruning playwright from latchkey tree"
    rm -rf "$latchkey_tree/node_modules/playwright" \
        "$latchkey_tree/node_modules/playwright-core" \
        "$latchkey_tree/node_modules/@playwright" \
        "$latchkey_tree/node_modules/chromium-bidi" \
        "$latchkey_tree/node_modules/electron"
    rm -f "$latchkey_tree/node_modules/.bin/playwright" \
        "$latchkey_tree/node_modules/.bin/playwright-core"
    # Re-stamp: the prune is part of the staged state.
    printf '%s' "latchkey@$latchkey_version" >"$latchkey_tree/.staged"
fi

# qmd: install scripts are left enabled here. As of qmd 2.8.3 nothing in
# the tree is known to need one — better-sqlite3 13 ships its bindings in
# the tarball, the tree-sitter grammars ship `prebuilds/<os>-<arch>/`, and
# node-llama-cpp's platform binary arrives as a prebuilt optional dep —
# but this path is a real `npm install` against the registry, not the
# Bazel lockfile, so it is not the place to assert that.
stage_tree qmd "$qmd_version" "@tobilu/qmd@$qmd_version" \
    "node_modules/@tobilu/qmd/dist/cli/qmd.js"

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

if [[ "$node_platform" == darwin-* && -n "${APPLE_SIGNING_IDENTITY:-}" ]]; then
    log "codesigning runtime (identity: $APPLE_SIGNING_IDENTITY)"
    # Preserve the JIT entitlements the upstream node binary is signed
    # with — V8 aborts under the hardened runtime without them.
    entitlements="$cache_dir/node-entitlements.plist"
    if codesign -d --entitlements - --xml "$runtime_dir/node/bin/node" \
        >"$entitlements" 2>/dev/null && [[ -s "$entitlements" ]]; then
        codesign --force --options runtime --timestamp \
            --entitlements "$entitlements" \
            --sign "$APPLE_SIGNING_IDENTITY" "$runtime_dir/node/bin/node"
    else
        codesign --force --options runtime --timestamp \
            --sign "$APPLE_SIGNING_IDENTITY" "$runtime_dir/node/bin/node"
    fi
    # Every native library in the trees must be signed for notarization.
    # *.so: node-llama-cpp names its Mach-O dylibs libggml-*.so.
    find "$runtime_dir/latchkey" "$runtime_dir/qmd" \
        \( -name '*.node' -o -name '*.dylib' -o -name '*.so' \) -type f -print0 |
        while IFS= read -r -d '' lib; do
            codesign --force --options runtime --timestamp \
                --sign "$APPLE_SIGNING_IDENTITY" "$lib"
        done
fi

log "runtime staged at $runtime_dir"
