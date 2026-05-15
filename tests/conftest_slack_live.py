"""Bazel py_test entrypoint for the live Slack integration test."""

import sys
from pathlib import Path

import pytest

if __name__ == "__main__":
    here = Path(__file__).parent
    sys.exit(pytest.main([str(here / "test_slack_live.py"), "-v", "-s", "-rs"]))
