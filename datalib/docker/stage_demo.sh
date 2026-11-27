#!/usr/bin/env bash
# Stage the demo data library into a docker build context.
#
# The demo is the TNG library (tests/fixtures/README.md §"Start here"),
# the one `bazelisk run //datalib:dev_tng` opens: `:ingested_tng`'s
# `library.tar` (every source's raw stores and the inputs the
# file-reading sources read) and its config, finished here with where
# the inputs land in the image, `/opt/datalib/demo/inputs`, and the
# applets in demo/applets.toml. Picard's curated stores
# (tests/fixtures/curated/datalib_curated/) come too, so the contacts
# app opens with people already linked.
#
# The archive is copied, not unpacked: it is another job's artifact, and
# this runs in the job that pushes the image. The Dockerfile unpacks it
# with demo/unpack.sh, which it copies into the context too.
#
# Called by scripts/build_docker.sh, by release.yml's image jobs through
# scripts/release/stage_docker_context.sh, and by
# //datalib/backend/dag:copy_restore_test.
#
# Usage: datalib/docker/stage_demo.sh <build-context-dir> <ingested-dir>
#   <ingested-dir> holds `library.tar` and `config_body.toml`, as
#   `bazelisk build //tests/fixtures:ingested_tng` leaves them in
#   bazel-bin/tests/fixtures/ingested/.

set -euo pipefail

ctx="${1:?usage: stage_demo.sh <build-context-dir> <ingested-dir>}"
ingested="${2:?usage: stage_demo.sh <build-context-dir> <ingested-dir>}"
repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
dest="${ctx}/demo"

for f in library.tar config_body.toml; do
    [[ -f "${ingested}/${f}" ]] || { echo "stage_demo: no ${f} in ${ingested}" >&2; exit 1; }
done

rm -rf "${dest}"
mkdir -p "${dest}"
cp "${ingested}/library.tar" "${dest}/library.tar"
cp "${repo_root}/datalib/docker/demo/unpack.sh" "${dest}/unpack.sh"

inputs=/opt/datalib/demo/inputs
body="$(cat "${ingested}/config_body.toml")"
{
    cat <<'EOF'
# The demo data library baked into the docker image at /opt/datalib/demo:
# the TNG library, written by datalib/docker/stage_demo.sh from the
# fixture build's config. The sources that read files read them under
# inputs/; the rest keep what their raw stores hold
# (`common.download = false`). No `data_root`: it defaults to this
# file's directory.

EOF
    printf '%s\n' "${body//@INPUTS@/${inputs}}"
    echo
    cat "${repo_root}/datalib/docker/demo/applets.toml"
} > "${dest}/config.toml"

curated="${repo_root}/tests/fixtures/curated/datalib_curated"
for store in "${curated}"/*/*.doltlite_db; do
    [[ -f "${store}" ]] || { echo "stage_demo: no curated stores under ${curated}" >&2; exit 1; }
    rel="${store#"${curated}"/}"
    mkdir -p "${dest}/datalib_curated/$(dirname "${rel}")"
    cp "${store}" "${dest}/datalib_curated/${rel}"
done
echo "stage_demo: staged $(find "${dest}" -type f | wc -l | tr -d ' ') files into ${dest}"
