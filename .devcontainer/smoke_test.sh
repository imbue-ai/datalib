#!/usr/bin/env bash
# Run inside a freshly built devcontainer image, with the repo mounted at
# a path other than the one the image was built against (CI's is), and
# ideally under a different HOME: the baked output base must let bazel
# analyse `//...` without downloading anything. Fails the publish if it
# does not.
set -euo pipefail

profile=/tmp/analysis.profile.gz
bazelisk --batch --output_user_root=/opt/bazel/user-root --output_base=/opt/bazel/output-base \
    build --nobuild --lockfile_mode=error --profile="$profile" //...

python3 - "$profile" <<'PY'
import gzip
import json
import sys

profile = json.load(gzip.open(sys.argv[1]))
downloads = [
    e
    for e in profile["traceEvents"]
    if e.get("cat") == "Starlark builtin function call"
    and e["name"] in ("download", "download_and_extract")
]
seconds = sum(e["dur"] for e in downloads) / 1e6
print(f"downloads during analysis: {len(downloads)} calls, {seconds:.1f}s")
sys.exit(1 if seconds > 5 else 0)
PY
