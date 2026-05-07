from __future__ import annotations

from dataclasses import asdict
from pathlib import Path

import typer

from claude_mirror.config import load_config
from claude_mirror.ingest import ingest as run_ingest

app = typer.Typer(help="Personal mirror of LLM chat history into Dolt + QMD.")


@app.callback()
def _root() -> None:
    """Force subcommand mode even with a single command (for future commands)."""


@app.command()
def ingest(
    config: Path | None = typer.Option(
        None, "--config", "-c", help="Path to YAML config (default: ~/.config/claude-mirror/config.yaml)"
    ),
) -> None:
    """Ingest every enabled source from the config; commit to Dolt; render QMD."""
    cfg = load_config(config)
    typer.echo(f"root: {cfg.root}")
    typer.echo(f"sources: {[s.name for s in cfg.enabled_sources]}")
    summary = run_ingest(cfg)
    for s in summary.sources:
        typer.echo(f"  [{s.name}] {s.provider}/{s.kind}: {asdict(s.stats)}")
    typer.echo(f"dolt commit: {summary.commit_hash or '(no changes)'}")
    typer.echo(
        f"rendered: {summary.rendered} qmd files (removed {summary.rendered_orphans_removed} orphans)"
    )
