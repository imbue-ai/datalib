"""Bazel py_test entrypoint for the pure-mapping unit tests."""

import sys
from pathlib import Path

import pytest

if __name__ == "__main__":
    here = Path(__file__).parent
    sys.exit(pytest.main([str(here / "test_qmd_bridge_mapping.py"), "-v"]))
