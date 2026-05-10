"""Smoke tests that run without external dependencies (no dolt, no qmd, no real export)."""

from __future__ import annotations

import json
from pathlib import Path

from ingest.config import (
    AnthropicExportDirSource,
    Config,
    DoltConfig,
    load_config,
)
from ingest.providers.anthropic.parse import parse_export
from ingest.render import _slugify


def test_slugify_basic() -> None:
    assert _slugify("Hello World") == "hello-world"
    assert _slugify("  Special!! Chars  ") == "special-chars"
    assert _slugify("") == "untitled"
    assert _slugify(None) == "untitled"


def test_slugify_truncates_long_names() -> None:
    long = "a" * 200
    assert len(_slugify(long)) <= 60


def test_config_round_trip(tmp_path: Path) -> None:
    cfg_path = tmp_path / "config.yaml"
    root = tmp_path / "data"
    root.mkdir()
    cfg_path.write_text(
        f"""
root: {root}
sources:
  - name: test
    provider: anthropic
    kind: export_dir
    path: {tmp_path}/export
    enabled: true
"""
    )
    cfg = load_config(cfg_path)
    assert isinstance(cfg, Config)
    assert isinstance(cfg.dolt, DoltConfig)
    assert len(cfg.enabled_sources) == 1
    assert isinstance(cfg.enabled_sources[0], AnthropicExportDirSource)
    assert cfg.enabled_sources[0].name == "test"


def test_config_rejects_duplicate_source_names(tmp_path: Path) -> None:
    cfg_path = tmp_path / "config.yaml"
    root = tmp_path / "data"
    root.mkdir()
    cfg_path.write_text(
        f"""
root: {root}
sources:
  - name: dupe
    provider: anthropic
    kind: export_dir
    path: {tmp_path}/a
  - name: dupe
    provider: anthropic
    kind: export_dir
    path: {tmp_path}/b
"""
    )
    try:
        load_config(cfg_path)
    except Exception as e:
        assert "duplicate" in str(e).lower()
        return
    raise AssertionError("expected duplicate-name validation to fail")


def test_parse_export_minimal(tmp_path: Path) -> None:
    """Build a tiny synthetic export and parse it."""
    export = tmp_path / "export"
    export.mkdir()
    (export / "users.json").write_text(
        json.dumps(
            [
                {
                    "uuid": "u-1",
                    "full_name": "Test User",
                    "email_address": "test@example.com",
                }
            ]
        )
    )
    (export / "conversations.json").write_text(
        json.dumps(
            [
                {
                    "uuid": "c-1",
                    "name": "Hello",
                    "summary": "",
                    "created_at": "2026-01-01T00:00:00Z",
                    "updated_at": "2026-01-01T00:00:01Z",
                    "account": {"uuid": "u-1"},
                    "chat_messages": [
                        {
                            "uuid": "m-1",
                            "text": "hi",
                            "sender": "human",
                            "created_at": "2026-01-01T00:00:00Z",
                            "updated_at": "2026-01-01T00:00:00Z",
                            "parent_message_uuid": None,
                            "content": [{"type": "text", "text": "hi"}],
                            "attachments": [],
                            "files": [],
                        }
                    ],
                }
            ]
        )
    )
    parsed = parse_export(export)
    assert len(parsed.accounts) == 1
    assert len(parsed.conversations) == 1
    assert len(parsed.messages) == 1
    assert len(parsed.content_blocks) == 1
    assert parsed.accounts[0].account_uuid == "u-1"
    assert parsed.messages[0].text == "hi"
