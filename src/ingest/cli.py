from __future__ import annotations

import logging
import sqlite3
import sys
import tempfile
from dataclasses import asdict
from pathlib import Path

import typer

from ingest.config import load_config
from ingest.diff import diff_commits, format_report
from ingest.dolt_service import DoltService
from ingest.dump import dump_sql as run_dump
from ingest.ingest import ingest as run_ingest
from ingest.qmd_index import build_qmd_index


def _configure_logging(verbose: bool = False) -> None:
    """Idempotent logger setup. tqdm writes its bars to stderr, so we send
    log records to stderr too; that way redirecting stdout still leaves the
    user-facing summary clean while progress + logs share one channel."""
    root = logging.getLogger()
    if root.handlers:
        return
    handler = logging.StreamHandler(sys.stderr)
    handler.setFormatter(
        logging.Formatter("%(asctime)s %(levelname)s %(name)s: %(message)s")
    )
    root.addHandler(handler)
    root.setLevel(logging.DEBUG if verbose else logging.INFO)


def _materialize_mirror_sqlite(conn, out_path: Path) -> None:
    """Write `<root>/mirror.sqlite` from a fresh dump of the live Dolt conn.

    Backend (frankweiler) reads this for grid queries — it's the schema-driven
    counterpart to the QMD prose. Always rewritten from scratch so it stays a
    pure function of current Dolt state.
    """
    with tempfile.NamedTemporaryFile(suffix=".sql", delete=False) as tmp:
        tmp_path = Path(tmp.name)
    try:
        run_dump(conn, tmp_path)
        if out_path.exists():
            out_path.unlink()
        out_path.parent.mkdir(parents=True, exist_ok=True)
        sconn = sqlite3.connect(str(out_path))
        try:
            sconn.executescript(tmp_path.read_text())
            sconn.commit()
        finally:
            sconn.close()
    finally:
        tmp_path.unlink(missing_ok=True)


app = typer.Typer(help="mixed-up-files: mirror LLM chat history into Dolt + markdown.")


@app.callback()
def _root() -> None:
    """Force subcommand mode even with a single command (for future commands)."""


@app.command()
def ingest(
    config: Path | None = typer.Option(
        None,
        "--config",
        "-c",
        help="Path to YAML config (default: ~/.config/mixed-up-files/config.yaml)",
    ),
    verbose: bool = typer.Option(False, "--verbose", "-v", help="DEBUG-level logging."),
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
    qmd_index: bool = typer.Option(
        True,
        "--qmd-index/--no-qmd-index",
        help="After rendering, rebuild the qmd search index over <root>.",
    ),
    max_samples: int = typer.Option(
        3,
        "--max-samples",
        help="Per-table sample rows to show in the report (added/modified/removed).",
    ),
) -> None:
    """Ingest every enabled source from the config; commit to Dolt; render QMD."""
    _configure_logging(verbose)
    cfg = load_config(config)
    if port is not None:
        cfg.dolt.port = port
    typer.echo(f"data_root: {cfg.data_root}")
    typer.echo(f"sources: {[s.name for s in cfg.enabled_sources]}")
    summary = run_ingest(cfg, now=now)
    for s in summary.sources:
        typer.echo(f"  [{s.name}] {s.type}: {asdict(s.stats)}")
    typer.echo(f"dolt commit: {summary.commit_hash or '(no changes)'}")
    typer.echo(
        f"rendered: {summary.rendered} qmd files (removed {summary.rendered_orphans_removed} orphans)"
    )

    mirror_path = cfg.data_root / "mirror.sqlite"
    with DoltService(cfg) as dolt, dolt.connect() as conn:
        _materialize_mirror_sqlite(conn, mirror_path)
        if dump_sql is not None:
            run_dump(conn, dump_sql)
        rep = (
            diff_commits(
                conn, from_ref=f"{summary.commit_hash}~1", to_ref=summary.commit_hash
            )
            if report and summary.commit_hash
            else None
        )
    typer.echo(f"mirror.sqlite: wrote {mirror_path}")
    if dump_sql is not None:
        typer.echo(f"dump-sql: wrote {dump_sql}")
    if rep is not None:
        typer.echo("")
        typer.echo(format_report(rep, max_samples=max_samples))

    if qmd_index:
        typer.echo("")
        typer.echo("qmd-index: rebuilding...")
        index_path = build_qmd_index(cfg.data_root)
        typer.echo(f"qmd-index: wrote {index_path}")


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
