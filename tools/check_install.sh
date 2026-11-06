#!/usr/bin/env bash
# Runs scripts/install.sh against a made-up tarball, with stand-ins for
# `curl` and `uname`. Guards what it puts on a user's PATH: every
# executable at the top of the tarball and nothing else, since the
# tarball's `git-hash` and `runtime.manifest` are data files that the
# installer once linked and marked executable like the rest. Guards too
# that a tarball it cannot check is not installed: it once carried on when
# the checksum file was missing or no tool could read it. And guards what it
# installs by default: the pinned release, checked against the pinned hash,
# whatever the release's own checksum file says. The pin is written by what the
# release procedure runs.

# --- begin runfiles.bash initialization v3 ---
# Copy-pasted from the Bazel Bash runfiles library v3.
set -uo pipefail; set +e
f=bazel_tools/tools/bash/runfiles/runfiles.bash
# shellcheck disable=SC1090
source "${RUNFILES_DIR:-/dev/null}/$f" 2>/dev/null || \
  source "$(grep -sm1 "^$f " "${RUNFILES_MANIFEST_FILE:-/dev/null}" | cut -f2- -d' ')" 2>/dev/null || \
  source "$0.runfiles/$f" 2>/dev/null || \
  source "$(grep -sm1 "^$f " "$0.runfiles_manifest" | cut -f2- -d' ')" 2>/dev/null || \
  { echo>&2 "ERROR: cannot find $f"; exit 1; }; f=; set -e
# --- end runfiles.bash initialization v3 ---

installer="$(rlocation _main/scripts/install.sh)"
[[ -f "$installer" ]] || { echo "ERROR: install.sh not in runfiles at $installer" >&2; exit 1; }
pin_release="$(rlocation _main/scripts/release/pin_release.py)"
[[ -f "$pin_release" ]] || { echo "ERROR: pin_release.py not in runfiles at $pin_release" >&2; exit 1; }

work="$(mktemp -d "${TEST_TMPDIR:-/tmp}/install.XXXXXX")"
lib="$work/lib"
bin="$work/bin"
stubs="$work/stubs"
mkdir -p "$stubs" "$work/home"

cat > "$stubs/uname" <<'STUB'
#!/bin/sh
case "$1" in
    -s) echo Linux ;;
    -m) echo x86_64 ;;
esac
STUB
cat > "$stubs/curl" <<'STUB'
#!/bin/sh
# Serves the made-up tarball and whichever checksum file the case names.
for arg in "$@"; do last="$arg"; done
echo "$last" >> "$FAKE_URL_LOG"
while [ "$#" -gt 1 ]; do
    if [ "$1" = "-o" ]; then
        dest="$2"
    fi
    shift
done
case "$1" in
    *.sha256)
        [ -f "$FAKE_SIDECAR" ] || exit 22
        cp "$FAKE_SIDECAR" "$dest"
        ;;
    *) cp "$FAKE_TARBALL" "$dest" ;;
esac
STUB
chmod +x "$stubs/uname" "$stubs/curl"

stage="$work/datalib-0.0.0-x86_64-unknown-linux-gnu"
mkdir -p "$stage/docs" "$stage/lib"
touch "$stage/datalib-dag" "$stage/latchkey" "$stage/git-hash" "$stage/runtime.manifest" "$stage/docs/agent_user.md" \
    "$stage/lib/libdoltlite.so"
chmod +x "$stage/datalib-dag" "$stage/latchkey"
tarball="$work/datalib.tar.gz"
tar -czf "$tarball" -C "$work" "$(basename "$stage")"

sidecar="$work/sidecar"
good_sum() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$tarball"
    else
        shasum -a 256 "$tarball"
    fi
}
# The line the release writes: "<hash>  <bare filename>".
echo "$(good_sum | cut -d' ' -f1)  datalib-x86_64-unknown-linux-gnu.tar.gz" > "$sidecar"

urls="$work/urls"
script="$installer"
version=v0.0.1  # a tag the script does not pin, so its checksum file decides
run_installer() { # PATH to run under, then the directories to install into
    : > "$urls"
    env PATH="$1" HOME="$work/home" FAKE_TARBALL="$tarball" FAKE_SIDECAR="$sidecar" FAKE_URL_LOG="$urls" \
        DATALIB_LIBC=gnu DATALIB_VERSION="$version" \
        DATALIB_LIB_DIR="$2" DATALIB_INSTALL_DIR="$3" /bin/sh "$script" > "$work/install.log" 2>&1
}

install() {
    run_installer "$stubs:$PATH" "$lib" "$bin" \
        || { echo "FAIL: install.sh failed:" >&2; cat "$work/install.log" >&2; exit 1; }
}

failed=0
complain() { echo "FAIL: $1" >&2; failed=1; }

expect_refused() { # what its message should say, the case, then optionally a PATH
    local want="$1" what="$2" path="${3:-$stubs:$PATH}" here="$work/refused"
    rm -rf "$here"
    if run_installer "$path" "$here/lib" "$here/bin"; then
        complain "$what: installed anyway"
    elif ! grep -q "$want" "$work/install.log"; then
        complain "$what: refused without saying '$want': $(cat "$work/install.log")"
    fi
    [[ ! -e "$here/lib" && ! -e "$here/bin" ]] || complain "$what: left an install behind"
}

good_sidecar="$(cat "$sidecar")"
rm "$sidecar"
expect_refused "could not fetch the checksum" "no checksum file on the release"
echo "$(printf '0%.0s' {1..64})  datalib-x86_64-unknown-linux-gnu.tar.gz" > "$sidecar"
expect_refused "checksum mismatch" "a checksum that does not match"
: > "$sidecar"
expect_refused "checksum mismatch" "an empty checksum file"
echo "$good_sidecar" > "$sidecar"
# Only the tools the installer needs before it checks, and no sha256 tool.
tools="$work/no_sha_tools"
mkdir -p "$tools"
for tool in cat cp mkdir mktemp mv rm tar gzip dirname basename; do
    ln -s "$(command -v "$tool")" "$tools/$tool"
done
expect_refused "need sha256sum or shasum" "no tool to check with" "$stubs:$tools"

install
for name in datalib-dag latchkey; do
    [[ -L "$bin/$name" && -x "$bin/$name" ]] || complain "$name is not linked as an executable"
done
for name in git-hash runtime.manifest docs lib libdoltlite.so; do
    [[ ! -e "$bin/$name" && ! -L "$bin/$name" ]] || complain "$name is on the PATH"
done
[[ -f "$lib/lib/libdoltlite.so" ]] || complain "lib/libdoltlite.so is not beside the binaries, where the reader looks"
for name in git-hash runtime.manifest; do
    [[ -f "$lib/$name" ]] || complain "$name is not beside the binaries, where they read it"
    [[ ! -x "$lib/$name" ]] || complain "$name was marked executable"
done

# An upgrade clears the data-file link an earlier install made, and leaves
# alone a link of that name which points somewhere else.
ln -s "$lib/git-hash" "$bin/git-hash"
ln -s "$work/someone-elses-file" "$bin/runtime.manifest"
install
[[ ! -L "$bin/git-hash" ]] || complain "the link an earlier install made to git-hash survived the upgrade"
[[ -L "$bin/runtime.manifest" ]] || complain "the upgrade removed a link it did not make"

# A wrapper may install into a folder of its own and link the binaries onto the
# PATH after this script ends, so the advice to add that folder is conditional.
install
grep -q "If nothing else links these binaries" "$work/install.log" \
    || complain "a folder off the PATH got no conditional advice: $(cat "$work/install.log")"
run_installer "$stubs:$bin:$PATH" "$lib" "$bin" \
    || complain "the install with its folder on the PATH failed: $(cat "$work/install.log")"
! grep -q "is not on your PATH" "$work/install.log" \
    || complain "a folder already on the PATH was called missing from it"

# --- the pinned release: installed by default, and checked against the pin ---
pin_to() { # the hash this copy of the script pins for the fake tarball's triple
    TRIPLES_HASH="$1" PYTHONPATH="$(dirname "$pin_release")" python3 -c '
import os, sys
from pin_release import pin_install
from publish_release import TARBALL_TRIPLES
hashes = {t: "0" * 64 for t in TARBALL_TRIPLES}
hashes["x86_64-unknown-linux-gnu"] = os.environ["TRIPLES_HASH"]
sys.stdout.write(pin_install(open(sys.argv[1]).read(), "v7.7.7", hashes))
' "$installer" > "$work/pinned_install.sh"
    script="$work/pinned_install.sh"
}
tarball_hash="$(good_sum | cut -d' ' -f1)"
wrong_hash="$(printf '1%.0s' {1..64})"
want_url="https://github.com/imbue-ai/datalib/releases/download/v7.7.7/datalib-x86_64-unknown-linux-gnu.tar.gz"

for default in "" latest v7.7.7; do
    version="$default"
    pin_to "$tarball_hash"
    # The release's own checksum file is wrong, and must not be asked.
    echo "$wrong_hash  datalib-x86_64-unknown-linux-gnu.tar.gz" > "$sidecar"
    rm -rf "$work/pinned"
    run_installer "$stubs:$PATH" "$work/pinned/lib" "$work/pinned/bin" \
        || complain "DATALIB_VERSION='$default': the pinned release did not install: $(cat "$work/install.log")"
    [[ "$(cat "$urls")" == "$want_url" ]] \
        || complain "DATALIB_VERSION='$default': fetched something other than the pinned tarball: $(cat "$urls")"
done

# The swap the pin exists to stop: a release whose tarball and checksum file
# agree with each other, and not with the pin.
version=""
pin_to "$wrong_hash"
echo "$good_sidecar" > "$sidecar"
expect_refused "checksum mismatch" "a tarball the pin does not name, with a checksum file that agrees with it"

exit "$failed"
