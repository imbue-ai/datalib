#!/bin/sh
# frankweiler installer — modeled on https://astral.sh/uv/install.sh
#
#   curl -LsSf https://raw.githubusercontent.com/imbue-ai/datalib/main/scripts/install.sh | sh
#
# Downloads the latest release tarball from
#   https://github.com/imbue-ai/datalib/releases
# and drops the binaries into ${FRANKWEILER_INSTALL_DIR:-$HOME/.local/bin}.
#
# Env vars:
#   FRANKWEILER_INSTALL_DIR   target dir for the binaries (default ~/.local/bin)
#   FRANKWEILER_VERSION       release tag to install (default: latest)
#
# Supported platforms (one published release tarball each):
#   macOS arm64     -> aarch64-apple-darwin
#   Linux x86_64    -> x86_64-unknown-linux-gnu
#   Linux arm64     -> aarch64-unknown-linux-gnu

set -eu

REPO="imbue-ai/datalib"
INSTALL_DIR="${FRANKWEILER_INSTALL_DIR:-${HOME}/.local/bin}"
VERSION="${FRANKWEILER_VERSION:-latest}"

say() { printf 'frankweiler-install: %s\n' "$1"; }
err() { printf 'frankweiler-install: error: %s\n' "$1" >&2; exit 1; }

# --- platform check ---
# Map uname's kernel/arch to the Rust target triple in the published
# tarball names. These three triples are exactly what the release
# workflow builds (see .github/workflows/release.yml's matrix).
os="$(uname -s)"
arch="$(uname -m)"
case "${os}/${arch}" in
    Darwin/arm64)               TRIPLE="aarch64-apple-darwin" ;;
    Linux/x86_64)               TRIPLE="x86_64-unknown-linux-gnu" ;;
    Linux/aarch64 | Linux/arm64) TRIPLE="aarch64-unknown-linux-gnu" ;;
    *) err "unsupported platform ${os}/${arch}; supported: macOS arm64, Linux x86_64, Linux arm64" ;;
esac
TARBALL="frankweiler-${TRIPLE}.tar.gz"

# --- tool check ---
need() { command -v "$1" >/dev/null 2>&1 || err "required tool not found: $1"; }
need curl
need tar
need mkdir
need mv
need uname

# --- resolve download URL ---
if [ "${VERSION}" = "latest" ]; then
    url="https://github.com/${REPO}/releases/latest/download/${TARBALL}"
    sha_url="${url}.sha256"
else
    url="https://github.com/${REPO}/releases/download/${VERSION}/${TARBALL}"
    sha_url="${url}.sha256"
fi

# --- download to tmpdir ---
tmpdir="$(mktemp -d 2>/dev/null || mktemp -d -t frankweiler-install)"
trap 'rm -rf "${tmpdir}"' EXIT INT TERM

say "downloading ${url}"
if ! curl --proto '=https' --tlsv1.2 -fsSL --retry 3 --retry-delay 2 \
        -o "${tmpdir}/${TARBALL}" "${url}"; then
    err "download failed (${url})"
fi

# --- optional checksum verification ---
if curl --proto '=https' --tlsv1.2 -fsSL --retry 2 \
        -o "${tmpdir}/${TARBALL}.sha256" "${sha_url}" 2>/dev/null; then
    # The .sha256 file lists the bare filename, so cd into tmpdir for
    # the `-c` check. Linux ships `sha256sum`; macOS ships `shasum`.
    # Both consume the same "HASH  filename" format the release writes.
    if command -v sha256sum >/dev/null 2>&1; then
        say "verifying checksum"
        (cd "${tmpdir}" && sha256sum -c "${TARBALL}.sha256") \
            || err "checksum verification failed"
    elif command -v shasum >/dev/null 2>&1; then
        say "verifying checksum"
        (cd "${tmpdir}" && shasum -a 256 -c "${TARBALL}.sha256") \
            || err "checksum verification failed"
    else
        say "no sha256 tool found; skipping checksum verification"
    fi
else
    say "checksum file not published; skipping verification"
fi

# --- extract ---
say "extracting"
tar -xzf "${tmpdir}/${TARBALL}" -C "${tmpdir}"

# Find the unpacked dir: `frankweiler-<version>-<triple>/`. Glob is fine
# because the tarball contains exactly one top-level dir.
staged=""
for d in "${tmpdir}"/frankweiler-*-"${TRIPLE}"; do
    [ -d "$d" ] && staged="$d" && break
done
[ -n "${staged}" ] || err "tarball did not contain expected frankweiler-*-${TRIPLE}/ dir"

# --- install ---
mkdir -p "${INSTALL_DIR}"
installed=""
for bin in "${staged}"/*; do
    [ -f "${bin}" ] || continue
    name="$(basename "${bin}")"
    mv -f "${bin}" "${INSTALL_DIR}/${name}"
    chmod +x "${INSTALL_DIR}/${name}"
    installed="${installed} ${name}"
done
[ -n "${installed}" ] || err "no binaries found in tarball"

say "installed:${installed} -> ${INSTALL_DIR}"

# --- PATH hint ---
case ":${PATH}:" in
    *":${INSTALL_DIR}:"*)
        ;;
    *)
        say ""
        say "${INSTALL_DIR} is not on your PATH. Add it with one of:"
        say "  echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.zshrc"
        say "  echo 'export PATH=\"${INSTALL_DIR}:\$PATH\"' >> ~/.bashrc"
        ;;
esac
