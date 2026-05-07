"""Bazel py_test entrypoint for the snapshot tests."""

import sys
from pathlib import Path

import pytest

if __name__ == "__main__":
    here = Path(__file__).parent
    sys.exit(pytest.main([str(here / "test_snapshots.py"), "-v"]))
