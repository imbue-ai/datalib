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
    2:  path to `signal-make-fixture` binary (used to expand the signal
        TNG JSON spec into an encrypted snapshot dir on the fly)
    3:  path to `whatsapp-make-fixture` binary (same idea for the
        WhatsApp TNG spec — produces a `WhatsApp/` backup dir with
        `Databases/msgstore.db.crypt15` + `Media/`)
    4:  --now stamp (ISO-8601)
    5:  data_root for the sync pipeline (rendered_md/, dolt_db/, raw/ land
        directly underneath; YAMLs + playback also stashed here)
    6:  anthropic_api fixture dir (input)
    7:  chatgpt_api   fixture dir
    8:  slack_api     fixture dir
    9:  github_api    fixture dir
    10: gitlab_api    fixture dir
    11: notion_web    fixture dir
    12: beeper_tng    fixture dir (SQL files + media; we materialize a
                                   BeeperTexts-shaped dir from it)
    13: carddav_tng   fixture dir (vCard files; translate-only —
                                    config carries no `sync:` block so
                                    extract is skipped and translate
                                    reads `.vcf` files straight from
                                    `input_path`)
    14: signal_tng    JSON spec for the TNG signal backup; the path is
                      to the .json file itself. We run
                      `signal-make-fixture` against it to materialize
                      an encrypted snapshot dir for extract to walk.
                      AEP is the public 64-zero fixture passphrase
                      (`SIGNAL_BACKUP_PASSPHRASE` env var set below).
    15: whatsapp_tng  JSON spec for the TNG WhatsApp backup. Same
                      idea: we expand to a `WhatsApp/` backup dir
                      under the workspace, with root key = 64 zeros
                      (`WHATSAPP_BACKUP_DECRYPTION_KEY` env var
                      set below).
    16: email_mbox    Path to a Google-Takeout-shaped `.mbox` file
                      (e.g. `star_trek.mbox`). The email extractor
                      walks it directly — no synth phase. Account
                      metadata (display name, address, is_personal)
                      is supplied via the source's `mbox:` block in
                      the extract YAML below.
    17: gtk_fx        Google Takeout root dir.
    18: linkedin_fx   LinkedIn data-export dir (CSVs + Articles HTML).
                      File-backed; extract walks `input_path` directly.
    19: sms_fx        "SMS Backup & Restore" export dir (sms-*.xml /
                      calls-*.xml with inline base64 attachments).
                      File-backed; extract walks `input_path` directly.
"""

from __future__ import annotations

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
    sync_bin = Path(sys.argv[1]).resolve()
    signal_make_fixture_bin = Path(sys.argv[2]).resolve()
    whatsapp_make_fixture_bin = Path(sys.argv[3]).resolve()
    now = sys.argv[4]
    data_root = Path(sys.argv[5]).resolve()
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
    ) = (Path(p).resolve() for p in sys.argv[6:20])

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
                # Contacts (carddav) in file mode: no HTTP synth
                # fixture, but extract DOES walk `input_path` for
                # `.vcf` files (just like the email mbox path). The
                # synth pass is a no-op; listed for symmetry.
                "tng_contacts": ("carddav", carddav_fx),
                # Signal also has no HTTP synthesizer (its extract
                # reads a local file tree). Listing it for symmetry —
                # the synth pass writes zero playback fixtures.
                "signal": ("signal_backup", signal_snapshot_root),
                # WhatsApp same pattern: no synthesizer; included so
                # `enabled_sources()` is consistent across phases.
                "whatsapp": ("whatsapp_backup", whatsapp_dir),
                # Email mbox: translate-only-shaped (no `sync:`
                # block → is_managed() false for synth) but the
                # extract phase below DOES walk the mbox into the
                # raw doltlite store. Listed here for symmetry with
                # the other no-synth providers.
                "tng_email": ("email", email_mbox),
                # Google Takeout: file-backed, no HTTP synthesizer;
                # listed for symmetry. Extract walks `input_path` (the
                # Takeout root) in the extract phase below.
                "google-takeout": ("google_takeout", gtk_fx),
                # LinkedIn: file-backed CSV walk. Its ONE HTTP path is the
                # connection-photo fetch (fetch_photos: true below); the
                # synth pass runs LinkedinSynth over the export's
                # Connections.csv to write profile-page + image fixtures.
                "linkedin": ("linkedin", linkedin_fx),
                # SMS Backup & Restore: file-backed, no HTTP synthesizer.
                # Extract walks `input_path` (the export dir) below.
                "sms-backup-restore": ("sms_backup_restore", sms_fx),
            },
            signal_snapshot_root=signal_snapshot_root,
            whatsapp_dir=whatsapp_dir,
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
                # Carddav file mode: extract walks `input_path` for
                # `.vcf` files and lands them in the raw doltlite
                # store using the same row shape CardDAV produces.
                # Translate reads from the raw store — no more
                # divergent "read from disk" path.
                "tng_contacts": ("carddav", carddav_fx),
                # Signal extract walks `snapshot_dir` (set in
                # `_yaml` below), not `input_path`; we still set
                # input_path to the per-source raw subdir so the
                # extractor's doltlite raw store lands there.
                "signal": ("signal_backup", raw_root / "signal"),
                # WhatsApp extract reads `backup_dir` (set in `_yaml`
                # below); input_path is where the `wa_*` mirror lands.
                "whatsapp": ("whatsapp_backup", raw_root / "whatsapp"),
                # Email mbox: extract walks `input_path` directly
                # (the `.mbox` file) and lands a raw doltlite store
                # at `<data_root>/raw/<name>`. `_yaml` omits the
                # `sync:` block for type `email` so is_managed()
                # picks the mbox path, then attaches an `mbox:`
                # block (account_id, display_name, …) so the
                # synthesized `accounts` row matches what JMAP
                # would produce for the same user.
                "tng_email": ("email", email_mbox),
                # Google Takeout: extract walks `input_path` (the Takeout
                # root); the Google Chat feed renders.
                "google-takeout": ("google_takeout", gtk_fx),
                # LinkedIn: extract walks the export dir directly (same
                # file-backed shape as carddav); the message feeds +
                # connections render, and connection photos are fetched
                # via the playback fixtures synthesized above.
                "linkedin": ("linkedin", linkedin_fx),
                # SMS Backup & Restore: extract walks `input_path` (the
                # export dir of sms-*.xml / calls-*.xml).
                "sms-backup-restore": ("sms_backup_restore", sms_fx),
            },
            notion_seed=notion_seed,
            beeper_data_dir=beeper_data_dir,
            signal_snapshot_root=signal_snapshot_root,
            whatsapp_dir=whatsapp_dir,
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
    # Inject the public fixture AEP so the signal extractor can decrypt
    # the snapshot we generated above. Everything else inherits the
    # genrule's env.
    extract_env = {
        **os.environ,
        "SIGNAL_BACKUP_PASSPHRASE": FIXTURE_SIGNAL_AEP,
        "WHATSAPP_BACKUP_DECRYPTION_KEY": FIXTURE_WHATSAPP_KEY,
    }
    extract_argv = [
        str(sync_bin),
        "--config",
        str(extract_yaml),
        "--now",
        now,
        "--playback-root",
        str(playback),
    ]
    # `INGESTED_TNG_RESET=1` is the env-var pass-through used by
    # ingested_tng_test's multi-run case to exercise the
    # --reset-and-redownload code path without changing the positional
    # arg signature.
    if os.environ.get("INGESTED_TNG_RESET") == "1":
        extract_argv.append("--reset-and-redownload")
    _run(extract_argv, env=extract_env)
    return 0


def _yaml(
    data_root: Path,
    sources: dict[str, tuple[str, Path]],
    notion_seed: str | None = None,
    beeper_data_dir: Path | None = None,
    signal_snapshot_root: Path | None = None,
    whatsapp_dir: Path | None = None,
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
        elif type_str == "carddav":
            # File-tree mode: no `sync:` block (otherwise we'd be in
            # CardDAV-server mode). The extract phase walks
            # `input_path` for `.vcf` files; translate reads the
            # raw doltlite store.
            pass
        elif type_str == "signal_backup":
            # The signal extractor needs `snapshot_dir` (where the
            # `signal-backup-*` subdirs live) in addition to the raw
            # store under `input_path`. AEP comes from the
            # SIGNAL_BACKUP_PASSPHRASE env var injected by main().
            if signal_snapshot_root is not None:
                lines.append("    sync:")
                lines.append(f"      snapshot_dir: {signal_snapshot_root}")
            else:
                lines.append("    sync: {}")
        elif type_str == "email":
            # Mbox mode: no `sync:` block (would otherwise trigger
            # the JMAP path). Account metadata is supplied via the
            # `mbox:` block at the source level so the synthesized
            # `accounts` row carries display name + canonical
            # address — same shape JMAP would produce.
            lines.append("    mbox:")
            lines.append("      account_id: picard@enterprise.starfleet")
            lines.append("      display_name: Jean-Luc Picard")
            lines.append("      email_address: picard@enterprise.starfleet")
            lines.append("      is_personal: true")
        elif type_str == "whatsapp_backup":
            # WhatsApp extractor needs `backup_dir` (the dir containing
            # `Databases/msgstore.db.crypt15` + `Media/`). Root key
            # comes from the WHATSAPP_BACKUP_DECRYPTION_KEY env var
            # injected by main().
            if whatsapp_dir is not None:
                lines.append("    sync:")
                lines.append(f"      backup_dir: {whatsapp_dir}")
            else:
                lines.append("    sync: {}")
        elif type_str == "linkedin":
            # File-backed CSV walk (no `sync:` block). Turn on the
            # connection-photo fetch so the pipeline exercises the
            # og:image → CAS path — hermetically, against the playback
            # fixtures LinkedinSynth wrote in the synth phase.
            lines.append("    fetch_photos: true")
        elif type_str == "google_takeout":
            # Opt into the rendering feeds: Google Chat and Google Voice
            # (incl. its Spam folder, to exercise that path). The other
            # feeds stay off for the central pipeline (their extract is
            # covered by the provider's own fixture_walk test).
            lines.append(
                "    sync: {google_chat: true, google_voice: true, "
                "google_voice_include_spam: true}"
            )
        elif type_str == "sms_backup_restore":
            # File-backed, no `sync:` block (the variant has no sync
            # field — deny_unknown_fields would reject `sync: {}`). The
            # extract phase walks `input_path` for `sms-*.xml` /
            # `calls-*.xml`; translate renders one chat per number.
            pass
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


def _run(argv: list[str], env: dict[str, str] | None = None) -> None:
    print("[run_sync_pipeline] $", " ".join(argv), flush=True)
    subprocess.run(argv, check=True, env=env)


if __name__ == "__main__":
    sys.exit(main())
