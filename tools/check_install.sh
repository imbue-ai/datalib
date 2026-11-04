#!/usr/bin/env bash
# Runs scripts/install.sh against a made-up tarball, with stand-ins for
# `curl` and `uname`. Guards what it puts on a user's PATH: every
# executable at the top of the tarball and nothing else, since the
# tarball's `git-hash` and `runtime.manifest` are data files that the
# installer once linked and marked executable like the rest.

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
# Serves the made-up tarball, and has no checksum file to offer.
while [ "$#" -gt 1 ]; do
    if [ "$1" = "-o" ]; then
        dest="$2"
    fi
    shift
done
case "$1" in
    *.sha256) exit 22 ;;
esac
cp "$FAKE_TARBALL" "$dest"
STUB
chmod +x "$stubs/uname" "$stubs/curl"

stage="$work/datalib-0.0.0-x86_64-unknown-linux-gnu"
mkdir -p "$stage/docs"
touch "$stage/datalib-dag" "$stage/latchkey" "$stage/git-hash" "$stage/runtime.manifest" "$stage/docs/agent_user.md"
chmod +x "$stage/datalib-dag" "$stage/latchkey"
tarball="$work/datalib.tar.gz"
tar -czf "$tarball" -C "$work" "$(basename "$stage")"

install() {
    env PATH="$stubs:$PATH" HOME="$work/home" FAKE_TARBALL="$tarball" DATALIB_LIBC=gnu \
        DATALIB_LIB_DIR="$lib" DATALIB_INSTALL_DIR="$bin" sh "$installer" > "$work/install.log" 2>&1 \
        || { echo "FAIL: install.sh failed:" >&2; cat "$work/install.log" >&2; exit 1; }
}

failed=0
complain() { echo "FAIL: $1" >&2; failed=1; }

install
for name in datalib-dag latchkey; do
    [[ -L "$bin/$name" && -x "$bin/$name" ]] || complain "$name is not linked as an executable"
done
for name in git-hash runtime.manifest docs; do
    [[ ! -e "$bin/$name" && ! -L "$bin/$name" ]] || complain "$name is on the PATH"
done
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

exit "$failed"
