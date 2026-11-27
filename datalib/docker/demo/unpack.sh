#!/bin/sh
# Unpack the demo library staged by datalib/docker/stage_demo.sh, in the
# folder it was staged to: its `library.tar` (the raw stores and the
# inputs) lands beside the config, and the archive goes. Run by the
# Dockerfile, inside the image build, and by
# //datalib/backend/dag:copy_restore_test.
#
# Usage: unpack.sh <library-dir>
set -eu
cd "${1:?usage: unpack.sh <library-dir>}"
tar -xf library.tar --strip-components=1 --no-same-owner
rm library.tar unpack.sh
