"""Bazel py_test entrypoint for the qmd_bridge integration test."""

import sys
from pathlib import Path

import pytest

if __name__ == "__main__":
    here = Path(__file__).parent
    sys.exit(pytest.main([str(here / "test_qmd_bridge_integration.py"), "-v", "-rs"]))
