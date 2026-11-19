#!/usr/bin/env bash
# Stage what CUDA adds to a release, x86_64 gnu Linux only
# (docs/dev/plans/embed_on_modal.md), into the current directory, each
# with a `.sha256` sidecar:
#
#   ggml-cuda-x86_64-unknown-linux-gnu.tar.gz   the release asset:
#       lib/libggml-cuda.so, which datalib-step loads from `lib/` beside
#       itself, our notice, llama.cpp's license and NVIDIA's license for
#       the CUDA runtime linked in; unpacks straight into an install
#   cuda-libs-x86_64-unknown-linux-gnu.tar.gz   the `-cuda` image's
#       cuBLAS, named by SONAME, our notice and NVIDIA's license for
#       cuBLAS; not published
#
#   scripts/release/stage_cuda.sh <libggml-cuda.so> <datalib-step> \
#       <llama.cpp LICENSE> <cudart LICENSE> <cuBLAS LICENSE> <cuBLAS file>...
#
# NVIDIA's two LICENSE files are what `@llama_cpp//:nvidia_licenses`
# builds, and the cuBLAS files what `@llama_cpp//:cublas_libs` builds:
# every `libcublas*.so*` the module links. Before staging, it refuses a module
# that needs a ggml symbol `datalib-step` does not export (it would not
# load into it), or a library other than NVIDIA's driver (libcuda.so.1)
# that the staged cuBLAS does not provide. Run by `.github/workflows/cuda.yml`
# on a change that could move any of it, and by the release's `cuda` job.
set -euo pipefail

if [[ $# -lt 6 ]]; then
    echo "usage: $0 <libggml-cuda.so> <datalib-step> <llama.cpp LICENSE> <cudart LICENSE> <cuBLAS LICENSE> <cuBLAS file>..." >&2
    exit 2
fi
module="$1"
step="$2"
llama_license="$3"
cudart_license="$4"
cublas_license="$5"
shift 5

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../.." && pwd -P)"
notice="$repo_root/third-party/llama_cpp/CUDA-NOTICE.md"
triple=x86_64-unknown-linux-gnu

log() { printf '>>> stage_cuda: %s\n' "$*" >&2; }
fail() { printf 'stage_cuda: error: %s\n' "$*" >&2; exit 1; }

work="$(mktemp -d "${TMPDIR:-/tmp}/stage_cuda.XXXXXX")"
trap 'rm -rf "$work"' EXIT

# The module's undefined symbols that no versioned system library, cuBLAS
# or the driver (cu*) provides must be ones datalib-step exports.
nm -D --undefined-only "$module" | awk '$1 == "U" {print $2}' | sort -u > "$work/undefined"
grep -v '@' "$work/undefined" | grep -vE '^(cublas|cu[A-Z])' > "$work/from_step" || true
nm -D --defined-only "$step" | awk '{print $3}' | sort -u > "$work/exported"
comm -23 "$work/from_step" "$work/exported" > "$work/missing"
log "$(wc -l < "$work/undefined") undefined; $(wc -l < "$work/from_step") wanted from datalib-step"
if [[ -s "$work/missing" ]]; then
    cat "$work/missing" >&2
    fail "libggml-cuda.so needs symbols datalib-step does not export"
fi

# cuBLAS under its SONAMEs, the names the module's NEEDED entries ask for;
# the unversioned and fully versioned names are the same bytes again.
# NVIDIA's text, which the notice names; refuse anything else in its place.
for f in "$cudart_license" "$cublas_license"; do
    head -n 1 "$f" | grep -qx 'End User License Agreement' \
        || fail "$f is not NVIDIA's CUDA license"
done

mkdir -p "$work/libs/cublas" "$work/libs/licenses/nvidia/cublas"
for f in "$@"; do
    soname="$(readelf -d "$f" | sed -n 's/.*(SONAME).*\[\(.*\)\]/\1/p')"
    if [[ -n "$soname" && "$(basename "$f")" == "$soname" ]]; then
        cp -L "$f" "$work/libs/cublas/$soname"
    fi
done
for want in libcublas.so.12 libcublasLt.so.12; do
    [[ -f "$work/libs/cublas/$want" ]] || fail "no $want among the cuBLAS files given"
done
cp "$notice" "$work/libs/licenses/CUDA-NOTICE.md"
cp "$cublas_license" "$work/libs/licenses/nvidia/cublas/LICENSE"

# With that cuBLAS beside it, the only library the module may still miss
# is the driver, which the machine with the GPU provides.
missing="$(LD_LIBRARY_PATH="$work/libs/cublas" ldd "$module" | awk '/not found/ {print $1}' | sort -u)"
log "not found beside the staged cuBLAS: ${missing:-nothing}"
if [[ -n "$missing" && "$missing" != "libcuda.so.1" ]]; then
    fail "libggml-cuda.so needs more than cuBLAS and the driver: $missing"
fi

mkdir -p "$work/module/lib" "$work/module/licenses/llama.cpp" "$work/module/licenses/nvidia/cudart"
cp -L "$module" "$work/module/lib/libggml-cuda.so"
chmod 0755 "$work/module/lib/libggml-cuda.so"
cp "$notice" "$work/module/licenses/CUDA-NOTICE.md"
cp "$llama_license" "$work/module/licenses/llama.cpp/LICENSE"
cp "$cudart_license" "$work/module/licenses/nvidia/cudart/LICENSE"
chmod -R u+w,go-w "$work/module" "$work/libs"
find "$work/module/licenses" "$work/libs/licenses" -type f -exec chmod 0644 {} +

checksum() {
    if command -v sha256sum >/dev/null; then
        sha256sum "$1" > "$1.sha256"
    else
        shasum -a 256 "$1" > "$1.sha256"
    fi
}
for kind in module libs; do
    case "$kind" in
        module) out="ggml-cuda-$triple.tar.gz" ;;
        libs) out="cuda-libs-$triple.tar.gz" ;;
    esac
    # Fixed owners and times, so the same bytes stage to the same archive.
    tar --sort=name --owner=0 --group=0 --numeric-owner --mtime=@0 \
        -C "$work/$kind" -cf - . | gzip -n > "$out"
    checksum "$out"
    ls -la "$out" >&2
done
