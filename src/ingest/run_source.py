"""Project-aware shim that drives a project-agnostic downloader from a
`config.yaml` source entry.

The downloaders in `src/download/` are intentionally kept usable outside
this project — they each ship a plain typer CLI with no dependency on
`ingest.config`. This module is the bridge: given a `--source-name`, it
loads `config.yaml`, picks the matching source, translates that source's
`sync:` block into the right per-downloader CLI flags, and spawns the
downloader as a subprocess.

The worker (Phase D) will call this same translation, either by spawning
this script or by reusing `sync_to_argv` in-process to build the child
argv. Either way, the downloader scripts stay portable.
"""

from __future__ import annotations

import subprocess
import sys
from datetime import datetime
from pathlib import Path

import typer

from ingest.config import (
    ChatgptWebSync,
    ClaudeWebSync,
    GithubWebSync,
    GitlabWebSync,
    NotionWebSync,
    SlackWebSync,
    SyncConfig,
    load_config,
)

KIND_TO_MODULE: dict[str, str] = {
    "claude_web": "download.claude_web",
    "chatgpt_web": "download.chatgpt_web",
    "slack_web": "download.slack_web",
    "github_web": "download.github_web",
    "gitlab_web": "download.gitlab_web",
    "notion_web": "download.notion_web",
}


def sync_to_argv(sync: SyncConfig, out_dir: Path) -> list[str]:
    """Translate a typed `sync:` block into the argv the matching downloader
    expects. `out_dir` is where the downloader should write its output."""
    if isinstance(sync, SlackWebSync):
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

    if isinstance(sync, ClaudeWebSync):
        argv = ["--out-dir", str(out_dir)]
        if sync.overlap is not None:
            argv += ["--overlap", str(sync.overlap)]
        return argv

    if isinstance(sync, ChatgptWebSync):
        argv = ["--out-dir", str(out_dir)]
        if sync.max_pages is not None:
            argv += ["--max-pages", str(sync.max_pages)]
        if sync.limit is not None:
            argv += ["--limit", str(sync.limit)]
        if sync.sleep_between is not None:
            argv += ["--sleep-between", str(sync.sleep_between)]
        return argv

    if isinstance(sync, GithubWebSync):
        argv = ["--out-dir", str(out_dir)]
        if sync.refresh_window_days is not None:
            argv += ["--refresh-window-days", str(sync.refresh_window_days)]
        if sync.max_prs is not None:
            argv += ["--max-prs", str(sync.max_prs)]
        return argv

    if isinstance(sync, GitlabWebSync):
        argv = ["--out-dir", str(out_dir)]
        if sync.refresh_window_days is not None:
            argv += ["--refresh-window-days", str(sync.refresh_window_days)]
        if sync.max_mrs is not None:
            argv += ["--max-mrs", str(sync.max_mrs)]
        return argv

    if isinstance(sync, NotionWebSync):
        argv = ["--out-dir", str(out_dir)]
        if sync.subtree is not None:
            argv += ["--subtree", sync.subtree]
        if sync.space is not None:
            argv += ["--space", sync.space]
        if sync.notification_page_size is not None:
            argv += ["--notification-page-size", str(sync.notification_page_size)]
        if sync.max_notification_pages is not None:
            argv += ["--max-notification-pages", str(sync.max_notification_pages)]
        for t in sync.inbox_types or []:
            argv += ["--inbox-types", t]
        if sync.subtree_max_pages is not None:
            argv += ["--subtree-max-pages", str(sync.subtree_max_pages)]
        return argv

    raise ValueError(f"unknown sync kind: {sync!r}")


def _run_timestamp() -> str:
    """Localized ISO-8601 with offset, filesystem-safe (`:` -> `-`)."""
    return datetime.now().astimezone().isoformat(timespec="seconds").replace(":", "-")


def resolve(
    source_name: str,
    config_path: Path | None = None,
    run_timestamp: str | None = None,
) -> tuple[SyncConfig, Path]:
    """Look up `source_name` in the config and return its `sync:` block plus
    the dated raw output directory for this run
    (`<root>/raw/<source-name>/<ISO-timestamp>/`). Each invocation gets its
    own subdir so consecutive runs don't trample each other; the worker
    (Phase D) will record `download_runs.raw_path` pointing at it."""
    cfg = load_config(config_path)
    for src in cfg.sources:
        if src.name != source_name:
            continue
        if src.sync is None:
            raise ValueError(
                f"source {source_name!r}: no `sync:` block in config — "
                f"cannot drive a downloader for this source"
            )
        ts = run_timestamp or _run_timestamp()
        out_dir = cfg.root / "raw" / src.name / ts
        out_dir.mkdir(parents=True, exist_ok=True)
        return src.sync, out_dir
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
    sync, out_dir = resolve(source_name, config)
    module = KIND_TO_MODULE[sync.kind]
    argv = sync_to_argv(sync, out_dir)
    cmd = [sys.executable, "-m", module, *argv]
    if dry_run:
        typer.echo(" ".join(cmd))
        return
    raise SystemExit(subprocess.call(cmd))


if __name__ == "__main__":
    typer.run(run)
