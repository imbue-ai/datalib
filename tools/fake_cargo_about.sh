#!/usr/bin/env bash
# Stands in for cargo-about under //tools:stage_tarball_test, where the
# real one cannot run (no cargo, no crate sources in the sandbox). It
# keeps the one behaviour the test is about: the file named by `-o` is
# written where cargo-about would write it, from the directory it is
# run in, and a directory that is not there is an error — which is how
# v0.35.1's tarballs failed.
set -euo pipefail
out=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        -o) out="$2"; shift 2 ;;
        *) shift ;;
    esac
done
[[ -n "$out" ]] || { echo "fake cargo-about: no -o" >&2; exit 2; }
[[ -d "$(dirname "$out")" ]] || { echo "[ERROR] output file $out could not be written: No such file or directory" >&2; exit 1; }
echo "# rust-crates.md written by tools/fake_cargo_about.sh" > "$out"
