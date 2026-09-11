#!/usr/bin/env bash
# Build upstream curl-impersonate for one triple and stage it as the
# tarball MODULE.bazel pins. Run by .github/workflows/curl-impersonate.yml
# — on the runner for the darwin and linux-gnu legs, inside an
# `alpine:3.21` container for the linux-musl legs — and runnable by hand
# the same way (docs/dev/curl_impersonate.md).
#
#   build.sh <triple> <upstream-checkout> <out-dir>
#
# Writes <out-dir>/curl-impersonate-<triple>.tar.gz containing the
# stripped binary, the license notices of everything linked into it, and
# a SOURCE file naming the upstream commit and the build.
set -euo pipefail

triple="$1"
upstream="$2"
out="$3"

# Mirrors upstream's own Linux builds: the binary reads the host's CA
# store the way the system curl does; SSL_CERT_FILE overrides it.
linux_ca_flags=(-DCURL_CA_PATH=/etc/ssl/certs -DCURL_CA_BUNDLE=/etc/ssl/certs/ca-certificates.crt)

# IDN is off everywhere: we never resolve an internationalized host, and
# libidn2 is the one LGPL component in upstream's Linux builds.
args=(-G Ninja -DCMAKE_BUILD_TYPE=Release -DUSE_LIBIDN2=OFF)
case "${triple}" in
    *-apple-darwin)
        args+=(-DCMAKE_OSX_DEPLOYMENT_TARGET=11.0)
        ;;
    *-linux-gnu)
        args+=("${linux_ca_flags[@]}")
        ;;
    *-linux-musl)
        args+=("${linux_ca_flags[@]}")
        args+=(-DCMAKE_EXE_LINKER_FLAGS="-static -static-libstdc++ -static-libgcc")
        # See cc-static-cxx for why the C compiler is a wrapper.
        args+=(-DCMAKE_C_COMPILER="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/cc-static-cxx")
        ;;
    *)
        echo "build.sh: unknown triple ${triple}" >&2
        exit 1
        ;;
esac

# Every upstream source tarball (curl, BoringSSL, nghttp2/3, ngtcp2,
# brotli, zstd, zlib) is fetched by upstream's CMakeLists.txt with a
# URL_HASH, so a moved tarball fails here.
cmake -S "${upstream}" -B "${upstream}/build" "${args[@]}"
cmake --build "${upstream}/build" --parallel

bin="${upstream}/build/deps/build/curl/src/curl-impersonate"
case "${triple}" in
    *-apple-darwin) strip -x "${bin}" ;;
    *) strip "${bin}" ;;
esac
file "${bin}"
"${bin}" --version

# The same check release.yml applies to every musl binary it ships: no
# dynamic loader, no shared-library deps.
if [[ "${triple}" == *-linux-musl ]] && file "${bin}" | grep -q 'dynamically linked'; then
    echo "build.sh: ${bin} is dynamically linked" >&2
    exit 1
fi

# The workflow passes the commit in; inside the musl container the
# checkout belongs to another uid and git would refuse to read it.
commit="${UPSTREAM_COMMIT:-$(git -C "${upstream}" rev-parse HEAD)}"
stage="${out}/curl-impersonate-${triple}"
rm -rf "${stage}"
mkdir -p "${stage}"
cp "${bin}" "${stage}/curl-impersonate"
# The notices the binary's licenses require to travel with it.
deps="${upstream}/build/deps/src"
cp "${upstream}/LICENSE" "${stage}/LICENSE-curl-impersonate"
cp "${deps}/curl/COPYING" "${stage}/LICENSE-curl"
cp "${deps}/boringssl/LICENSE" "${stage}/LICENSE-boringssl"
cp "${deps}/nghttp2/COPYING" "${stage}/LICENSE-nghttp2"
cp "${deps}/nghttp3/COPYING" "${stage}/LICENSE-nghttp3"
cp "${deps}/ngtcp2/COPYING" "${stage}/LICENSE-ngtcp2"
cp "${deps}/brotli/LICENSE" "${stage}/LICENSE-brotli"
cp "${deps}/zstd/LICENSE" "${stage}/LICENSE-zstd"
cp "${deps}/zlib/LICENSE" "${stage}/LICENSE-zlib"
{
    echo "lexiforest/curl-impersonate ${commit}"
    echo "built by ${BUILD_URL:-$(hostname) $(date -u +%Y-%m-%dT%H:%M:%SZ)}"
} > "${stage}/SOURCE"
tar -C "${out}" -czf "${stage}.tar.gz" "curl-impersonate-${triple}"
echo "build.sh: wrote ${stage}.tar.gz"
