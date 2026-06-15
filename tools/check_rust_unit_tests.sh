#!/bin/bash
# Guard: every `rust_library` must have a `rust_test(crate = ":<lib>")`
# unit-test target so its inline `#[cfg(test)]` tests actually run under
# `bazel test //...`.
#
# Why this exists: a `rust_test` with a `crate = ":<lib>"` attribute is
# what compiles the library in test mode and runs its inline unit tests.
# WITHOUT such a target, the inline tests compile only under `cargo test`
# and are SILENTLY SKIPPED by the supported `bazel test //...` path —
# i.e. they look like they pass while never running. We hit exactly this:
# `frankweiler_etl_fsindex` shipped with inline tests in stamp.rs/hash.rs
# that no CI run ever executed. This check makes that failure loud.
#
# Mechanism: `labels(crate, kind(rust_test, //...))` is the set of crates
# that some rust_test points its `crate` attribute at. Any rust_library
# not in that set has no unit-test target.
#
# Runs via Bazel query (needs the real workspace), so it lives in the
# pre-commit hook rather than as a sandboxed `bazel test` target — a
# hermetic test can't see the whole build graph. See README/PR notes.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
cd "$REPO_ROOT"

BAZEL="${BAZEL:-bazelisk}"

# Allowlist of rust_library labels that are intentionally exempt (e.g. a
# generated or trivially-thin crate with nothing to unit-test). Keep this
# EMPTY unless you have a real reason; the whole point is to force the
# unit-test target to exist so future inline tests run. One label per line.
ALLOWLIST=$(cat <<'EOF'
EOF
)

libs="$($BAZEL query "kind('rust_library rule', //...)" 2>/dev/null | sort -u)"
covered="$($BAZEL query "labels(crate, kind('rust_test rule', //...))" 2>/dev/null | sort -u)"

# rust_library targets with no rust_test(crate=...) pointing at them.
missing="$(comm -23 <(printf '%s\n' "$libs") <(printf '%s\n' "$covered"))"

# Drop allowlisted labels.
if [ -n "$ALLOWLIST" ]; then
    missing="$(comm -23 <(printf '%s\n' "$missing") <(printf '%s\n' "$ALLOWLIST" | sort -u))"
fi

if [ -n "$missing" ]; then
    {
        echo "ERROR: these rust_library targets have NO rust_test(crate = ...) target."
        echo "Their inline #[cfg(test)] unit tests will NOT run under 'bazel test //...'."
        echo "Add one next to the library (see //frankweiler/backend/etl/providers/contacts:contacts_unittests):"
        echo
        echo "    rust_test("
        echo "        name = \"<lib>_unittests\","
        echo "        crate = \":<lib>\","
        echo "        edition = \"2021\","
        echo "        deps = [ ... dev-only deps, e.g. @frankweiler_crates//:tempfile ... ],"
        echo "    )"
        echo
        echo "Missing unit-test targets:"
        printf '%s\n' "$missing" | sed 's/^/  - /'
    } >&2
    exit 1
fi

echo "[rust] unit-test coverage: every rust_library has a rust_test(crate=...) target"
