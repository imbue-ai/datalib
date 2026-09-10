#!/usr/bin/env bash
# Stage the demo data library's inputs into a docker build context.
#
# Copies the TNG fixtures the demo config names into `<ctx>/demo/`, one
# directory per source, plus the config itself. The Dockerfile COPYs
# that directory to /opt/datalib/demo-sources and runs the pipeline over
# it. Called by scripts/build_docker.sh and by release.yml's
# docker-publish job, so the list of fixtures lives here and nowhere
# else.
#
# Usage: datalib/docker/stage_demo.sh <build-context-dir>

set -euo pipefail

ctx="${1:?usage: stage_demo.sh <build-context-dir>}"
repo_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)"
providers="${repo_root}/datalib/backend/etl/providers"
dest="${ctx}/demo"

rm -rf "${dest}"
mkdir -p "${dest}"

# <name in demo/config.toml>  <fixture path under the providers tree>
while read -r name src; do
    [[ -e "${providers}/${src}" ]] || { echo "stage_demo: missing fixture ${providers}/${src}" >&2; exit 1; }
    mkdir -p "${dest}/${name}"
    cp -R "${providers}/${src}" "${dest}/${name}/"
done <<'TABLE'
claude-export       claude/tests/fixtures/claude_export/.
gmail-takeout       email/tests/fixtures/mbox/star_trek.mbox
google-takeout      google_takeout/tests/fixtures/Takeout
sms-backup-restore  sms_backup_restore/tests/fixtures/sms_backup_restore_tng/.
linkedin            linkedin/tests/fixtures/linkedin_tng/.
contacts            contacts/tests/fixtures/carddav_tng/.
pdfs                pdf/tests/fixtures/pdf_tng/.
TABLE

cp "${repo_root}/datalib/docker/demo/config.toml" "${dest}/config.toml"
echo "stage_demo: staged $(find "${dest}" -type f | wc -l | tr -d ' ') files into ${dest}"
