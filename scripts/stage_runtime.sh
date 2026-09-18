#!/usr/bin/env bash
#
# Stage the bundled Node runtime + JS package trees (`latchkey`, `qmd`)
# into one `runtime/` directory — the tree the backend binaries resolve
# through `datalib_runtime::node_runtime` so that `latchkey` and `qmd`
# run with NO Node/npm/npx on the host. The release tarball carries it
# beside the binaries (`datalib-<v>-<triple>/runtime/`), the .app under
# `Contents/Resources/runtime/` (datalib/tauri/stage-runtime.sh calls
# this and then codesigns), and a checkout can stage one anywhere and
# point `DATALIB_RUNTIME_DIR` at it.
#
#   scripts/stage_runtime.sh <dest>
#
# Layout staged (and expected by the Rust resolver — keep in sync):
#
#   <dest>/
#     node/bin/node                                    pinned Node
#     node/LICENSE                                     its notice
#     latchkey/<v>/node_modules/latchkey/dist/src/cli.js
#     qmd/<v>/node_modules/@tobilu/qmd/dist/cli/qmd.js   (one tree per
#                                                         distinct pin)
#
# Everything staged here comes out of Bazel. That is the whole design:
# this script downloads nothing and resolves nothing. Four targets:
#
#   //datalib/tauri:bundled_node             the rules_nodejs toolchain's
#                                            Node, NODE_VERSION in MODULE.bazel
#   //third-party:bundled_licenses           Node's LICENSE (with the rest
#                                            of the shipped notices)
#   //third-party/qmd/runtime:qmd_tree       lockfile-pinned, sha512 per tarball
#   //third-party/latchkey/runtime:latchkey_tree            likewise
#
# The version pins are grepped out of the Rust sources that spawn the
# tools, because they name the staged DIRECTORIES and the resolver
# looks those up by the Rust constant:
#   * latchkey  — LATCHKEY_VERSION in backend/runtime/src/node_runtime.rs
#   * qmd       — DEFAULT_QMD_VERSION in backend/runtime/src/qmd.rs
# `//tools:version_pins_test` holds each equal to the package.json that
# its Bazel tree is built from, so a pin that moves in one place fails
# the build rather than staging a directory nothing will look in.
#
# Build-host requirements: bazelisk (or bazel) and rsync. No Node, no
# npm, no C toolchain — the native modules arrive prebuilt inside their
# npm tarballs (see the better-sqlite3 13 note in MODULE.bazel). Trees
# are staged for the HOST platform; cross builds are not supported.

set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <dest>" >&2
    exit 2
fi
runtime_dir="$1"

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$script_dir/.."
backend_dir="$repo_root/datalib/backend"

log() { printf '>>> stage_runtime: %s\n' "$*" >&2; }
fail() { printf 'stage_runtime: error: %s\n' "$*" >&2; exit 1; }

if command -v bazelisk >/dev/null 2>&1; then
    bazel=bazelisk
elif command -v bazel >/dev/null 2>&1; then
    bazel=bazel
else
    fail "neither bazelisk nor bazel found on PATH"
fi
command -v rsync >/dev/null 2>&1 || fail "rsync not found on PATH"

# ---------------------------------------------------------------------------
# Version pins, grepped from the Rust sources (see header).
# ---------------------------------------------------------------------------

extract_pin() { # file, pattern of the const line
    local v
    v="$(grep -E -m1 "$2" "$1" | sed -E 's/.*"([^"]+)".*/\1/')"
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
(cd "$repo_root" && "$bazel" build \
    //datalib/tauri:bundled_node \
    //third-party:bundled_licenses \
    //third-party/qmd/runtime:qmd_tree \
    //third-party/latchkey/runtime:latchkey_tree >&2)

bin="$(cd "$repo_root" && "$bazel" info bazel-bin)"

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
# pointing into it:
#
#   * typescript is qmd's only peer dependency, ~23 MB, and its CLI
#     never imports it at runtime (dev-time tsx/typechecking).
#   * playwright (with playwright-core, ~17 MB of a 28 MB tree) backs
#     latchkey's browser-login flows, which datalib never invokes;
#     latchkey degrades gracefully when the import fails, the same way
#     its own bun-compiled release binaries do.
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
# Node's own notice travels with the binary; the release's full set of
# third-party notices is scripts/third_party_notices.sh's job.
rsync -a --chmod=u+w "$bin/third-party/bundled_licenses/node/LICENSE" "$runtime_dir/node/LICENSE"

stage_tree qmd "$qmd_version" "$bin/third-party/qmd/runtime/node_modules"
prune_pkg "$runtime_dir/qmd/$qmd_version/node_modules" 'typescript@*'

stage_tree latchkey "$latchkey_version" "$bin/third-party/latchkey/runtime/node_modules"
prune_pkg "$runtime_dir/latchkey/$latchkey_version/node_modules" 'playwright*'

# Assert the two entry points the Rust resolver will look for actually
# resolve. Without this the staging can be subtly wrong — a moved entry,
# a prune that took too much — and the only symptom is the binaries
# refusing to run either tool.
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

log "runtime staged at $runtime_dir"
