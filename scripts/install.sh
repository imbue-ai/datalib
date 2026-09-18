#!/bin/sh
# datalib installer — modeled on https://astral.sh/uv/install.sh
#
#   curl -LsSf https://raw.githubusercontent.com/imbue-ai/datalib/main/scripts/install.sh | sh
#
# Downloads the latest release tarball from
#   https://github.com/imbue-ai/datalib/releases
# unpacks it whole into ${DATALIB_LIB_DIR:-$HOME/.local/lib/datalib} and
# links each binary into ${DATALIB_INSTALL_DIR:-$HOME/.local/bin}.
#
# Whole, because the tarball is more than binaries: `runtime.manifest`
# beside them names the release's Node runtime (Node plus the `qmd` and
# `latchkey` package trees), which the binaries fetch, sha256-checked,
# into ~/.cache/datalib/runtime on their first use — so a sync runs
# both tools with no Node, npm or npx on the host, and this script
# installs nothing but the tarball. The symlinks are what puts the
# binaries on PATH; they canonicalize their own path before looking
# beside it, so the link is transparent to them.
#
# Env vars:
#   DATALIB_INSTALL_DIR   where the binaries are linked (default ~/.local/bin)
#   DATALIB_LIB_DIR       where the tarball is unpacked (default
#                             ~/.local/lib/datalib); replaced on every install
#   DATALIB_VERSION       release tag to install (default: latest)
#   DATALIB_LIBC          Linux only: 'gnu' or 'musl' (default:
#                             auto-detected; see libc detection below)
#
# Supported platforms (one published release tarball each):
#   macOS arm64          -> aarch64-apple-darwin
#   Linux x86_64 (glibc) -> x86_64-unknown-linux-gnu
#   Linux arm64 (glibc)  -> aarch64-unknown-linux-gnu
#   Linux x86_64 (musl)  -> x86_64-unknown-linux-musl   (fully static)
#   Linux arm64 (musl)   -> aarch64-unknown-linux-musl  (fully static)

set -eu

REPO="imbue-ai/datalib"
INSTALL_DIR="${DATALIB_INSTALL_DIR:-${HOME}/.local/bin}"
LIB_DIR="${DATALIB_LIB_DIR:-${HOME}/.local/lib/datalib}"
VERSION="${DATALIB_VERSION:-latest}"

say() { printf 'datalib-install: %s\n' "$1"; }
err() { printf 'datalib-install: error: %s\n' "$1" >&2; exit 1; }

# --- platform check ---
# Map uname's kernel/arch to the Rust target triple in the published
# tarball names. These triples are exactly what the release workflow
# builds (see .github/workflows/release.yml's matrix).
#
# Linux libc detection: musl-based distros (Alpine, postmarketOS, ...)
# can't run the gnu builds, so pick the fully-static musl tarball
# there. Every musl system ships its dynamic loader at
# /lib/ld-musl-<arch>.so.1 — its presence is the detection signal.
# glibc hosts default to the gnu build (the long-standing default);
# set DATALIB_LIBC=musl to force the static build anywhere — it
# runs on any Linux of the right arch regardless of libc.
os="$(uname -s)"
arch="$(uname -m)"
case "${os}/${arch}" in
    Darwin/arm64) TRIPLE="aarch64-apple-darwin" ;;
    Linux/x86_64 | Linux/aarch64 | Linux/arm64)
        case "${arch}" in
            x86_64) cpu="x86_64" ;;
            *)      cpu="aarch64" ;;
        esac
        libc="${DATALIB_LIBC:-}"
        if [ -z "${libc}" ]; then
            libc="gnu"
            for loader in /lib/ld-musl-*.so.1; do
                if [ -e "${loader}" ]; then
                    libc="musl"
                    break
                fi
            done
        fi
        case "${libc}" in
            gnu | musl) ;;
            *) err "invalid DATALIB_LIBC='${libc}' (expected 'gnu' or 'musl')" ;;
        esac
        TRIPLE="${cpu}-unknown-linux-${libc}"
        ;;
    *) err "unsupported platform ${os}/${arch}; supported: macOS arm64, Linux x86_64, Linux arm64" ;;
esac
TARBALL="datalib-${TRIPLE}.tar.gz"

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
tmpdir="$(mktemp -d 2>/dev/null || mktemp -d -t datalib-install)"
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

# Find the unpacked dir: `datalib-<version>-<triple>/`. Glob is fine
# because the tarball contains exactly one top-level dir.
staged=""
for d in "${tmpdir}"/datalib-*-"${TRIPLE}"; do
    [ -d "$d" ] && staged="$d" && break
done
[ -n "${staged}" ] || err "tarball did not contain expected datalib-*-${TRIPLE}/ dir"

# --- install ---
# The unpacked tree replaces LIB_DIR wholesale: it is ours (nothing else
# is documented to live there), and a stale `runtime.manifest` beside
# new binaries is exactly the drift a version bump must not leave behind.
# Swap through a sibling so an interrupted install leaves either the
# old tree or the new one, never a half of each.
mkdir -p "$(dirname "${LIB_DIR}")" "${INSTALL_DIR}"
rm -rf "${LIB_DIR}.new" "${LIB_DIR}.old"
mv "${staged}" "${LIB_DIR}.new"
[ ! -e "${LIB_DIR}" ] || mv "${LIB_DIR}" "${LIB_DIR}.old"
mv "${LIB_DIR}.new" "${LIB_DIR}"
rm -rf "${LIB_DIR}.old"

installed=""
for bin in "${LIB_DIR}"/*; do
    [ -f "${bin}" ] || continue
    name="$(basename "${bin}")"
    chmod +x "${bin}"
    ln -sfn "${bin}" "${INSTALL_DIR}/${name}"
    installed="${installed} ${name}"
done
[ -n "${installed}" ] || err "no binaries found in tarball"

say "unpacked into ${LIB_DIR}"
say "linked:${installed} -> ${INSTALL_DIR}"
if [ -f "${LIB_DIR}/runtime.manifest" ]; then
    say "the Node runtime for qmd and latchkey is fetched on first use (once per release,"
    say "sha256-checked, into ~/.cache/datalib/runtime); \`datalib-step pull-runtime\` does it now."
else
    say "note: this tarball names no Node runtime; semantic search and latchkey need"
    say "      DATALIB_RUNTIME_DIR pointed at one, or DATALIB_ALLOW_NPX=1 with Node on the host."
fi

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
