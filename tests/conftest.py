"""Bazel py_test entrypoint that delegates to pytest.

Also (when imported as a pytest conftest) puts the tests directory on
sys.path so sibling modules like ``snapshot_extensions`` resolve under
both Bazel and ``uv run pytest``.
"""

import sys
from pathlib import Path

import pytest

_HERE = str(Path(__file__).resolve().parent)
if _HERE not in sys.path:
    sys.path.insert(0, _HERE)

if __name__ == "__main__":
    here = Path(__file__).parent
    sys.exit(pytest.main([
        str(here / "test_smoke.py"),
        str(here / "test_fixtures.py"),
        "-v",
    ]))
