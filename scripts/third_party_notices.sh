#!/usr/bin/env bash
#
# Assemble the third-party license notices a release has to carry, into
# one `licenses/` directory beside the binaries. MIT, BSD and Apache all
# require the copyright notice to travel with a binary distribution;
# this is how it travels. The release tarball gets it from
# .github/workflows/release.yml, the .app from datalib/tauri/stage-runtime.sh
# (under Contents/Resources/licenses/), and scripts/stage_runtime.sh
# calls it for a checkout-staged runtime.
#
#   scripts/third_party_notices.sh <dest>
#
# What lands in <dest>:
#
#   README.md               what each file covers
#   rust-crates.md          every crate linked into the datalib-* binaries,
#                           from `cargo about` over datalib/backend
#                           (datalib/backend/about.toml + about.hbs)
#   ui-bundle.md            every npm package in the embedded UI, written
#                           by the vite build (datalib/ui/tools/thirdPartyNotices.ts)
#   curl-impersonate/       the notices packed with the impersonating curl
#   doltlite/               DoltLite's Apache-2.0 notice and license text
#   node/LICENSE            the bundled Node runtime's notice
#
# The last four come out of Bazel (//third-party:bundled_licenses and
# //datalib/ui:dist). cargo-about is the one tool this needs on PATH
# beyond bazel: `brew install cargo-about`, or the pinned download in
# release.yml. It fetches crate sources itself, so it needs the network
# on a cold machine. The qmd and latchkey trees under runtime/ keep each
# package's own LICENSE file inside node_modules and are not repeated
# here.

set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <dest>" >&2
    exit 2
fi
dest="$1"

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$script_dir/.."

log() { printf '>>> third_party_notices: %s\n' "$*" >&2; }
fail() { printf 'third_party_notices: error: %s\n' "$*" >&2; exit 1; }

if command -v bazelisk >/dev/null 2>&1; then
    bazel=bazelisk
elif command -v bazel >/dev/null 2>&1; then
    bazel=bazel
else
    fail "neither bazelisk nor bazel found on PATH"
fi
command -v cargo-about >/dev/null 2>&1 || fail "cargo-about not found on PATH (brew install cargo-about)"

log "building //third-party:bundled_licenses //datalib/ui:dist"
(cd "$repo_root" && "$bazel" build //third-party:bundled_licenses //datalib/ui:dist >&2)
bin="$(cd "$repo_root" && "$bazel" info bazel-bin)"

rm -rf "$dest"
mkdir -p "$dest"

# Bazel outputs are read-only; the copies must not be, since codesign
# and `tar` on the staging tree both expect to own their files.
cp -R "$bin/third-party/bundled_licenses/." "$dest/"
cp "$bin/datalib/ui/dist/THIRD_PARTY_NOTICES.md" "$dest/ui-bundle.md"
chmod -R u+w "$dest"

log "cargo about generate (datalib/backend)"
(cd "$repo_root/datalib/backend" && cargo about generate --locked --workspace \
    --fail -c about.toml about.hbs -o "$dest/rust-crates.md" >&2)

cat > "$dest/README.md" <<'EOF'
# Third-party notices

datalib is MIT-licensed (LICENSE at the root of the repository). It
ships with third-party software under permissive licenses that ask for
their copyright notices to travel with binary distributions. This
directory is those notices.

| file | covers |
|---|---|
| `rust-crates.md` | every Rust crate linked into the `datalib-*` binaries |
| `ui-bundle.md` | every npm package bundled into the web UI that `datalib-http` serves |
| `curl-impersonate/` | `latchkey-curl-impersonate`: curl-impersonate, curl, BoringSSL, nghttp2, nghttp3, ngtcp2, brotli, zstd, zlib |
| `doltlite/` | DoltLite (Apache-2.0), the SQLite fork linked into every binary; SQLite itself is public domain |
| `node/LICENSE` | the bundled Node.js runtime under `runtime/node/` |

The `qmd` and `latchkey` package trees under `runtime/` carry each
package's own license file inside `node_modules/`.
EOF

for f in README.md rust-crates.md ui-bundle.md doltlite/LICENSE.md node/LICENSE curl-impersonate/LICENSE-curl; do
    [[ -s "$dest/$f" ]] || fail "missing or empty: $dest/$f"
done
log "notices assembled at $dest"
