#!/usr/bin/env bash
# Package the Frankweiler Tauri shell into a self-contained macOS app
# bundle: target/release/bundle/macos/Frankweiler.app (tauri.conf.json's
# `bundle.targets` is `["app"]`; pass `--bundles app,dmg` for a .dmg too).
#
# `pnpm dlx @tauri-apps/cli@^2 build` does the heavy lifting; its
# `beforeBuildCommand` (run with frankweiler/ as the working directory)
# builds the Bazel doltlite archive and the Vite UI. On top of that this
# script stages the two Bazel-built helper binaries the app shells out
# to into Contents/MacOS/, mirroring the release tarball's side-by-side
# layout (.github/workflows/release.yml "Stage tarball"):
#
#   * frankweiler-sync — the sync worker resolves it as a sibling of the
#     running executable (worker::resolve_sync_bin); without it every
#     UI-triggered sync fails.
#   * frankweiler-latchkey-curl-shim — frankweiler-sync resolves it as
#     a sibling of *itself* (SIBLING_NAMES in etl/src/latchkey.rs).
#
# The helpers are built `-c opt` and stashed BEFORE `tauri build` runs:
# its hook rebuilds doltlite in the default config, which repoints the
# bazel-bin convenience symlink away from the opt output tree.
#
# Ad-hoc signed unless the usual APPLE_* / tauri signing env vars are
# set; the post-copy re-sign below keeps the bundle's seal valid. Extra
# args are forwarded to `tauri build`.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")"
repo_root="$(cd ../.. && pwd)"

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
bazelisk build -c opt \
    //frankweiler/backend/sync:frankweiler_sync_bin \
    //frankweiler/backend/etl:latchkey_curl_shim
cp "${repo_root}/bazel-bin/frankweiler/backend/sync/frankweiler_sync_bin" \
   "${stage}/frankweiler-sync"
cp "${repo_root}/bazel-bin/frankweiler/backend/etl/latchkey_curl_shim" \
   "${stage}/frankweiler-latchkey-curl-shim"
chmod +x "${stage}"/*

pnpm dlx @tauri-apps/cli@^2 build "$@"

app="target/release/bundle/macos/Frankweiler.app"
if [[ -d "$app" ]]; then
    cp "${stage}/frankweiler-sync" "${stage}/frankweiler-latchkey-curl-shim" \
       "$app/Contents/MacOS/"
    # Adding files under Contents/ invalidates the signature tauri just
    # produced; re-sign (ad-hoc `-` unless a real identity is wanted).
    codesign --force --deep --sign - "$app"
    echo
    echo "bundle: $(pwd)/$app"
fi
