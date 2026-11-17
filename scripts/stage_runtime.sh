#!/usr/bin/env bash
#
# Stage the bundled Node runtime + the `latchkey` package tree into one
# `runtime/` directory — the tree the backend binaries resolve through
# `datalib_runtime::node_runtime` so that `latchkey` runs with NO
# Node/npm/npx on the host. The release publishes it as its
# own asset per platform (`runtime-<triple>.tar.gz`, fetched on first
# use — docs/dev/runtime_fetch.md), the docker image unpacks that asset
# beside the binaries, the .app carries it under
# `Contents/Resources/runtime/` (datalib/tauri/stage-runtime.sh calls
# this and then codesigns), and a checkout can stage one anywhere and
# point `DATALIB_RUNTIME_DIR` at it.
#
#   scripts/stage_runtime.sh <dest> [--no-symlinks]
#
# Layout staged (and expected by the Rust resolver — keep in sync):
#
#   <dest>/
#     node/bin/node                                    pinned Node
#     node/LICENSE                                     its notice
#     latchkey/<v>/node_modules/latchkey/dist/src/cli.js
#
# `--no-symlinks` rewrites each JS tree into npm's flat layout
# (scripts/hoist_node_modules.py), which has no links at all. The .app
# needs it: Tauri's resource bundler copies regular files only, and the
# pnpm layout reaches every package through a link
# (`node_modules/latchkey -> .aspect_rules_js/latchkey@<v>/…`), so a
# tree bundled with its links dropped has every byte and no entry
# script. Dereferencing the links is not a fix — Node finds a package's
# dependencies beside where it really lives, and a copy lives nowhere
# — hence the rewrite.
#
# Everything staged here comes out of Bazel. That is the whole design:
# this script downloads nothing and resolves nothing. Three targets:
#
#   //datalib/tauri:bundled_node             the rules_nodejs toolchain's
#                                            Node, NODE_VERSION in MODULE.bazel
#   //third-party:bundled_licenses           Node's LICENSE (with the rest
#                                            of the shipped notices)
#   //third-party/latchkey/runtime:latchkey_tree  lockfile-pinned, sha512
#                                                 per tarball
#
# The version pin is grepped out of the Rust source that spawns the
# tool, because it names the staged DIRECTORY and the resolver looks
# that up by the Rust constant: LATCHKEY_VERSION in
# backend/runtime/src/node_runtime.rs. `//tools:version_pins_test` holds
# it equal to the package.json the Bazel tree is built from, so a pin
# that moves in one place fails the build rather than staging a
# directory nothing will look in.
#
# Build-host requirements: bazelisk (or bazel) and rsync, plus python3
# for `--no-symlinks`. No Node, no npm, no C toolchain. The tree is
# staged for the HOST platform; cross builds are not supported.

set -euo pipefail

usage() { echo "usage: $0 <dest> [--no-symlinks]" >&2; exit 2; }

runtime_dir=""
no_symlinks=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --no-symlinks) no_symlinks=1; shift ;;
        -*) usage ;;
        *)
            [[ -z "$runtime_dir" ]] || usage
            runtime_dir="$1"; shift ;;
    esac
done
[[ -n "$runtime_dir" ]] || usage

# Absolute: the smoke test below runs from inside the latchkey package,
# where a relative <dest> names nothing.
absolute_dir() { mkdir -p "$1" && (cd -- "$1" && pwd -P); }
runtime_dir="$(absolute_dir "$runtime_dir")"

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$script_dir/.."
backend_dir="$repo_root/datalib/backend"

log() { printf '>>> stage_runtime: %s\n' "$*" >&2; }
fail() { printf 'stage_runtime: error: %s\n' "$*" >&2; exit 1; }

command -v rsync >/dev/null 2>&1 || fail "rsync not found on PATH"
[[ -z "$no_symlinks" ]] || command -v python3 >/dev/null 2>&1 || fail "python3 not found on PATH (--no-symlinks needs it)"

# ---------------------------------------------------------------------------
# The version pin, grepped from the Rust source (see header).
# ---------------------------------------------------------------------------

extract_pin() { # file, pattern of the const line
    local v
    v="$(grep -E -m1 "$2" "$1" | sed -E 's/.*"([^"]+)".*/\1/')"
    [[ -n "$v" ]] || fail "could not extract version pin from $1 (pattern: $2)"
    printf '%s' "$v"
}

latchkey_version="$(extract_pin "$backend_dir/runtime/src/node_runtime.rs" \
    '^pub const LATCHKEY_VERSION:')"

log "pin: latchkey=$latchkey_version"

# ---------------------------------------------------------------------------
# Build the Bazel targets and locate their outputs.
# ---------------------------------------------------------------------------

if [[ -n "${STAGE_RUNTIME_BAZEL_BIN:-}" ]]; then
    # //tools:stage_runtime_test hands the targets over as
    # runfiles, laid out the way bazel-bin lays them out.
    bin="$STAGE_RUNTIME_BAZEL_BIN"
else
    if command -v bazelisk >/dev/null 2>&1; then
        bazel=bazelisk
    elif command -v bazel >/dev/null 2>&1; then
        bazel=bazel
    else
        fail "neither bazelisk nor bazel found on PATH"
    fi
    log "building runtime targets"
    (cd "$repo_root" && "$bazel" build \
        //datalib/tauri:bundled_node \
        //third-party:bundled_licenses \
        //third-party/latchkey/runtime:latchkey_tree >&2)
    bin="$(cd "$repo_root" && "$bazel" info bazel-bin)"
fi

# ---------------------------------------------------------------------------
# Stage.
# ---------------------------------------------------------------------------

# rsync rather than cp: `-a` keeps the pnpm store's relative symlinks as
# symlinks (`--no-symlinks` rewrites the tree afterwards, once every
# prune is done), `--delete` clears whatever a previous stage left
# behind. `--copy-unsafe-links` is for a runfiles tree, where every
# package directory is an absolute link into bazel-out: those are
# copied for real, so the staged tree is one, while bazel-bin's own
# links are all relative and untouched. The chmod is not rsync's
# `--chmod` because macOS's own rsync (openrsync, since macOS 15) accepts
# that and ignores it on a tree; Bazel's outputs are read-only, and
# the smoke below and codesign both write here.
stage_tree() { # kind, version, source node_modules dir
    local dest="$runtime_dir/$1/$2/node_modules"
    log "staging $1@$2"
    mkdir -p "$dest"
    rsync -a --copy-unsafe-links --delete "$3/" "$dest/"
    chmod -R u+w "$dest"
}

log "staging node"
mkdir -p "$runtime_dir/node/bin"
rsync -aL "$bin/datalib/tauri/bundled_node_bin" "$runtime_dir/node/bin/node"
chmod u+wx "$runtime_dir/node/bin/node"
# Node's own notice travels with the binary; the release's full set of
# third-party notices is scripts/third_party_notices.sh's job.
rsync -aL "$bin/third-party/bundled_licenses/node/LICENSE" "$runtime_dir/node/LICENSE"
chmod u+w "$runtime_dir/node/LICENSE"

stage_tree latchkey "$latchkey_version" "$bin/third-party/latchkey/runtime/node_modules"

# Drop trees whose version is no longer pinned (left behind by a bump),
# so incremental build machines don't ship dead weight. Before the
# symlink count below, which a stale pnpm tree would fail.
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
# A tree staged by a build that still bundled qmd.
if [[ -d "$runtime_dir/qmd" ]]; then
    log "pruning qmd, which the runtime no longer carries"
    rm -rf "$runtime_dir/qmd"
fi

# After every prune, so a dropped package is not carried into the flat
# tree. The smoke test below then runs against the tree that ships.
if [[ -n "$no_symlinks" ]]; then
    python3 "$script_dir/hoist_node_modules.py" "$runtime_dir/latchkey/$latchkey_version/node_modules"
    links="$(find "$runtime_dir" -type l | wc -l | tr -d ' ')"
    [[ "$links" == 0 ]] || fail "$links symlinks survived --no-symlinks"
fi

# Assert the entry point the Rust resolver will look for actually
# resolves. Without this the staging can be subtly wrong — a moved
# entry, a rewrite that took too much — and the only symptom is the
# binaries refusing to run latchkey.
entry="$runtime_dir/latchkey/$latchkey_version/node_modules/latchkey/dist/src/cli.js"
[[ -f "$entry" ]] || fail "staged entry missing: $entry"

# latchkey imports playwright lazily, so a tree without it passes the
# entry check above and still fails every browser login. Resolved from
# inside the real package directory, as latchkey's own import is.
latchkey_pkg="$(cd -P "$runtime_dir/latchkey/$latchkey_version/node_modules/latchkey" && pwd -P)"
smoke="$latchkey_pkg/.stage_runtime_smoke.mjs"
echo 'await import("playwright");' > "$smoke"
log "smoke: latchkey resolves playwright"
smoke_ok=1
"$runtime_dir/node/bin/node" "$smoke" || smoke_ok=""
rm -f "$smoke"
[[ -n "$smoke_ok" ]] || fail "the staged latchkey tree cannot import playwright; browser logins would fail"

log "runtime staged at $runtime_dir"
