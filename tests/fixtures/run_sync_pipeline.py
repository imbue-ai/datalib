#!/usr/bin/env python3
"""Driver for the `:ingested_tng` genrule.

Generates the DAG config `datalib-dag` needs, runs the synth phase
(per-source `datalib-step synthesize`) and then the pipeline
(download → render → index) hermetically against playback fixtures,
and leaves the staged outputs where `tar_qmd.py` can pick them up.

Splitting the genrule into a python driver keeps the Bazel `cmd =` block
readable and concentrates the file-layout logic in one place — the
fixture trees live under different parent directories so a single
`--synth-input-root` flag wouldn't have worked.

Args (positional):
    1:  path to `datalib-dag` binary (the DAG runner)
    2:  path to `datalib-step` binary (the step-type host)
    3:  path to `signal-make-fixture` binary (used to expand the signal
        TNG JSON spec into an encrypted snapshot dir on the fly)
    4:  path to `whatsapp-make-fixture` binary (same idea for the
        WhatsApp TNG spec — produces a `WhatsApp/` backup dir with
        `Databases/msgstore.db.crypt15` + `Media/`)
    5:  --now stamp (ISO-8601)
    6:  data_root for the pipeline (rendered_md/, system/, raw/ land
        directly underneath; the DAG config + playback also stashed here)
    7:  anthropic_api fixture dir (input)
    8:  chatgpt_api   fixture dir
    9:  slack_api     fixture dir
    10: github_api    fixture dir
    11: gitlab_api    fixture dir
    12: notion_web    fixture dir
    13: beeper_tng    fixture dir (SQL files + media; we materialize a
                                   BeeperTexts-shaped dir from it)
    14: carddav_tng   fixture dir (vCard files; file mode — extract
                                   walks `.vcf` files straight from
                                   `input_path`)
    15: signal_tng    JSON spec for the TNG signal backup; the path is
                      to the .json file itself. We run
                      `signal-make-fixture` against it to materialize
                      an encrypted snapshot dir for extract to walk.
                      AEP is the public 64-zero fixture passphrase
                      (`SIGNAL_BACKUP_PASSPHRASE` env var set below).
    16: whatsapp_tng  JSON spec for the TNG WhatsApp backup. Same
                      idea: we expand to a `WhatsApp/` backup dir
                      under the workspace, with root key = 64 zeros
                      (`WHATSAPP_BACKUP_DECRYPTION_KEY` env var
                      set below).
    17: email_mbox    Path to a Google-Takeout-shaped `.mbox` file
                      (e.g. `star_trek.mbox`). The email extractor
                      walks it directly — no synth phase. Account
                      metadata (display name, address, is_personal)
                      is supplied via the source's `mbox:` block.
    18: gtk_fx        Google Takeout root dir.
    19: linkedin_fx   LinkedIn data-export dir (CSVs + Articles HTML).
                      File-backed; extract walks `input_path` directly.
    20: sms_fx        "SMS Backup & Restore" export dir (sms-*.xml /
                      calls-*.xml with inline base64 attachments).
                      File-backed; extract walks `input_path` directly.
"""

from __future__ import annotations

import json
import os
import shutil
import sqlite3
import subprocess
import sys
from pathlib import Path

# Public fixture AEP — the signal-tng fixture is generated and
# decrypted with this passphrase. Documented in the signal-backup
# crate. NOT a secret; the whole point of having a fixed AEP is so
# the crypto path runs unchanged against synthetic data.
FIXTURE_SIGNAL_AEP = "0" * 64

# Public fixture root key for WhatsApp — 64 zeros, same idea as
# `FIXTURE_SIGNAL_AEP`. The whatsapp-make-fixture binary defaults
# to this; we set the env var here so the extract path doesn't
# need any special wiring.
FIXTURE_WHATSAPP_KEY = "0" * 64


def main() -> int:
    dag_bin = Path(sys.argv[1]).resolve()
    step_bin = Path(sys.argv[2]).resolve()
    signal_make_fixture_bin = Path(sys.argv[3]).resolve()
    whatsapp_make_fixture_bin = Path(sys.argv[4]).resolve()
    now = sys.argv[5]
    data_root = Path(sys.argv[6]).resolve()
    (
        anth_fx,
        cgpt_fx,
        slack_fx,
        gh_fx,
        gl_fx,
        notion_fx,
        beeper_fx,
        carddav_fx,
        signal_spec,
        whatsapp_spec,
        email_mbox,
        gtk_fx,
        linkedin_fx,
        sms_fx,
    ) = (Path(p).resolve() for p in sys.argv[7:21])

    data_root.mkdir(parents=True, exist_ok=True)
    # The DAG config + playback fixtures + per-source input dirs all
    # stashed under the data_root. The pipeline lays out its own
    # `<name>/raw`, `<name>/rendered_md`, and `system/` directly under
    # data_root. The enclosing genrule is sandboxed (no `no-sandbox`
    # tag; see scripts/lint_no_sandbox.py), so this dir is fresh per
    # action — no need to clean it ourselves.
    workspace = data_root
    raw_root = data_root / "raw"
    raw_root.mkdir(exist_ok=True)
    playback = workspace / "playback"

    # Materialize a BeeperTexts-shaped directory from the SQL +
    # media fixtures. Beeper doesn't go through the synth/playback
    # flow — its extractor reads on-disk SQLite directly — so we
    # build the dbs here once and point `beeper_data_dir` at them.
    beeper_data_dir = _materialize_beeper_fixture(beeper_fx, workspace / "beeper_data")

    # Signal: expand the JSON spec into an encrypted snapshot dir. The
    # extractor will scan this dir for `signal-backup-*` subdirs and
    # pick the newest one — same code path as a real user's
    # `~/backups/SignalBackups`.
    #
    # Skip regen if a snapshot already exists. The signal resume cursor
    # is `(mtime_ns, byte_size)`-keyed (see
    # `providers/signal/src/extract/schema_raw.rs::INGESTED_BACKUPS_DDL`),
    # so re-running `signal_make_fixture_bin` would touch the files
    # and defeat the cursor across pipeline runs sharing the same
    # workspace.
    signal_snapshot_root = workspace / "signal_snapshots"
    signal_snapshot_root.mkdir(exist_ok=True)
    existing_signal_snaps = [
        p for p in signal_snapshot_root.iterdir() if p.name.startswith("signal-backup-")
    ]
    if not existing_signal_snaps:
        _run(
            [str(signal_make_fixture_bin), str(signal_spec), str(signal_snapshot_root)]
        )

    # WhatsApp: same idea — expand the spec into a `WhatsApp/` backup
    # dir under the workspace. The extractor's `WhatsAppSync.backup_dir`
    # points here directly (no scan-for-newest as with Signal — there's
    # only the single dir).
    whatsapp_root = workspace / "whatsapp_backup"
    whatsapp_root.mkdir(exist_ok=True)
    _run([str(whatsapp_make_fixture_bin), str(whatsapp_spec), str(whatsapp_root)])
    whatsapp_dir = whatsapp_root / "WhatsApp"

    # Every source: name → (source type, synth input fixture dir,
    # extract-phase input_path). The synth input is the checked-in
    # fixture tree the synthesizer reads; the extract input_path is
    # what the provider walks at pipeline time (per-source raw subdirs
    # for the HTTP providers, the fixture/export trees for the
    # file-backed ones). Raw doltlite stores always land at the
    # canonical `<data_root>/<name>/raw` regardless.
    sources: dict[str, tuple[str, Path, Path]] = {
        "anthropic-api": ("claude_api", anth_fx, raw_root / "anthropic-api"),
        "chatgpt-api": ("chatgpt_api", cgpt_fx, raw_root / "chatgpt-api"),
        "slack": ("slack_api", slack_fx, raw_root / "slack"),
        "github": ("github_api", gh_fx, raw_root / "github"),
        "gitlab": ("gitlab_api", gl_fx, raw_root / "gitlab"),
        "notion": ("notion_api", notion_fx, raw_root / "notion"),
        "beeper": ("beeper", beeper_data_dir, raw_root / "beeper"),
        "tng_contacts": ("carddav", carddav_fx, carddav_fx),
        "signal": ("signal_backup", signal_snapshot_root, raw_root / "signal"),
        "whatsapp": ("whatsapp_backup", whatsapp_dir, raw_root / "whatsapp"),
        "tng_email": ("email", email_mbox, email_mbox),
        "google-takeout": ("google_takeout", gtk_fx, gtk_fx),
        "linkedin": ("linkedin", linkedin_fx, linkedin_fx),
        "sms-backup-restore": ("sms_backup_restore", sms_fx, sms_fx),
    }

    # ── Synth: build HTTP playback fixtures per source. ─────────────
    # `datalib-step synthesize` reads each source's fixture tree
    # (`common.input_path`) and writes replay tapes into `playback/`.
    # Sources without an HTTP synthesizer (beeper, signal, …) log a
    # skip and write nothing — invoked anyway for symmetry, exactly
    # like the old whole-config synth pass.
    print(f"[run_sync_pipeline] synth → {playback}", flush=True)
    step_env = {**os.environ, "FRANKWEILER_DAG_DATA_ROOT": str(workspace)}
    for name, (type_str, synth_input, _extract_input) in sources.items():
        source: dict = {"common": {"input_path": str(synth_input)}}
        if type_str == "linkedin":
            # The photo fetch is linkedin's one HTTP path; the synth
            # gate checks this flag.
            source["fetch_photos"] = True
        _run(
            [
                str(step_bin),
                "synthesize",
                type_str,
                "--name",
                name,
                "--params",
                json.dumps(source),
                "--out",
                str(playback),
            ],
            env=step_env,
        )

    # ── DAG config: a download+render pair per source, plus the index
    # fan-in. qmd is skipped here (the old configs set `qmd.skip`);
    # `:ingested_tng_qmd` builds the search index separately.
    notion_seed = _first_notion_page_id(notion_fx)
    steps: list[str] = []
    for name, (type_str, _synth_input, extract_input) in sources.items():
        # Per-phase params, verbatim JSON (valid YAML). The source name
        # isn't in either — each step derives it from its first
        # declared output. Download gets the provider config subtree;
        # render gets only the render-side knobs (most sources: none).
        params = json.dumps(
            _source_config(
                type_str,
                extract_input,
                notion_seed=notion_seed,
                beeper_data_dir=beeper_data_dir,
                signal_snapshot_root=signal_snapshot_root,
                whatsapp_dir=whatsapp_dir,
            )
        )
        render_params = _render_config(type_str)
        render_params_line = (
            f"\n    params: {json.dumps(render_params)}" if render_params else ""
        )
        steps.append(
            f"""  - id: {name}.download
    command: datalib-step download {type_str}
    outputs: [{name}/raw]
    params: {params}
  - id: {name}.render
    command: datalib-step render {type_str}
    inputs: [{name}/raw]
    outputs: [{name}/rendered_md]{render_params_line}"""
        )
    steps.append(
        """  - id: grid_index
    command: datalib-step grid_index
    inputs: ["**/rendered_md"]
    outputs: [system/backend_index]"""
    )
    dag_yaml = workspace / "dag.yaml"
    dag_yaml.write_text(f"data_root: {workspace}\nsteps:\n" + "\n".join(steps) + "\n")

    # Step commands resolve `datalib-step` via PATH; bazel names the
    # binary `datalib_step`, so stage a dash-named symlink dir and hand
    # it to the runner as --binary-dir.
    bindir = workspace / "bindir"
    bindir.mkdir(exist_ok=True)
    step_link = bindir / "datalib-step"
    if not step_link.exists():
        step_link.symlink_to(step_bin)

    # Anthropic extract reads users.json from `export_dir` (== input_path
    # in our wiring) — that file is a bulk-export artifact, not an HTTP
    # response, so seed it from the checked-in fixture tree.
    anth_raw = raw_root / "anthropic-api"
    anth_raw.mkdir(parents=True, exist_ok=True)
    users_src = anth_fx / "users.json"
    if users_src.exists():
        shutil.copy(users_src, anth_raw / "users.json")

    print(f"[run_sync_pipeline] pipeline → {data_root}", flush=True)
    # `FRANKWEILER_HTTP_PLAYBACK` redirects every provider transport to
    # the playback tree (steps inherit the runner's env); the fixture
    # AEP/root key let the signal/whatsapp extractors decrypt the
    # snapshots generated above.
    pipeline_env = {
        **os.environ,
        "FRANKWEILER_HTTP_PLAYBACK": str(playback),
        "SIGNAL_BACKUP_PASSPHRASE": FIXTURE_SIGNAL_AEP,
        "WHATSAPP_BACKUP_DECRYPTION_KEY": FIXTURE_WHATSAPP_KEY,
    }
    pipeline_argv = [
        str(dag_bin),
        str(dag_yaml),
        "--binary-dir",
        str(bindir),
        "--now",
        now,
    ]
    # `INGESTED_TNG_RESET=1` is the env-var pass-through used by
    # ingested_tng_test's multi-run case to exercise the
    # --reset-and-redownload code path without changing the positional
    # arg signature.
    if os.environ.get("INGESTED_TNG_RESET") == "1":
        pipeline_argv.append("--reset-and-redownload")
    _run(pipeline_argv, env=pipeline_env)
    return 0


def _source_config(
    type_str: str,
    input_path: Path,
    notion_seed: str | None = None,
    beeper_data_dir: Path | None = None,
    signal_snapshot_root: Path | None = None,
    whatsapp_dir: Path | None = None,
) -> dict:
    """The provider config subtree (step `params:`) for one fixture source.

    Mirrors the knobs the old sync YAML carried, minus the `type:` tag
    (the command's subcommand names the provider now). Notion needs a non-empty
    sync block to pass validation — `notion_seed` anchors a
    `subtrees.pages` entry (extract additionally derives BFS seeds
    from the playback responses, so the seed needn't be reachable on
    its own).
    """
    source: dict = {"common": {"input_path": str(input_path)}}
    if type_str == "notion_api":
        if notion_seed:
            source["sync"] = {"subtrees": {"pages": [notion_seed]}}
        else:
            source["sync"] = {"inbox": {"enabled": True}}
    elif type_str == "slack_api":
        # Disable media so extract doesn't fall back to the direct
        # `latchkey curl -v` path for file downloads (not on PATH in
        # the bazel sandbox, and the fixtures don't exercise media).
        source["sync"] = {"media": False}
    elif type_str == "beeper":
        # `sources` here is the canonical-network list that filters
        # which rooms get ingested. `beeper_data_dir` points at the
        # materialized BeeperTexts fixture.
        source["sync"] = {"sources": ["signal", "googlechat"]}
        if beeper_data_dir is not None:
            source["sync"]["beeper_data_dir"] = str(beeper_data_dir)
    elif type_str == "carddav":
        # File-tree mode: no `sync:` block (otherwise we'd be in
        # CardDAV-server mode). Extract walks `input_path` for `.vcf`
        # files; translate reads the raw doltlite store.
        pass
    elif type_str == "signal_backup":
        # The signal extractor needs `snapshot_dir` (where the
        # `signal-backup-*` subdirs live) in addition to the raw store.
        # AEP comes from the SIGNAL_BACKUP_PASSPHRASE env var.
        source["sync"] = (
            {"snapshot_dir": str(signal_snapshot_root)}
            if signal_snapshot_root is not None
            else {}
        )
    elif type_str == "email":
        # Mbox mode: no `sync:` block (would otherwise trigger the
        # JMAP path). Account metadata is supplied via the `mbox:`
        # block so the synthesized `accounts` row carries display name
        # + canonical address — same shape JMAP would produce. (The
        # Gmail outlink format is a render knob — see _render_config.)
        source["mbox"] = {
            "account_id": "picard@enterprise.starfleet",
            "display_name": "Jean-Luc Picard",
            "email_address": "picard@enterprise.starfleet",
            "is_personal": True,
        }
    elif type_str == "whatsapp_backup":
        # WhatsApp extractor needs `backup_dir` (the dir containing
        # `Databases/msgstore.db.crypt15` + `Media/`). Root key comes
        # from the WHATSAPP_BACKUP_DECRYPTION_KEY env var.
        source["sync"] = (
            {"backup_dir": str(whatsapp_dir)} if whatsapp_dir is not None else {}
        )
    elif type_str == "linkedin":
        # File-backed CSV walk (no `sync:` block). Turn on the
        # connection-photo fetch so the pipeline exercises the
        # og:image → CAS path — hermetically, against the playback
        # fixtures LinkedinSynth wrote in the synth phase.
        source["fetch_photos"] = True
    elif type_str == "google_takeout":
        # Opt into the rendering feeds: Google Chat and Google Voice
        # (incl. its Spam folder, to exercise that path). The other
        # feeds stay off for the central pipeline (their extract is
        # covered by the provider's own fixture_walk test).
        source["sync"] = {
            "google_chat": True,
            "google_voice": True,
            "google_voice_include_spam": True,
        }
    elif type_str == "sms_backup_restore":
        # File-backed, no `sync:` field at all (deny_unknown_fields
        # would reject `sync: {}`). Extract walks `input_path`.
        pass
    else:
        source["sync"] = {}
    return source


def _render_config(type_str: str) -> dict | None:
    """The render step's params for one fixture source — only the
    render-side knobs (per-phase params split). Most sources need
    none; email carries the webmail outlink format for its
    Google-Takeout-shaped `.mbox`."""
    if type_str == "email":
        return {"outlink_format": "gmail"}
    return None


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


def _run(argv: list[str], env: dict[str, str] | None = None) -> None:
    print("[run_sync_pipeline] $", " ".join(argv), flush=True)
    subprocess.run(argv, check=True, env=env)


if __name__ == "__main__":
    sys.exit(main())
