#!/usr/bin/env python3
"""Hermetic ruff gate: `ruff check` + `ruff format --check` under Bazel.

Replaces the `uv run ruff ...` half of `scripts/run_checks.sh`. The
difference that matters is where ruff comes from: `uv run` resolves it
from PyPI into the host `.venv` at test time, while here it is
`@py_pip//ruff`, ingested from the hash-pinned `//:requirements.txt` by
`pip.parse` in MODULE.bazel. No network, no host `$HOME`, no `.venv` —
so the test runs in the sandbox and is remote-cacheable like any other.

Both ruff invocations always run, even if the first fails, so a single
test run reports every problem rather than making you fix lint errors
one round-trip at a time.

Locating the ruff executable is fiddlier than it should be. ruff's wheel
declares NO console-script entry point (`entry_points = {}` in the
generated whl BUILD), so `py_console_script_binary` can't wrap it, and
`python -m ruff` doesn't work either: `ruff._find_ruff` searches
`<pkg parent>/bin`, but rules_python extracts the package to
`<repo>/site-packages/ruff` while the executable lands at `<repo>/bin`,
one level higher than any of its five candidate paths.

So we anchor off `@py_pip//ruff:whl` instead — a single-file label
(hence unambiguous under `$(rootpath)`) that sits in the whl repo root,
the same directory `bin/` is extracted into. `:extracted_whl_files` in
`data` is what actually puts `bin/ruff` in the runfiles tree.

Args (positional):
    1:  path to the ruff `.whl` file; `bin/ruff` beside it is the binary
    2:  path to pyproject.toml, which carries `[tool.ruff]`
    3+: the Python files to check

All paths arrive as `$(rootpath ...)` expansions, i.e. relative to the
runfiles root, which is also this process's cwd.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

# `--no-cache`: ruff would otherwise want to write `.ruff_cache/`. The
# sandbox makes that either a wasted write or an outright failure, and
# Bazel's own action cache already covers the "don't redo this" job.
_INVOCATIONS: list[tuple[str, list[str]]] = [
    ("ruff check", ["check", "--no-cache"]),
    ("ruff format --check", ["format", "--check", "--no-cache"]),
]


def main() -> int:
    if len(sys.argv) < 4:
        sys.stderr.write(f"usage: {sys.argv[0]} RUFF_WHL PYPROJECT FILE...\n")
        return 2
    whl, pyproject = Path(sys.argv[1]), sys.argv[2]
    files = sys.argv[3:]

    ruff = whl.parent / "bin" / "ruff"
    if not ruff.is_file():
        sys.stderr.write(
            f"ruff binary not found at {ruff}\n"
            "Expected it beside the wheel, from @py_pip//ruff:extracted_whl_files.\n"
        )
        return 2

    failed: list[str] = []
    for label, args in _INVOCATIONS:
        print(f"[python] {label} ({len(files)} files)", flush=True)
        cmd = [str(ruff), *args, "--config", pyproject, *files]
        if subprocess.run(cmd).returncode != 0:
            failed.append(label)

    if failed:
        sys.stderr.write(
            "\nFAILED: " + ", ".join(failed) + "\n"
            "Fix in place with:\n"
            "    uv run ruff check --fix .\n"
            "    uv run ruff format .\n"
        )
        return 1

    print("[python] ruff OK", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
