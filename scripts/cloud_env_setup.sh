#!/usr/bin/env bash
# The setup script for a Claude Code cloud environment (claude.ai/code,
# the environment's settings, "Setup script"). Paste this file's text
# there. The environment runs it as root on Ubuntu 24.04 x86_64 before
# Claude starts, snapshots the VM, and reuses the snapshot until the
# pasted text changes, so an edit here reaches no environment until
# someone pastes it again.
#
# The base image already has gcc/clang, Python with uv, Node, git and gh.
# This adds what `.devcontainer/Dockerfile` adds that the base image
# lacks: bazelisk, verified against the same sha256 pins, and the
# packages a build script or test shells out to. Bazel brings its own
# Rust, Node and Python. Playwright's browsers are left out, so
# `//datalib/ui:e2e_test` will not run here.
set -euo pipefail

apt-get update
# perl, pkg-config, libssl-dev: native crates' build scripts (ring, onig_sys).
# lld: rustc on Linux opts into it. gdb: datalib-step's exit watchdog.
# rsync: the e2e wrapper. sqlite3: the beeper extract runs the stock CLI.
apt-get install -y --no-install-recommends \
    gdb libssl-dev lld perl pkg-config rsync sqlite3 unzip zip

# Keep in step with `.devcontainer/Dockerfile`'s BAZELISK_* args.
BAZELISK_VERSION=1.30.0
BAZELISK_SHA256_AMD64=a248ae2b2b62b5baebb748f6fba7203bff1a27e5d01b55399a8406744cb539e8
BAZELISK_SHA256_ARM64=b2bcc040c213b44586b121bda6d983c35df121b917c3a7a66fb16f2fa0df1a4f
ARCH="$(dpkg --print-architecture)"
case "$ARCH" in
    amd64) SHA="$BAZELISK_SHA256_AMD64" ;;
    arm64) SHA="$BAZELISK_SHA256_ARM64" ;;
    *) echo "unknown arch: $ARCH" >&2; exit 1 ;;
esac
curl -fsSL -o /usr/local/bin/bazelisk \
    "https://github.com/bazelbuild/bazelisk/releases/download/v${BAZELISK_VERSION}/bazelisk-linux-${ARCH}"
echo "${SHA}  /usr/local/bin/bazelisk" | sha256sum -c -
chmod +x /usr/local/bin/bazelisk
ln -sf /usr/local/bin/bazelisk /usr/local/bin/bazel

# Download the bazel that `.bazelversion` names into bazelisk's cache, so
# the snapshot carries it. Best effort: the checkout may not be here yet.
if [[ -f .bazelversion ]]; then
    bazelisk version || true
fi
