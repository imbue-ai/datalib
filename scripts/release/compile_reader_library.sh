#!/usr/bin/env bash
# Compiles libdoltlite for a reader wheel. stage_reader_wheels.sh runs this
# inside a manylinux or musllinux image, whose libc is what the library ends up
# needing: <in> holds sqlite3.c and library_copts.txt, the flags every other
# build of the engine uses, one per line.
set -euo pipefail

in="$1"
out="$2"

mapfile -t copts < "$in/library_copts.txt"

# -Bsymbolic: the library's own calls stay inside it. musl has no RTLD_DEEPBIND,
# so without this a process with stock SQLite in its global symbols segfaults
# on the first open (check_reader_wheels.sh holds that).
# -static-libgcc: a minimal Alpine has no libgcc_s to load.
gcc -shared -fPIC -static-libgcc -Wl,-Bsymbolic "${copts[@]}" -w \
    "$in/sqlite3.c" -o "$out/libdoltlite.so" -lpthread -ldl -lm
