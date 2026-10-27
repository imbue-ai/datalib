#!/usr/bin/env bash
# Ensure `pnpm` is on PATH, bootstrapping via `corepack` when missing.
#
# Why this exists
# ---------------
# `datalib/ui/` uses pnpm (`pnpm-lock.yaml` is checked in, and
# `package.json` pins a specific version via the `packageManager`
# field). Nothing about the UI _requires_ pnpm — no workspaces, no
# pnpm-specific lockfile features — but pnpm is what the lockfile is
# keyed against, so swapping to npm/yarn would mean regenerating it.
#
# `corepack` reads `package.json`'s `packageManager` field to provision
# the exact pinned pnpm version on demand. Node 16.9 through 24 bundle
# it; Node 25 and later do not, so on those it is a separate install
# (`brew install corepack` or `npm install -g corepack`).
#
# Usage:
#   source scripts/ensure_pnpm.sh
# After sourcing, `pnpm` is on PATH (or the script exits with a clear
# error pointing at the right install path).

set -eo pipefail

if command -v pnpm >/dev/null 2>&1; then
    return 0 2>/dev/null || exit 0
fi

# A shim `corepack enable` wrote points into the Node install that
# shipped corepack, so upgrading that Node past 24 leaves it dangling,
# and `command -v` skips it without a word.
_ensure_pnpm_broken=""
IFS=: read -ra _ensure_pnpm_dirs <<<"$PATH"
for _ensure_pnpm_dir in "${_ensure_pnpm_dirs[@]}"; do
    _ensure_pnpm_shim="${_ensure_pnpm_dir:-.}/pnpm"
    if [[ -L "$_ensure_pnpm_shim" && ! -e "$_ensure_pnpm_shim" ]]; then
        _ensure_pnpm_broken+=$'\n'"  $_ensure_pnpm_shim -> $(readlink "$_ensure_pnpm_shim")"
    fi
done
unset _ensure_pnpm_dirs _ensure_pnpm_dir _ensure_pnpm_shim

_ensure_pnpm_report_broken() {
    if [[ -n "$_ensure_pnpm_broken" ]]; then
        cat >&2 <<EOF

A pnpm shim on PATH points at a file that no longer exists, most
likely left by \`corepack enable\` under a Node that has since been
upgraded:$_ensure_pnpm_broken
Remove it (\`rm <path>\`) before enabling corepack again.
EOF
    fi
}

if ! command -v corepack >/dev/null 2>&1; then
    cat >&2 <<'EOF'
ERROR: neither `pnpm` nor `corepack` on PATH.

The datalib UI uses pnpm (pinned via `packageManager` in
datalib/ui/package.json), provisioned through corepack. Node 25 and
later no longer bundle corepack; install it on its own:

  brew install corepack      # or: npm install -g corepack

then re-run. (Node 16.9–24 bundle it: if you use one of those, make
sure its bin dir is on PATH.)

If you really don't want corepack: `npm install -g pnpm`.
EOF
    _ensure_pnpm_report_broken
    exit 1
fi

echo "  bootstrapping pnpm via corepack..." >&2
# `corepack enable pnpm` writes a pnpm shim into the dir containing
# `corepack` (typically ~/.nvm/versions/node/<v>/bin/ or /opt/homebrew/bin).
# The shim itself defers to whatever version `packageManager` pins, so
# the first actual `pnpm <cmd>` invocation may print a one-time download
# notice. After that it's cached in `~/.cache/corepack`.
corepack enable pnpm >/dev/null

if ! command -v pnpm >/dev/null 2>&1; then
    echo "ERROR: corepack ran but pnpm still not on PATH." >&2
    echo "       Try: corepack prepare pnpm@latest --activate" >&2
    _ensure_pnpm_report_broken
    exit 1
fi

unset _ensure_pnpm_broken
unset -f _ensure_pnpm_report_broken
