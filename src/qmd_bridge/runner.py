"""Thin wrapper around the `qmd` CLI.

The qmd CLI is shelled out via `npx -y @tobilu/qmd@<version>` — same
incantation the indexer genrule uses, so versions stay aligned. The
runner doesn't build the index; it expects one already present at
`<qmd_root>/.frankweiler/qmd/index.sqlite` (produced by
`//tests/fixtures:ingested_tng_qmd`).

Search modes:
  * `query`   — hybrid (BM25 + vectors + reranker). What a user types
                into the search bar maps to this.
  * `vsearch` — vector-only. Faster, no LLM reranking — useful for
                tests and for the `qmd_vsearch:"..."` predicate.

Output parsing: `--json` is concatenated after a non-JSON status banner
on stdout. We find the first `[` and parse from there.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
from dataclasses import dataclass, field
from pathlib import Path

from qmd_bridge.mapping import QmdHit, QueryMode


DEFAULT_QMD_VERSION = "2.1.0"
DEFAULT_COLLECTION = "mirror"


@dataclass
class QmdRunnerConfig:
    qmd_root: Path
    """The qmd collection root — i.e. the directory containing the
    rendered markdown tree AND the `.frankweiler/qmd/index.sqlite` index.
    """

    qmd_version: str = DEFAULT_QMD_VERSION
    collection: str = DEFAULT_COLLECTION
    extra_env: dict[str, str] = field(default_factory=dict)

    @property
    def cache_home(self) -> Path:
        return self.qmd_root / ".frankweiler"


class QmdRunner:
    def __init__(self, config: QmdRunnerConfig) -> None:
        self.config = config
        index = config.cache_home / "qmd" / "index.sqlite"
        if not index.exists():
            raise FileNotFoundError(f"qmd index not found at {index}")

    def query(self, q: str, limit: int = 10) -> list[QmdHit]:
        return self._run("query", q, limit, extra=["--no-rerank"])

    def vsearch(self, q: str, limit: int = 10) -> list[QmdHit]:
        return self._run("vsearch", q, limit)

    def search(self, mode: QueryMode, q: str, limit: int = 10) -> list[QmdHit]:
        return self.query(q, limit) if mode == "query" else self.vsearch(q, limit)

    def _run(
        self, mode: str, q: str, limit: int, extra: list[str] | None = None
    ) -> list[QmdHit]:
        pkg = f"@tobilu/qmd@{self.config.qmd_version}"
        cmd = ["npx", "-y", pkg, mode, q, "-n", str(limit), "--json"]
        if extra:
            cmd.extend(extra)
        env = os.environ.copy()
        env["XDG_CACHE_HOME"] = str(self.config.cache_home)
        env["XDG_CONFIG_HOME"] = str(self.config.cache_home)
        env.update(self.config.extra_env)
        proc = subprocess.run(
            cmd,
            env=env,
            capture_output=True,
            text=True,
            check=False,
        )
        if proc.returncode != 0:
            raise RuntimeError(
                f"qmd {mode} failed (rc={proc.returncode}): {proc.stderr.strip()}"
            )
        return self._parse_stdout(proc.stdout)

    def _parse_stdout(self, stdout: str) -> list[QmdHit]:
        # qmd prints a status banner before the JSON array. The banner
        # itself may contain `[...]`-shaped fragments (e.g. progress
        # markers), so we can't just `find('[')`. Look for a `[` at the
        # start of a line — that's the JSON array opener.
        m = re.search(r"^\[", stdout, re.MULTILINE)
        if m is None:
            return []
        data = json.loads(stdout[m.start() :])
        out: list[QmdHit] = []
        for d in data:
            path = self._strip_uri(d["file"])
            out.append(
                QmdHit(
                    path=path,
                    score=float(d.get("score", 0.0)),
                    snippet=d.get("snippet", "") or "",
                    docid=d.get("docid", "") or "",
                    title=d.get("title", "") or "",
                )
            )
        return out

    _URI_RE = re.compile(r"^qmd://[^/]+/")

    def _strip_uri(self, uri: str) -> str:
        """`qmd://mirror/foo/bar.qmd` -> `foo/bar.qmd`."""
        return self._URI_RE.sub("", uri)
