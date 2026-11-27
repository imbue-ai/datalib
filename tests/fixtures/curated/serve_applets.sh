#!/usr/bin/env bash
# Serve the two applets that write `datalib_curated/` (the contacts app
# and the grid and search index, which writes the mutes) on a data root,
# as the gateway would, without datalib-http and so without its sync
# loop. For curating a root by hand through the applets' routes; see
# datalib/backend/curated_overlay/README.md.
#
#   serve_applets.sh <data_root> <state_dir>
#
# Each applet's port lands in <state_dir>/port.<applet id>; send the
# secret in <state_dir>/applet-secret as `X-Datalib-Applet-Secret`. Runs
# until interrupted. DATALIB_APPLET names the binary; the default is
# the one `bazelisk build //datalib/backend:bin` stages.
#
# The applets run in UTC, so a checked-in store's stamps say nothing of
# where it was curated.
set -euo pipefail

root="$(cd "${1:?usage: serve_applets.sh <data_root> <state_dir>}" && pwd)"
state="${2:?usage: serve_applets.sh <data_root> <state_dir>}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
applet="${DATALIB_APPLET:-$here/../../../bazel-bin/datalib/backend/bin/datalib-applet}"
[[ -x "$applet" ]] || { echo "no datalib-applet at $applet: bazelisk build //datalib/backend:bin" >&2; exit 1; }

mkdir -p "$state"
umask 077
head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$state/applet-secret"
cd "$root"
for id in datalib_contacts unified_grid_and_search_index; do
  TZ=UTC DATALIB_DAG_DATA_ROOT="$root" DATALIB_APPLET_ID="$id" DATALIB_APPLET_BASE="/applet/$id/" \
  DATALIB_APPLET_SECRET_FILE="$state/applet-secret" DATALIB_DAG_PARAMS='{}' \
    "$applet" "$id" --port 0 \
    > "$state/port.$id" 2> "$state/applet.$id.log" &
done
wait
