"""Smoke tests that run without external dependencies (no dolt, no qmd, no real export)."""

from __future__ import annotations

import json
from pathlib import Path

from ingest.config import (
    AnthropicExportDirSource,
    ChatgptWebSync,
    ClaudeWebSync,
    Config,
    DoltConfig,
    GithubWebSync,
    GitlabWebSync,
    NotionWebSync,
    SlackWebSync,
    load_config,
)
from ingest.grid_rows import _anthropic_rows
from ingest.providers.anthropic.parse import parse_export
from ingest.render import _slugify
from ingest.run_source import resolve, sync_to_argv


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


def test_config_managed_source_with_sync_block(tmp_path: Path) -> None:
    cfg_path = tmp_path / "config.yaml"
    root = tmp_path / "data"
    root.mkdir()
    cfg_path.write_text(
        f"""
root: {root}
sources:
  - name: slack-work
    provider: slack
    kind: slack_api_dir
    path: {tmp_path}/slack
    managed: true
    sync:
      kind: slack_web
      channels: ["general", "random"]
      refresh_window_days: 7
"""
    )
    cfg = load_config(cfg_path)
    src = cfg.enabled_sources[0]
    assert src.managed is True
    assert src.sync is not None
    assert src.sync.kind == "slack_web"
    assert src.sync.channels == ["general", "random"]
    assert src.sync.refresh_window_days == 7


def test_config_managed_without_sync_block_rejected(tmp_path: Path) -> None:
    cfg_path = tmp_path / "config.yaml"
    root = tmp_path / "data"
    root.mkdir()
    cfg_path.write_text(
        f"""
root: {root}
sources:
  - name: bad
    provider: slack
    kind: slack_api_dir
    path: {tmp_path}/slack
    managed: true
"""
    )
    try:
        load_config(cfg_path)
    except Exception as e:
        assert "sync" in str(e).lower()
        return
    raise AssertionError("expected managed-without-sync validation to fail")


def test_sync_to_argv_per_provider(tmp_path: Path) -> None:
    out = tmp_path / "out"
    out.mkdir()
    assert sync_to_argv(
        SlackWebSync(
            kind="slack_web",
            channels=["general", "random"],
            since="2026-01-01",
            refresh_window_days=7,
            all_channels=True,
            media=False,
        ),
        out,
    ) == [
        "--out-dir",
        str(out),
        "--channels",
        "general",
        "--channels",
        "random",
        "--since",
        "2026-01-01",
        "--refresh-window-days",
        "7",
        "--all",
        "--no-media",
    ]
    assert sync_to_argv(ClaudeWebSync(kind="claude_web", overlap=5), out) == [
        "--out-dir",
        str(out),
        "--overlap",
        "5",
    ]
    assert sync_to_argv(
        ChatgptWebSync(kind="chatgpt_web", max_pages=10, sleep_between=0.5), out
    ) == [
        "--out-dir",
        str(out),
        "--max-pages",
        "10",
        "--sleep-between",
        "0.5",
    ]
    assert sync_to_argv(GithubWebSync(kind="github_web", max_prs=3), out) == [
        "--out-dir",
        str(out),
        "--max-prs",
        "3",
    ]
    assert sync_to_argv(GitlabWebSync(kind="gitlab_web", max_mrs=4), out) == [
        "--out-dir",
        str(out),
        "--max-mrs",
        "4",
    ]
    assert sync_to_argv(
        NotionWebSync(kind="notion_web", subtree="abc", space="def"), out
    ) == ["--out-dir", str(out), "--subtree", "abc", "--space", "def"]


def test_resolve_returns_sync_and_raw_outdir(tmp_path: Path) -> None:
    cfg_path = tmp_path / "config.yaml"
    root = tmp_path / "data"
    root.mkdir()
    cfg_path.write_text(
        f"""
root: {root}
sources:
  - name: slack-imbue
    provider: slack
    kind: slack_api_dir
    path: {tmp_path}/slack
    managed: true
    sync:
      kind: slack_web
      channels: ["general"]
"""
    )
    sync, out_dir = resolve("slack-imbue", cfg_path)
    assert isinstance(sync, SlackWebSync)
    assert sync.channels == ["general"]
    assert out_dir == root / "raw" / "slack-imbue"
    assert out_dir.exists()


def test_resolve_missing_sync_block_rejected(tmp_path: Path) -> None:
    cfg_path = tmp_path / "config.yaml"
    root = tmp_path / "data"
    root.mkdir()
    cfg_path.write_text(
        f"""
root: {root}
sources:
  - name: manual
    provider: anthropic
    kind: export_dir
    path: {tmp_path}/export
"""
    )
    try:
        resolve("manual", cfg_path)
    except ValueError as e:
        assert "sync" in str(e).lower()
        return
    raise AssertionError("expected resolve() to reject sync-less source")


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


def test_anthropic_llm_response_row_uses_final_text_block_not_message_text(
    tmp_path: Path,
) -> None:
    """The real claude.ai API populates the message-level ``text`` field with the
    first text-or-thinking-shaped block, which is often the ``thinking`` content
    rather than the assistant's actual final response. The ``LLM Response`` grid
    row must reflect the user-visible text — i.e. the concatenation of the
    ``text``-type blocks — not whatever ended up in ``message.text``."""
    export = tmp_path / "export"
    export.mkdir()
    (export / "users.json").write_text(
        json.dumps([{"uuid": "u-1", "full_name": "U", "email_address": "u@x"}])
    )
    thinking = "internal reasoning that should NOT surface as the response"
    final = "the actual user-visible answer"
    (export / "conversations.json").write_text(
        json.dumps(
            [
                {
                    "uuid": "c-1",
                    "name": "T",
                    "summary": "",
                    "created_at": "2026-01-01T00:00:00Z",
                    "updated_at": "2026-01-01T00:00:01Z",
                    "account": {"uuid": "u-1"},
                    "chat_messages": [
                        {
                            "uuid": "m-asst",
                            # Mirrors real claude.ai API: ``text`` carries the
                            # thinking content, not the final response.
                            "text": thinking,
                            "sender": "assistant",
                            "created_at": "2026-01-01T00:00:00Z",
                            "updated_at": "2026-01-01T00:00:00Z",
                            "parent_message_uuid": None,
                            "content": [
                                {"type": "thinking", "thinking": thinking},
                                {"type": "text", "text": final},
                            ],
                            "attachments": [],
                            "files": [],
                        }
                    ],
                }
            ]
        )
    )
    parsed = parse_export(export)
    rows = list(_anthropic_rows(parsed))
    response_rows = [r for r in rows if r.kind == "LLM Response"]
    assert len(response_rows) == 1
    assert response_rows[0].text == final, (
        f"LLM Response row should carry final text block, got: {response_rows[0].text!r}"
    )
