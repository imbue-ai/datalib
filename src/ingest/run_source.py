"""Project-aware shim that drives a project-agnostic downloader from a
`config.yaml` source entry.

The downloaders in `src/download/` are intentionally kept usable outside
this project — they each ship a plain typer CLI with no dependency on
`ingest.config`. This module is the bridge: given a `--source-name`, it
loads `config.yaml`, picks the matching source, translates that source's
`sync:` block into the right per-downloader CLI flags, and spawns the
downloader as a subprocess.

The worker calls this same translation, either by spawning this script or
by reusing `sync_to_argv` in-process to build the child argv. Either way,
the downloader scripts stay portable.
"""

from __future__ import annotations

import subprocess
import sys
from datetime import datetime
from pathlib import Path

import typer

from ingest.config import (
    ChatgptApiSource,
    ClaudeApiSource,
    ClaudeExportSource,
    GithubApiSource,
    GitlabApiSource,
    NotionApiSource,
    SlackApiSource,
    SourceConfig,
    load_config,
)

# Map each source `type:` to the downloader module that knows how to fetch
# it. `claude_export` is intentionally absent: there's no API equivalent
# for the bulk-download zip.
TYPE_TO_MODULE: dict[str, str] = {
    "claude_api": "download.claude_web",
    "chatgpt_api": "download.chatgpt_web",
    "slack_api": "download.slack_web",
    "github_api": "download.github_web",
    "gitlab_api": "download.gitlab_web",
    "notion_api": "download.notion_official",
}


def sync_to_argv(src: SourceConfig, out_dir: Path) -> list[str]:
    """Translate a source's `sync:` block into the argv its downloader
    expects. `out_dir` is where the downloader should write its output."""
    if src.sync is None:
        raise ValueError(
            f"source {src.name!r}: no `sync:` block — cannot drive a downloader"
        )

    if isinstance(src, SlackApiSource):
        sync = src.sync
        argv = ["--out-dir", str(out_dir)]
        for c in sync.channels or []:
            argv += ["--channels", c]
        if sync.since is not None:
            argv += ["--since", sync.since]
        if sync.refresh_window_days is not None:
            argv += ["--refresh-window-days", str(sync.refresh_window_days)]
        if sync.all_channels:
            argv += ["--all"]
        if not sync.media:
            argv += ["--no-media"]
        return argv

    if isinstance(src, ClaudeApiSource):
        sync = src.sync
        argv = ["--out-dir", str(out_dir)]
        if sync.overlap is not None:
            argv += ["--overlap", str(sync.overlap)]
        return argv

    if isinstance(src, ChatgptApiSource):
        sync = src.sync
        argv = ["--out-dir", str(out_dir)]
        if sync.max_pages is not None:
            argv += ["--max-pages", str(sync.max_pages)]
        if sync.limit is not None:
            argv += ["--limit", str(sync.limit)]
        if sync.sleep_between is not None:
            argv += ["--sleep-between", str(sync.sleep_between)]
        return argv

    if isinstance(src, GithubApiSource):
        sync = src.sync
        argv = ["--out-dir", str(out_dir)]
        if sync.refresh_window_days is not None:
            argv += ["--refresh-window-days", str(sync.refresh_window_days)]
        if sync.max_prs is not None:
            argv += ["--max-prs", str(sync.max_prs)]
        return argv

    if isinstance(src, GitlabApiSource):
        sync = src.sync
        argv = ["--out-dir", str(out_dir)]
        if sync.refresh_window_days is not None:
            argv += ["--refresh-window-days", str(sync.refresh_window_days)]
        if sync.max_mrs is not None:
            argv += ["--max-mrs", str(sync.max_mrs)]
        return argv

    if isinstance(src, NotionApiSource):
        sync = src.sync
        argv = ["--out-dir", str(out_dir)]
        if sync.inbox is not None and sync.inbox.enabled:
            argv += ["--inbox"]
            for t in sync.inbox.types or []:
                argv += ["--inbox-types", t]
            if sync.inbox.notification_page_size is not None:
                argv += [
                    "--notification-page-size",
                    str(sync.inbox.notification_page_size),
                ]
            if sync.inbox.max_notification_pages is not None:
                argv += [
                    "--max-notification-pages",
                    str(sync.inbox.max_notification_pages),
                ]
            if sync.inbox.space is not None:
                argv += ["--space", sync.inbox.space]
        if sync.subtrees is not None:
            for pid in sync.subtrees.pages:
                argv += ["--subtree-page", pid]
            if sync.subtrees.max_pages is not None:
                argv += ["--max-pages", str(sync.subtrees.max_pages)]
        return argv

    if isinstance(src, ClaudeExportSource):
        raise ValueError(
            f"source {src.name!r}: claude_export has no downloader "
            "(bulk export must be unpacked manually)"
        )

    raise ValueError(f"unknown source type: {src!r}")


def _run_timestamp() -> str:
    """Localized ISO-8601 with offset, filesystem-safe (`:` -> `-`)."""
    return datetime.now().astimezone().isoformat(timespec="seconds").replace(":", "-")


def resolve(
    source_name: str,
    config_path: Path | None = None,
    run_timestamp: str | None = None,
) -> tuple[SourceConfig, Path]:
    """Look up `source_name` in the config and return the source plus the
    dated raw output directory for this run
    (`<input_path>/<ISO-timestamp>/`). Each invocation gets its own subdir
    so consecutive runs don't trample each other; the worker records
    `download_runs.raw_path` pointing at it."""
    cfg = load_config(config_path)
    for src in cfg.sources:
        if src.name != source_name:
            continue
        if src.sync is None:
            raise ValueError(
                f"source {source_name!r}: no `sync:` block in config — "
                f"cannot drive a downloader for this source"
            )
        assert src.input_path is not None, "input_path defaulted at load time"
        ts = run_timestamp or _run_timestamp()
        out_dir = src.input_path / ts
        out_dir.mkdir(parents=True, exist_ok=True)
        return src, out_dir
    raise ValueError(f"source {source_name!r} not found in config")


def run(
    source_name: str = typer.Argument(..., help="`sources[].name` from config.yaml"),
    config: Path | None = typer.Option(
        None, "--config", help="Path to config.yaml (default: project default)."
    ),
    dry_run: bool = typer.Option(
        False, "--dry-run", help="Print the resolved argv and exit without spawning."
    ),
) -> None:
    """Resolve `source_name` in config.yaml and exec the matching downloader."""
    src, out_dir = resolve(source_name, config)
    module = TYPE_TO_MODULE[src.type]
    argv = sync_to_argv(src, out_dir)
    cmd = [sys.executable, "-m", module, *argv]
    if dry_run:
        typer.echo(" ".join(cmd))
        return
    raise SystemExit(subprocess.call(cmd))


if __name__ == "__main__":
    typer.run(run)
