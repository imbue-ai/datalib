from __future__ import annotations

from dataclasses import asdict
from pathlib import Path

import typer

from ingest.config import load_config
from ingest.diff import diff_commits, format_report
from ingest.dolt_service import DoltService
from ingest.dump import dump_sql as run_dump
from ingest.ingest import ingest as run_ingest

app = typer.Typer(help="Personal mirror of LLM chat history into Dolt + QMD.")


@app.callback()
def _root() -> None:
    """Force subcommand mode even with a single command (for future commands)."""


@app.command()
def ingest(
    config: Path | None = typer.Option(
        None, "--config", "-c", help="Path to YAML config (default: ~/.config/claude-mirror/config.yaml)"
    ),
    now: str | None = typer.Option(
        None,
        "--now",
        help=(
            "ISO-8601 timestamp to use as ingest_started_at. Set this for "
            "deterministic / Bazel-cacheable runs; leave unset for production."
        ),
    ),
    port: int | None = typer.Option(
        None, "--port", help="Override Dolt SQL server port from config."
    ),
    dump_sql: Path | None = typer.Option(
        None,
        "--dump-sql",
        help=(
            "After ingest, write a deterministic SQL dump of the DB to this path. "
            "Useful as a downstream-test / fixture artifact."
        ),
    ),
    report: bool = typer.Option(
        True,
        "--report/--no-report",
        help="After ingest, print a row-level diff between the new commit and its parent.",
    ),
    max_samples: int = typer.Option(
        3,
        "--max-samples",
        help="Per-table sample rows to show in the report (added/modified/removed).",
    ),
) -> None:
    """Ingest every enabled source from the config; commit to Dolt; render QMD."""
    cfg = load_config(config)
    if port is not None:
        cfg.dolt.port = port
    typer.echo(f"root: {cfg.root}")
    typer.echo(f"sources: {[s.name for s in cfg.enabled_sources]}")
    summary = run_ingest(cfg, now=now)
    for s in summary.sources:
        typer.echo(f"  [{s.name}] {s.provider}/{s.kind}: {asdict(s.stats)}")
    typer.echo(f"dolt commit: {summary.commit_hash or '(no changes)'}")
    typer.echo(
        f"rendered: {summary.rendered} qmd files (removed {summary.rendered_orphans_removed} orphans)"
    )

    if dump_sql is not None:
        with DoltService(cfg) as dolt, dolt.connect() as conn:
            run_dump(conn, dump_sql)
        typer.echo(f"dump-sql: wrote {dump_sql}")

    if report and summary.commit_hash:
        with DoltService(cfg) as dolt, dolt.connect() as conn:
            rep = diff_commits(conn, from_ref=f"{summary.commit_hash}~1", to_ref=summary.commit_hash)
        typer.echo("")
        typer.echo(format_report(rep, max_samples=max_samples))


@app.command()
def diff(
    config: Path | None = typer.Option(
        None, "--config", "-c", help="Path to YAML config."
    ),
    from_ref: str = typer.Option(
        "HEAD~1", "--from", help="Dolt rev-spec for the 'before' commit."
    ),
    to_ref: str = typer.Option(
        "HEAD", "--to", help="Dolt rev-spec for the 'after' commit."
    ),
    port: int | None = typer.Option(
        None, "--port", help="Override Dolt SQL server port from config."
    ),
    max_samples: int = typer.Option(
        3,
        "--max-samples",
        help="Per-table sample rows to show (added/modified/removed).",
    ),
) -> None:
    """Human-readable row-level diff between two Dolt commits."""
    cfg = load_config(config)
    if port is not None:
        cfg.dolt.port = port
    with DoltService(cfg) as dolt, dolt.connect() as conn:
        rep = diff_commits(conn, from_ref=from_ref, to_ref=to_ref)
    typer.echo(format_report(rep, max_samples=max_samples))


@app.command()
def dump(
    config: Path | None = typer.Option(
        None, "--config", "-c", help="Path to YAML config."
    ),
    out: Path = typer.Option(..., "--out", "-o", help="Output path for the SQL dump."),
    port: int | None = typer.Option(
        None, "--port", help="Override Dolt SQL server port from config."
    ),
) -> None:
    """Emit a deterministic SQL dump of the current Dolt state to --out."""
    cfg = load_config(config)
    if port is not None:
        cfg.dolt.port = port
    with DoltService(cfg) as dolt, dolt.connect() as conn:
        run_dump(conn, out)
    typer.echo(f"wrote {out}")
