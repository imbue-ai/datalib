#!/usr/bin/env bash
# Fails when datalib_step_lib depends, directly or not, on a provider
# crate. Code that names a provider belongs in datalib_step's src/, which
# a provider change is meant to recompile alone.
set -euo pipefail

deps="$1"
grep -qx '//datalib/backend/datalib_step/lib:datalib_step_lib' "$deps" || {
  echo "the dependency query names no datalib_step_lib; it queried something else" >&2
  exit 1
}
if grep '^//datalib/backend/etl/providers/' "$deps" >&2; then
  echo "datalib_step_lib depends on the provider targets above; move the code" \
    "that needs them to datalib/backend/datalib_step/src/" >&2
  exit 1
fi
