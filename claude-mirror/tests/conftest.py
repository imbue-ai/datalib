"""Bazel py_test entrypoint that delegates to pytest."""

import sys
from pathlib import Path

import pytest

if __name__ == "__main__":
    here = Path(__file__).parent
    sys.exit(pytest.main([
        str(here / "test_smoke.py"),
        str(here / "test_fixtures.py"),
        "-v",
    ]))
