#!/usr/bin/env bash
# Compiles libdoltlite for a reader wheel. stage_reader_wheels.sh runs this
# inside a manylinux or musllinux image, whose libc is what the library ends up
# needing: <in> holds sqlite3.c and library_copts.txt, the flags every other
# build of the engine uses, one per line, and blake3_kernels/, BLAKE3's SIMD
# kernels with kernels.txt, the list of them.
set -euo pipefail

in="$1"
out="$2"

mapfile -t copts < "$in/library_copts.txt"

# BLAKE3's SIMD kernels for this CPU, each with its own flags, beside the
# amalgamation that dispatches to them: `<cpu> <source> <flags>` per line.
objects=()
while read -r cpu source flags; do
    [[ "$cpu" == "$(uname -m)" ]] || continue
    # shellcheck disable=SC2086 # split on purpose: one flag per word
    gcc -c "${copts[@]}" $flags -w "$in/blake3_kernels/$source" -o "/tmp/${source%.c}.o"
    objects+=("/tmp/${source%.c}.o")
done < "$in/blake3_kernels/kernels.txt"
[[ ${#objects[@]} -gt 0 ]] || { echo "no BLAKE3 kernel for $(uname -m)" >&2; exit 1; }

# -Bsymbolic: the library's own calls stay inside it. musl has no RTLD_DEEPBIND,
# so without this a process with stock SQLite in its global symbols segfaults
# on the first open (check_reader_wheels.sh holds that).
# -static-libgcc: a minimal Alpine has no libgcc_s to load.
gcc -shared -fPIC -static-libgcc -Wl,-Bsymbolic "${copts[@]}" -w \
    "$in/sqlite3.c" "${objects[@]}" -o "$out/libdoltlite.so" -lpthread -ldl -lm
