#!/usr/bin/env bash
# Refuse to ship a Linux file that needs a newer glibc than the oldest one
# the release promises to run on, GLIBC_FLOOR (docs/dev/release_steps.md
# §"The Linux floor"). Run by release.yml on every Linux asset it builds,
# the binaries' tarball and the Node runtime alike, before anything is
# published.
#
#   scripts/release/check_glibc_floor.sh <dir-or-.tar.gz>...
#
# A build links against its runner's glibc: v0.52.0's x86_64 tarball, built
# on Ubuntu 26.04, asked for GLIBC_2.43 and would not start on 24.04. Every
# ELF file under each argument is read, at any depth; anything else is
# skipped. The version names an ELF file needs are whole NUL-terminated
# strings in its dynamic string table, so they are read without binutils,
# which a mac lacks; a static binary names none. An argument with no ELF
# file in it fails, since then the check looked at nothing.
# //tools:check_glibc_floor_test runs it against stand-ins.
set -euo pipefail

GLIBC_FLOOR="2.39"

if [[ $# -lt 1 ]]; then
    echo "usage: $0 <dir-or-.tar.gz>..." >&2
    exit 2
fi

failed=0
complain() { printf 'check_glibc_floor: error: %s\n' "$*" >&2; failed=1; }

# 0 when glibc version $1 (`2.43`, `2.2.5`) is newer than GLIBC_FLOOR.
above_floor() {
    local major minor rest floor_major floor_minor
    major="${1%%.*}"; rest="${1#*.}"; minor="${rest%%.*}"
    floor_major="${GLIBC_FLOOR%%.*}"; floor_minor="${GLIBC_FLOOR#*.}"
    (( major > floor_major || (major == floor_major && minor > floor_minor) ))
}

unpacked="$(mktemp -d "${TMPDIR:-/tmp}/check_glibc_floor.XXXXXX")"
trap 'rm -rf "$unpacked"' EXIT

for arg in "$@"; do
    root="$arg"
    if [[ -f "$arg" ]]; then
        root="$unpacked/$(basename "$arg")"
        mkdir -p "$root"
        tar -xzf "$arg" -C "$root"
    elif [[ ! -d "$arg" ]]; then
        complain "$arg is neither a directory nor a file"
        continue
    fi
    elf_files=0
    while IFS= read -r file; do
        # A builtin read, since a runtime holds thousands of files; it stops
        # at a NUL, which the four bytes of ELF magic do not hold.
        magic=""
        LC_ALL=C IFS= read -r -d '' -n 4 magic < "$file" || true
        [[ "$magic" == $'\x7fELF' ]] || continue
        elf_files=$((elf_files + 1))
        name="$(basename "$arg"):${file#"$root"/}"
        needed="$(LC_ALL=C tr '\0' '\n' < "$file" \
            | LC_ALL=C grep -E '^GLIBC_[0-9]+\.[0-9]+(\.[0-9]+)?$' | sed 's/^GLIBC_//' || true)"
        too_new=""
        for version in $needed; do
            if above_floor "$version"; then
                too_new="$too_new GLIBC_$version"
            fi
        done
        if [[ -n "$too_new" ]]; then
            complain "$name needs$too_new, above the floor GLIBC_$GLIBC_FLOOR (\`objdump -T <file> | grep GLIBC_\` names the symbols)"
        fi
    done < <(find -L "$root" -type f | LC_ALL=C sort)
    if [[ $elf_files -eq 0 ]]; then
        complain "$arg holds no ELF file, so there was nothing to check"
    else
        echo "$arg: $elf_files ELF files read, against GLIBC_$GLIBC_FLOOR"
    fi
done

exit "$failed"
