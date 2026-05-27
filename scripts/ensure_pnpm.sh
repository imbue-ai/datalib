#!/usr/bin/env bash
# Ensure `pnpm` is on PATH, bootstrapping via `corepack` when missing.
#
# Why this exists
# ---------------
# `frankweiler/ui/` uses pnpm (`pnpm-lock.yaml` is checked in, and
# `package.json` pins a specific version via the `packageManager`
# field). Nothing about the UI _requires_ pnpm — no workspaces, no
# pnpm-specific lockfile features — but pnpm is what the lockfile is
# keyed against, so swapping to npm/yarn would mean regenerating it.
#
# `corepack` ships with Node 16.9+ and reads `package.json`'s
# `packageManager` field to provision the exact pinned pnpm version
# on demand. That removes the previous "you must `brew install pnpm`
# separately" footgun — every dev with Node already has corepack.
#
# Usage:
#   source scripts/ensure_pnpm.sh
# After sourcing, `pnpm` is on PATH (or the script exits with a clear
# error pointing at the right install path).

set -eo pipefail

if command -v pnpm >/dev/null 2>&1; then
    return 0 2>/dev/null || exit 0
fi

if ! command -v corepack >/dev/null 2>&1; then
    cat >&2 <<'EOF'
ERROR: neither `pnpm` nor `corepack` on PATH.

The frankweiler UI uses pnpm (pinned via `packageManager` in
frankweiler/ui/package.json). On a recent Node install (16.9+),
`corepack` is bundled — make sure your Node bin dir is on PATH.

If you really don't want corepack: `npm install -g pnpm`.
EOF
    exit 1
fi

echo "  bootstrapping pnpm via corepack..." >&2
# `corepack enable pnpm` writes a pnpm shim into the dir containing
# `node` (typically ~/.nvm/versions/node/<v>/bin/ or /opt/homebrew/bin).
# The shim itself defers to whatever version `packageManager` pins, so
# the first actual `pnpm <cmd>` invocation may print a one-time download
# notice. After that it's cached in `~/.cache/corepack`.
corepack enable pnpm >/dev/null

if ! command -v pnpm >/dev/null 2>&1; then
    echo "ERROR: corepack ran but pnpm still not on PATH." >&2
    echo "       Try: corepack prepare pnpm@latest --activate" >&2
    exit 1
fi
