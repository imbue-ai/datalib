#!/usr/bin/env bash
# Build the CUDA module, the cuBLAS it links and the `datalib-step` it
# loads into, then stage them with stage_cuda.sh into the current
# directory. The bazel flags are the caller's: release.yml's `cuda` job
# builds as the gnu legs do, `.github/workflows/cuda.yml` without a stamp.
#
#   scripts/release/build_cuda.sh <bazel flag>...
#
# x86_64 gnu Linux only, with NVIDIA's toolkit fetched by Bazel (~1 GB).
set -euo pipefail

script_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd -- "$script_dir/../.." && pwd -P)"
bazel="${BAZEL:-bazelisk}"
flags=(--config=cuda "$@")
targets=(
    @llama_cpp//:ggml_cuda_so
    @llama_cpp//:cublas_libs
    @llama_cpp//:nvidia_licenses
    //datalib/backend/datalib_step:datalib_step
)

(cd "$repo_root" && "$bazel" build "${flags[@]}" "${targets[@]}" >&2)
# Absolute paths, one per line; a Rust target also lists its rustfmt and
# clippy checks.
files() {
    (cd "$repo_root" && "$bazel" cquery "${flags[@]}" --output=files "$1" 2>/dev/null) \
        | grep -vE '\.(rustfmt|clippy)\.ok$' \
        | sed "s|^|$repo_root/|"
}
module="$(files @llama_cpp//:ggml_cuda_so)"
step="$(files //datalib/backend/datalib_step:datalib_step)"
cublas=()
while IFS= read -r f; do cublas+=("$f"); done < <(files @llama_cpp//:cublas_libs)
nvidia_licenses="$(files @llama_cpp//:nvidia_licenses)"
cudart_license="$(grep '/nvidia/cudart/LICENSE$' <<< "$nvidia_licenses")"
cublas_license="$(grep '/nvidia/cublas/LICENSE$' <<< "$nvidia_licenses")"
license="$(cd "$repo_root" && "$bazel" info "${flags[@]}" output_base 2>/dev/null)/external/+http_archive+llama_cpp/LICENSE"

"$BASH" "$script_dir/stage_cuda.sh" "$module" "$step" "$license" \
    "$cudart_license" "$cublas_license" "${cublas[@]}"
