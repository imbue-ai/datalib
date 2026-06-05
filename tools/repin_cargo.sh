#!/usr/bin/env bash
# Force a real Cargo.lock refresh against the workspace's Cargo.toml
# files. Run this after bumping `[workspace.package].version` (or any
# other manifest edit that should re-resolve) before committing.
#
# Why this script exists: `CARGO_BAZEL_REPIN=1 bazel test //...`
# *almost* works as the refresh command — except when every test
# target cache-hits. In that case Bazel never actually runs the
# `crate.from_cargo` repin extension and Cargo.lock stays exactly
# where it was, even though the env var was set. Then a later
# pre-commit hook runs `cargo` (clippy, fmt, anything) which
# regenerates Cargo.lock after the commit has already been sealed,
# leaving the working tree dirty post-commit (see f25a952 +
# 8e4a8b3 for a previous round of this exact paper cut).
#
# The fix is to invoke `cargo metadata` (or `cargo update --workspace`)
# directly against the workspace. Either always re-resolves; the
# metadata variant is faster because it doesn't touch the registry.
#
# Pairs with //frankweiler/backend:cargo_lock_versions_test which
# refuses the next `bazel test //...` if the lockfile drifts.

set -euo pipefail

repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}/frankweiler/backend"

echo "tools/repin_cargo.sh: refreshing frankweiler/backend/Cargo.lock"
cargo metadata --format-version=1 --offline >/dev/null 2>&1 \
    || cargo metadata --format-version=1 >/dev/null

# Verify the lockfile now agrees with Cargo.toml's [workspace.package]
# version. Re-running the test would be cleaner but adds a bazel
# dependency; this 5-line shell check is enough to catch a no-op repin.
canonical="$(grep -E '^version = "[^"]+"$' Cargo.toml | head -n1 | sed -E 's/^version = "([^"]+)"$/\1/')"
got="$(awk '/^name = "frankweiler-core"/ {f=1; next} f && /^version = "/{match($0,/"[^"]+"/); print substr($0,RSTART+1,RLENGTH-2); exit}' Cargo.lock)"
if [[ "${canonical}" != "${got}" ]]; then
    echo "tools/repin_cargo.sh: ERROR — Cargo.lock still pins frankweiler-core at ${got} after refresh, expected ${canonical}." >&2
    echo "If you just bumped [workspace.package].version, you may need to remove Cargo.lock and rerun this script." >&2
    exit 1
fi
echo "tools/repin_cargo.sh: OK — Cargo.lock is in sync with Cargo.toml @ ${canonical}"
