#!/usr/bin/env bash
# Moved to the repo root as run-app.sh; kept so the old path still works.
exec "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/../../run-app.sh" "$@"
