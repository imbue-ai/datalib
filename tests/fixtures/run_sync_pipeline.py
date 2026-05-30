#!/usr/bin/env python3
"""Driver for the `:ingested_tng` genrule.

Generates the two YAML configs `frankweiler-sync` needs (one for the
synth phase, one for the extract+translate phase), runs both phases, and
leaves the staged outputs where `tar_qmd.py` can pick them up.

Splitting the genrule into a python driver keeps the Bazel `cmd =` block
readable and concentrates the file-layout logic in one place — the
fixture trees live under different parent directories so a single
`--synth-input-root` flag wouldn't have worked.

Args (positional):
    1:  path to `frankweiler-sync` binary
    2:  --now stamp (ISO-8601)
    3:  data_root for the sync pipeline (rendered_md/, dolt_db/, raw/ land
        directly underneath; YAMLs + playback also stashed here)
    4:  anthropic_api fixture dir (input)
    5:  chatgpt_api   fixture dir
    6:  slack_api     fixture dir
    7:  github_api    fixture dir
    8:  gitlab_api    fixture dir
    9:  notion_web    fixture dir
    10: beeper_tng    fixture dir (SQL files + media; we materialize a
                                   BeeperTexts-shaped dir from it)
"""

from __future__ import annotations

import shutil
import sqlite3
import subprocess
import sys
from pathlib import Path


def main() -> int:
    sync_bin = Path(sys.argv[1]).resolve()
    now = sys.argv[2]
    data_root = Path(sys.argv[3]).resolve()
    anth_fx, cgpt_fx, slack_fx, gh_fx, gl_fx, notion_fx, beeper_fx = (
        Path(p).resolve() for p in sys.argv[4:11]
    )

    data_root.mkdir(parents=True, exist_ok=True)
    # YAML configs + playback fixtures + per-source raw dirs all stashed
    # under the data_root. The sync binary lays out its own `rendered_md/`
    # and `backend_index.doltlite_db` directly under data_root. The enclosing genrule is
    # sandboxed (no `no-sandbox` tag; see scripts/lint_no_sandbox.py),
    # so this dir is fresh per action — no need to clean it ourselves.
    workspace = data_root
    raw_root = data_root / "raw"
    raw_root.mkdir(exist_ok=True)
    playback = workspace / "playback"

    # Materialize a BeeperTexts-shaped directory from the SQL +
    # media fixtures. Beeper doesn't go through the synth/playback
    # flow — its extractor reads on-disk SQLite directly — so we
    # build the dbs here once and point `beeper_data_dir` at them
    # in the extract YAML.
    beeper_data_dir = _materialize_beeper_fixture(beeper_fx, workspace / "beeper_data")

    # Synth YAML: each source's input_path points at the checked-in
    # fixture tree. The synth phase reads from those and writes HTTP
    # playback responses into `playback/`.
    #
    # Beeper has no HTTP synthesizer (its `Synthesizer` impl is a
    # no-op), but we include it in the YAML anyway so the sync
    # orchestrator's `enabled_sources()` set is consistent across
    # phases. The synth pass for beeper writes zero fixtures.
    synth_yaml = workspace / "synth.yaml"
    synth_yaml.write_text(
        _yaml(
            workspace,
            {
                "anthropic-api": ("claude_api", anth_fx),
                "chatgpt-api": ("chatgpt_api", cgpt_fx),
                "slack": ("slack_api", slack_fx),
                "github": ("github_api", gh_fx),
                "gitlab": ("gitlab_api", gl_fx),
                "notion": ("notion_api", notion_fx),
                "beeper": ("beeper", beeper_data_dir),
            },
        )
    )

    # Extract YAML: each source's input_path points at a fresh per-source
    # subdir of the workspace. Extract writes there; translate reads from
    # the same place. We hand notion a seed page id from the fixture
    # tree so the (still-validated) `sync:` block is non-empty; the
    # extract phase additionally derives BFS seeds from the playback
    # responses, so this seed needn't be reachable on its own.
    notion_seed = _first_notion_page_id(notion_fx)
    extract_yaml = workspace / "extract.yaml"
    extract_yaml.write_text(
        _yaml(
            workspace,
            {
                "anthropic-api": ("claude_api", raw_root / "anthropic-api"),
                "chatgpt-api": ("chatgpt_api", raw_root / "chatgpt-api"),
                "slack": ("slack_api", raw_root / "slack"),
                "github": ("github_api", raw_root / "github"),
                "gitlab": ("gitlab_api", raw_root / "gitlab"),
                "notion": ("notion_api", raw_root / "notion"),
                # Beeper extract writes its raw doltlite into
                # `<input_path>.doltlite_db`. The on-disk source
                # (BeeperTexts-shaped dir) is configured via the
                # sync block's `beeper_data_dir:` field; see
                # `_yaml` below.
                "beeper": ("beeper", raw_root / "beeper"),
            },
            notion_seed=notion_seed,
            beeper_data_dir=beeper_data_dir,
        )
    )

    # Anthropic extract reads users.json from `export_dir` (== input_path
    # in our wiring) — that file is a bulk-export artifact, not an HTTP
    # response, so seed it from the checked-in fixture tree.
    anth_raw = raw_root / "anthropic-api"
    anth_raw.mkdir(parents=True, exist_ok=True)
    users_src = anth_fx / "users.json"
    if users_src.exists():
        shutil.copy(users_src, anth_raw / "users.json")

    print(f"[run_sync_pipeline] synth → {playback}", flush=True)
    _run(
        [
            str(sync_bin),
            "--config",
            str(synth_yaml),
            "--now",
            now,
            "--synthesize-playback-root",
            str(playback),
        ]
    )

    print(f"[run_sync_pipeline] extract+translate → {data_root}", flush=True)
    _run(
        [
            str(sync_bin),
            "--config",
            str(extract_yaml),
            "--now",
            now,
            "--playback-root",
            str(playback),
        ]
    )
    return 0


def _yaml(
    data_root: Path,
    sources: dict[str, tuple[str, Path]],
    notion_seed: str | None = None,
    beeper_data_dir: Path | None = None,
) -> str:
    """Render a minimal YAML covering every fixture source.

    `sources` maps name → (type_str, input_path). Notion needs a non-empty
    sync block to pass validation — pass `notion_seed` to use a real page
    id as a `subtrees.pages` entry. When `notion_seed` is None (the synth
    phase, which doesn't actually fetch) we fall back to `inbox.enabled`.
    Beeper's `sync:` block needs both a non-empty `sources:` list and
    a path to a BeeperTexts-shaped dir.
    """
    lines = [
        f"data_root: {data_root}",
        "qmd:",
        "  skip: true",
        "sources:",
    ]
    for name, (type_str, path) in sources.items():
        lines.append(f"  - name: {name}")
        lines.append(f"    type: {type_str}")
        lines.append(f"    input_path: {path}")
        if type_str == "notion_api":
            lines.append("    sync:")
            if notion_seed:
                lines.append(f"      subtrees: {{pages: ['{notion_seed}']}}")
            else:
                lines.append("      inbox: {enabled: true}")
        elif type_str == "slack_api":
            # Disable media so extract doesn't fall back to the direct
            # `latchkey curl -v` path for file downloads (not on PATH in
            # the bazel sandbox, and the fixtures don't exercise media).
            lines.append("    sync: {media: false}")
        elif type_str == "beeper":
            # `sources` here is the canonical-network list that
            # filters which rooms get ingested. `beeper_data_dir`
            # points at the materialized BeeperTexts fixture.
            lines.append("    sync:")
            lines.append("      sources: ['signal', 'googlechat']")
            if beeper_data_dir is not None:
                lines.append(f"      beeper_data_dir: {beeper_data_dir}")
        elif type_str != "claude_export":
            lines.append("    sync: {}")
    return "\n".join(lines) + "\n"


def _materialize_beeper_fixture(fx_dir: Path, target: Path) -> Path:
    """Build a BeeperTexts-shaped data directory from the SQL +
    media checked in at `fx_dir`. Mirrors `build_fixture.sh`, but
    uses Python's stdlib sqlite3 so we don't depend on the system
    `sqlite3` CLI being on the sandbox PATH at *build* time. (The
    extract step at run time does shell out to `sqlite3`; that's a
    separate concern, addressed by the genrule's runtime sandbox.)
    """
    target.mkdir(parents=True, exist_ok=True)
    (target / "local-signal").mkdir(exist_ok=True)
    (target / "media" / "local.beeper.com").mkdir(parents=True, exist_ok=True)
    (target / "media" / "localhostlocal-signal").mkdir(parents=True, exist_ok=True)

    _load_sql(fx_dir / "index_db.sql", target / "index.db")
    _load_sql(
        fx_dir / "local_signal_megabridge.sql",
        target / "local-signal" / "megabridge.db",
    )

    shutil.copy(
        fx_dir / "media" / "local.beeper.com" / "TNGRPT01",
        target / "media" / "local.beeper.com" / "TNGRPT01",
    )
    shutil.copy(
        fx_dir / "media" / "localhostlocal-signal" / "TNGART01",
        target / "media" / "localhostlocal-signal" / "TNGART01",
    )
    return target


def _load_sql(sql_path: Path, db_path: Path) -> None:
    """Execute every statement in `sql_path` against a fresh
    sqlite3 file at `db_path`. Stdlib's sqlite3 module has
    JSON1 enabled by default (we rely on `json_object` /
    `json_array` in the fixture), and `executescript` handles
    multi-statement SQL."""
    if db_path.exists():
        db_path.unlink()
    conn = sqlite3.connect(str(db_path))
    try:
        conn.executescript(sql_path.read_text())
        conn.commit()
    finally:
        conn.close()


def _first_notion_page_id(notion_fx: Path) -> str | None:
    """Pick any page id from the notion fixture tree to anchor the
    config's required `subtrees.pages` entry. BFS in extract derives
    the actual fetched set from playback fixtures, so this seed only
    needs to satisfy validation."""
    import json

    candidate = notion_fx / "notion_official_page" / "created" / "events.jsonl"
    if not candidate.exists():
        return None
    with candidate.open() as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(obj, dict) and obj.get("id"):
                return obj["id"]
    return None


def _run(argv: list[str]) -> None:
    print("[run_sync_pipeline] $", " ".join(argv), flush=True)
    subprocess.run(argv, check=True)


if __name__ == "__main__":
    sys.exit(main())
