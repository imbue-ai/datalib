#!/usr/bin/env bash
#
# Stage the bundled Node runtime + JS package trees (`latchkey`, `qmd`)
# into one `runtime/` directory — the tree the backend binaries resolve
# through `datalib_runtime::node_runtime` so that `latchkey` and `qmd`
# run with NO Node/npm/npx on the host. The release publishes it as its
# own asset per platform (`runtime-<triple>.tar.gz`, fetched on first
# use — docs/dev/runtime_fetch.md), the docker image unpacks that asset
# beside the binaries, the .app carries it under
# `Contents/Resources/runtime/` (datalib/tauri/stage-runtime.sh calls
# this and then codesigns), and a checkout can stage one anywhere and
# point `DATALIB_RUNTIME_DIR` at it.
#
#   scripts/stage_runtime.sh <dest> [--cuda <cuda-dest>] [--no-symlinks]
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
# Only THIS platform's CPU binding of node-llama-cpp is kept under
# <dest>: pnpm links every optional `@node-llama-cpp/*` package the
# lockfile names for the host OS, which on Linux x86_64 is 600 MB of
# CUDA and Vulkan backends plus arm builds that cannot run here. With
# `--cuda`, the two CUDA packages go to <cuda-dest> instead of being
# dropped, laid out so that unpacking it over <dest> restores them
# (the `-cuda` release asset). Vulkan is dropped: it is the backend
# qmd's own fallback exists for, throwing at init on driverless
# machines, and nobody has asked for it.
#
# `--no-symlinks` rewrites each JS tree into npm's flat layout
# (scripts/hoist_node_modules.py), which has no links at all. The .app
# needs it: Tauri's resource bundler copies regular files only, and the
# pnpm layout reaches every package through a link
# (`node_modules/latchkey -> .aspect_rules_js/latchkey@<v>/…`), so a
# tree bundled with its links dropped has every byte and no entry
# script. Dereferencing the links is not a fix — Node finds a package's
# dependencies beside where it really lives, and a copy lives nowhere
# — hence the rewrite. Not combinable with `--cuda`, whose overlay is
# laid out for the store.
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
# Build-host requirements: bazelisk (or bazel) and rsync, plus python3
# for `--no-symlinks`. No Node, no npm, no C toolchain — the native modules arrive prebuilt inside their
# npm tarballs (see the better-sqlite3 13 note in MODULE.bazel). Trees
# are staged for the HOST platform; cross builds are not supported.

set -euo pipefail

usage() { echo "usage: $0 <dest> [--cuda <cuda-dest>] [--no-symlinks]" >&2; exit 2; }

runtime_dir=""
cuda_dir=""
no_symlinks=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --cuda)
            [[ $# -ge 2 ]] || usage
            cuda_dir="$2"; shift 2 ;;
        --no-symlinks) no_symlinks=1; shift ;;
        -*) usage ;;
        *)
            [[ -z "$runtime_dir" ]] || usage
            runtime_dir="$1"; shift ;;
    esac
done
[[ -n "$runtime_dir" ]] || usage
[[ -z "$no_symlinks" || -z "$cuda_dir" ]] || usage

# Both destinations absolute: the smoke test below runs from inside
# the qmd package, where a relative <dest> names nothing.
absolute_dir() { mkdir -p "$1" && (cd -- "$1" && pwd -P); }
runtime_dir="$(absolute_dir "$runtime_dir")"
[[ -z "$cuda_dir" ]] || cuda_dir="$(absolute_dir "$cuda_dir")"

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$script_dir/.."
backend_dir="$repo_root/datalib/backend"

log() { printf '>>> stage_runtime: %s\n' "$*" >&2; }
fail() { printf 'stage_runtime: error: %s\n' "$*" >&2; exit 1; }

command -v rsync >/dev/null 2>&1 || fail "rsync not found on PATH"
[[ -z "$no_symlinks" ]] || command -v python3 >/dev/null 2>&1 || fail "python3 not found on PATH (--no-symlinks needs it)"

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

if [[ -n "${STAGE_RUNTIME_BAZEL_BIN:-}" ]]; then
    # //tools:stage_runtime_test hands the four targets over as
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
        //third-party/qmd/runtime:qmd_tree \
        //third-party/latchkey/runtime:latchkey_tree >&2)
    bin="$(cd "$repo_root" && "$bazel" info bazel-bin)"
fi

# ---------------------------------------------------------------------------
# Stage.
# ---------------------------------------------------------------------------

# rsync rather than cp: `-a` keeps the pnpm store's relative symlinks as
# symlinks (`--no-symlinks` rewrites the tree afterwards, once every
# prune is done), `--delete` clears whatever a previous stage left
# behind, and `--chmod` makes the copy writable since Bazel's outputs
# are read-only and codesign has to rewrite them. `--copy-unsafe-links`
# is for a runfiles tree, where every package directory is an absolute
# link into bazel-out: those are copied for real, so the staged tree
# is one, while bazel-bin's own links are all relative and untouched.
stage_tree() { # kind, version, source node_modules dir
    local dest="$runtime_dir/$1/$2/node_modules"
    log "staging $1@$2"
    mkdir -p "$dest"
    rsync -a --copy-unsafe-links --delete --chmod=Du+wx,Fu+w "$3/" "$dest/"
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
rsync -aL --chmod=u+wx "$bin/datalib/tauri/bundled_node_bin" "$runtime_dir/node/bin/node"
# Node's own notice travels with the binary; the release's full set of
# third-party notices is scripts/third_party_notices.sh's job.
rsync -aL --chmod=u+w "$bin/third-party/bundled_licenses/node/LICENSE" "$runtime_dir/node/LICENSE"

stage_tree qmd "$qmd_version" "$bin/third-party/qmd/runtime/node_modules"
prune_pkg "$runtime_dir/qmd/$qmd_version/node_modules" 'typescript@*'

stage_tree latchkey "$latchkey_version" "$bin/third-party/latchkey/runtime/node_modules"
prune_pkg "$runtime_dir/latchkey/$latchkey_version/node_modules" 'playwright*'

# ---------------------------------------------------------------------------
# The node-llama-cpp platform filter.
# ---------------------------------------------------------------------------

# The `@node-llama-cpp/<name>` package node-llama-cpp imports for this
# host on the CPU path, and the CUDA pair it imports instead when told
# to (`getPrebuiltBinariesPackageDirectoryForBuildOptions` in its
# dist/bindings/utils/compileLLamaCpp.js). Every other binding package
# is dead weight here.
case "$(uname -s)/$(uname -m)" in
    Linux/x86_64)  cpu_binding=linux-x64; cuda_bindings="linux-x64-cuda linux-x64-cuda-ext" ;;
    Linux/aarch64) cpu_binding=linux-arm64; cuda_bindings="" ;;
    Darwin/arm64)  cpu_binding=mac-arm64-metal; cuda_bindings="" ;;
    Darwin/x86_64) cpu_binding=mac-x64; cuda_bindings="" ;;
    *) fail "no node-llama-cpp binding known for $(uname -s)/$(uname -m)" ;;
esac

qmd_modules="$runtime_dir/qmd/$qmd_version/node_modules"
store="$qmd_modules/.aspect_rules_js"

# A binding package lives in the store as `@node-llama-cpp+<name>@<v>`
# and is reached from node-llama-cpp's own node_modules through a
# symlink `@node-llama-cpp/<name>`; both are what `--cuda` carries
# across, and both are what the prune below removes.
binding_paths() { # name → the store dir and the link(s), relative to node_modules
    local name="$1"
    (cd "$qmd_modules" && find .aspect_rules_js -maxdepth 1 -name "@node-llama-cpp+$name@*" \
        && find .aspect_rules_js -maxdepth 4 -path "*/node_modules/@node-llama-cpp/$name" -type l)
}

if [[ -n "$cuda_dir" && -n "$cuda_bindings" ]]; then
    cuda_modules="$cuda_dir/qmd/$qmd_version/node_modules"
    log "staging the CUDA bindings to $cuda_dir"
    rm -rf "$cuda_dir"
    mkdir -p "$cuda_modules"
    for name in $cuda_bindings; do
        binding_paths "$name" | while IFS= read -r rel; do
            rsync -aR --chmod=Du+wx,Fu+w "$qmd_modules/./$rel" "$cuda_modules/"
        done
    done
elif [[ -n "$cuda_dir" ]]; then
    log "no CUDA bindings for this platform; $cuda_dir not staged"
fi

kept=0
for dir in "$store"/@node-llama-cpp+*; do
    [[ -e "$dir" ]] || continue
    name="$(basename "$dir")"; name="${name#@node-llama-cpp+}"; name="${name%@*}"
    if [[ "$name" == "$cpu_binding" ]]; then
        kept=1
        continue
    fi
    log "dropping node-llama-cpp binding $name"
    rm -rf "$dir"
done
[[ "$kept" == 1 ]] || fail "the $cpu_binding binding is not in the staged qmd tree"
# The links into the dropped store dirs now dangle; sweep them.
find "$qmd_modules" -type l ! -exec test -e {} \; -exec rm -f {} + 2>/dev/null || true

# After every prune, so a dropped package is not carried into the flat
# tree. The smoke test below then runs against the tree that ships.
if [[ -n "$no_symlinks" ]]; then
    python3 "$script_dir/hoist_node_modules.py" "$qmd_modules"
    python3 "$script_dir/hoist_node_modules.py" "$runtime_dir/latchkey/$latchkey_version/node_modules"
    links="$(find "$runtime_dir" -type l | wc -l | tr -d ' ')"
    [[ "$links" == 0 ]] || fail "$links symlinks survived --no-symlinks"
fi

# Assert the two entry points the Rust resolver will look for actually
# resolve. Without this the staging can be subtly wrong — a moved entry,
# a prune that took too much — and the only symptom is the binaries
# refusing to run either tool.
for entry in \
    "$runtime_dir/qmd/$qmd_version/node_modules/@tobilu/qmd/dist/cli/qmd.js" \
    "$runtime_dir/latchkey/$latchkey_version/node_modules/latchkey/dist/src/cli.js"; do
    [[ -f "$entry" ]] || fail "staged entry missing: $entry"
done

# Prove the binding loads after the prune, not just that its files are
# there: `getLlama` with `build: "never"` either opens a prebuilt
# library or throws, and never reaches for cmake. `gpu: "auto"` is what
# qmd asks for — Metal on a mac, else the CPU binding once the pruned
# GPU packages fail to import. The script sits inside the real package
# directory so the bare `node-llama-cpp` import resolves the way qmd's
# own does, and it is a file rather than `node -e`: on Linux
# node-llama-cpp probes a prebuilt binding by fork()ing a child that
# inherits `process.execArgv`, and a child started with
# `--input-type=module` refuses to run a file.
qmd_pkg="$(cd -P "$qmd_modules/@tobilu/qmd" && pwd -P)"
smoke="$qmd_pkg/.stage_runtime_smoke.mjs"
cat > "$smoke" <<'EOF'
const { getLlama } = await import("node-llama-cpp");
const llama = await getLlama({ build: "never", gpu: "auto", progressLogs: false, logLevel: "error" });
console.error(`>>> stage_runtime: node-llama-cpp loaded (gpu=${llama.gpu}, ${llama.cpuMathCores} math cores)`);
await llama.dispose();
EOF
log "smoke: loading the $cpu_binding binding"
smoke_ok=1
"$runtime_dir/node/bin/node" "$smoke" || smoke_ok=""
rm -f "$smoke"
[[ -n "$smoke_ok" ]] || fail "the staged node-llama-cpp binding does not load"

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
