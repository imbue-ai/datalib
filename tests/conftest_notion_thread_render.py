"""Bazel py_test entrypoint for test_notion_thread_render."""

import sys
from pathlib import Path

import pytest

if __name__ == "__main__":
    here = Path(__file__).parent
    sys.exit(pytest.main([str(here / "test_notion_thread_render.py"), "-v"]))
