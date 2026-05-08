#!/usr/bin/env python3
"""Driver invoked by the Bazel genrule that runs the full pipeline against
the TNG fixtures and emits the cacheable artifacts (dump.sql, qmd.tar).

Why a script instead of inlining shell into the genrule? Three things have
to happen atomically and would each be ugly in a `cmd =` string:

  1. Pick a free TCP port (Dolt's sql-server has no `--port=0` mode that
     reports the chosen port, and parallel Bazel actions must not collide).
  2. Synthesize a config.yaml whose paths point at the sandboxed copies of
     the fixture trees, not the workspace.
  3. Tar the rendered QMD tree into a single declared output (Bazel
     genrules emit a fixed, declared file list).

Args (positional):
    1: path to the ingest CLI py_binary launcher
    2: workspace-relative dir containing the fixture trees (anthropic_export/,
       anthropic_api/, chatgpt_api/)
    3: output dir for dump.sql + qmd.tar (Bazel-supplied)
    4: --now value (fixed ISO-8601 timestamp)
"""

from __future__ import annotations

import os
import socket
import subprocess
import sys
import tarfile
from pathlib import Path


def _free_port() -> int:
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def main() -> int:
    cli, fixtures_dir, out_dir, now = sys.argv[1:5]
    fixtures = Path(fixtures_dir).resolve()
    out = Path(out_dir).resolve()
    out.mkdir(parents=True, exist_ok=True)

    work = out / "work"
    work.mkdir(exist_ok=True)
    root = work / "root"
    root.mkdir(exist_ok=True)

    port = _free_port()
    config_path = work / "config.yaml"
    config_path.write_text(
        f"""\
root: {root}
dolt:
  port: {port}
sources:
  - name: anthropic_export_tng
    provider: anthropic
    kind: export_dir
    path: {fixtures / "anthropic_export"}
    provenance: export
  - name: anthropic_api_tng
    provider: anthropic
    kind: export_dir
    path: {fixtures / "anthropic_api"}
    provenance: api
  - name: chatgpt_api_tng
    provider: openai
    kind: chatgpt_api_dir
    path: {fixtures / "chatgpt_api"}
"""
    )

    dump_sql = out / "dump.sql"
    cmd = [
        cli,
        "ingest",
        "--config",
        str(config_path),
        "--now",
        now,
        "--dump-sql",
        str(dump_sql),
    ]
    env = os.environ.copy()
    # Dolt may try to read a global config; isolate it to the sandbox.
    env["HOME"] = str(work)
    # Bazel scrubs PATH for hermeticity. Re-add the common locations where
    # `dolt` is installed by Homebrew (Apple Silicon + Intel) and Linux pkg
    # managers, plus whatever the invoking shell had.
    extra_paths = [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
    ]
    parent_path = os.environ.get("CLAUDE_MIRROR_HOST_PATH") or os.environ.get(
        "PATH", ""
    )
    env["PATH"] = ":".join([p for p in extra_paths + parent_path.split(":") if p])
    r = subprocess.run(cmd, env=env)
    if r.returncode != 0:
        return r.returncode

    # Tar the QMD tree (rooted at "qmd/<provider>/...") into a single output.
    # Crucially, exclude the live Dolt repo dir under <root>/dolt_repo/ \u2014
    # its internal chunk store / journal files are not byte-stable across
    # runs (commit hashes, packing differences) and would bust the cache.
    qmd_tar = out / "qmd.tar"
    qmd_subtrees = [d for d in ("anthropic", "openai") if (root / d).is_dir()]
    with tarfile.open(qmd_tar, "w") as tf:
        entries: list[Path] = []
        for sub in qmd_subtrees:
            entries.extend(
                p for p in (root / sub).rglob("*") if p.is_file() or p.is_dir()
            )
        entries.sort()
        for p in entries:
            arcname = "qmd/" + str(p.relative_to(root))
            ti = tf.gettarinfo(str(p), arcname=arcname)
            # Strip mtime/uid/gid/size-of-dir-noise so the tar is hermetic.
            ti.mtime = 0
            ti.uid = 0
            ti.gid = 0
            ti.uname = ""
            ti.gname = ""
            if p.is_file():
                with open(p, "rb") as f:
                    tf.addfile(ti, f)
            else:
                tf.addfile(ti)
    return 0


if __name__ == "__main__":
    sys.exit(main())
