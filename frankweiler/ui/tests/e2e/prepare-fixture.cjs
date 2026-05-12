#!/usr/bin/env node
// Materialize a frankweiler data root from the bazel-built ingested fixture.
//
// The bazel target //tests/fixtures:ingested_tng emits two byte-stable files:
//   bazel-bin/tests/fixtures/ingested/dump.sql   -- portable SQL dump
//   bazel-bin/tests/fixtures/ingested/qmd.tar    -- rendered conversation tree
//
// Backend layout expected at <root>:
//   <root>/mirror.sqlite
//   <root>/anthropic/<account>/llm_chats/*.qmd
//   <root>/openai/<account>/llm_chats/*.qmd
//
// The tar archive's entries are prefixed with `qmd/`, matching the directory
// the genrule writes into, so we extract to <root>/.. and the qmd/ tree lands
// alongside mirror.sqlite under <root>/qmd/. The backend's qmd::scan_root,
// however, expects <root>/{anthropic,openai} directly. To bridge that, we
// extract with `--strip-components=1` so the inner anthropic/ and openai/
// directories sit at <root>/.
//
// Usage:
//   node prepare-fixture.cjs <out-root>
//
// Resolves the bazel-bin paths from a checked-in well-known location relative
// to the workspace root.

const fs = require("node:fs");
const path = require("node:path");
const { execFileSync, spawnSync } = require("node:child_process");

function findWorkspaceRoot(start) {
  let dir = start;
  while (dir !== path.dirname(dir)) {
    if (fs.existsSync(path.join(dir, "MODULE.bazel"))) return dir;
    dir = path.dirname(dir);
  }
  throw new Error(`could not locate MODULE.bazel above ${start}`);
}

function ensureFixtureBuilt(workspace) {
  const dump = path.join(workspace, "bazel-bin/tests/fixtures/ingested/dump.sql");
  const tar = path.join(workspace, "bazel-bin/tests/fixtures/ingested/qmd.tar");
  const qmdIndex = path.join(
    workspace,
    "bazel-bin/tests/fixtures/ingested/qmd-index.tar",
  );
  if (fs.existsSync(dump) && fs.existsSync(tar) && fs.existsSync(qmdIndex)) {
    return { dump, tar, qmdIndex };
  }
  // eslint-disable-next-line no-console
  console.error(
    "[prepare-fixture] building //tests/fixtures:ingested_tng and :ingested_tng_qmd…",
  );
  const r = spawnSync(
    "bazelisk",
    [
      "build",
      "//tests/fixtures:ingested_tng",
      "//tests/fixtures:ingested_tng_qmd",
    ],
    {
      cwd: workspace,
      stdio: "inherit",
    },
  );
  if (r.status !== 0) {
    throw new Error(
      "bazelisk build //tests/fixtures:ingested_tng[_qmd] failed",
    );
  }
  return { dump, tar, qmdIndex };
}

function loadDumpIntoSqlite(dumpPath, sqlitePath) {
  // Use python3 — the dump is the SQL subset accepted by sqlite, and
  // src/ingest/sqlite_load.py demonstrates this works via executescript.
  const script = `
import sqlite3, sys, pathlib
dump = pathlib.Path(sys.argv[1]).read_text()
out = sys.argv[2]
pathlib.Path(out).unlink(missing_ok=True)
conn = sqlite3.connect(out)
conn.executescript(dump)
conn.commit()
conn.close()
`;
  execFileSync("python3", ["-c", script, dumpPath, sqlitePath], {
    stdio: "inherit",
  });
}

function main() {
  const outRoot = process.argv[2];
  if (!outRoot) {
    // eslint-disable-next-line no-console
    console.error("usage: node prepare-fixture.cjs <out-root>");
    process.exit(2);
  }
  const workspace = findWorkspaceRoot(__dirname);
  const { dump, tar, qmdIndex } = ensureFixtureBuilt(workspace);

  fs.rmSync(outRoot, { recursive: true, force: true });
  fs.mkdirSync(outRoot, { recursive: true });

  // Extract qmd.tar with the leading `qmd/` stripped so <root>/anthropic/...
  // lands where qmd::scan_root looks.
  execFileSync("tar", ["-xf", tar, "-C", outRoot, "--strip-components=1"], {
    stdio: "inherit",
  });
  // Overlay qmd-index.tar with the same strip — its archive paths are
  // also rooted at `qmd/`, so the index file lands at
  // <root>/.frankweiler/qmd/index.sqlite, exactly where the backend
  // expects it (frankweiler_core::qmd::QMD_INDEX_REL).
  execFileSync(
    "tar",
    ["-xf", qmdIndex, "-C", outRoot, "--strip-components=1"],
    { stdio: "inherit" },
  );

  loadDumpIntoSqlite(dump, path.join(outRoot, "mirror.sqlite"));
  // Backend reads root via FRANKWEILER_ROOT env var (set by playwright config).

  // Recreate the `models` symlink that `tests/fixtures/build_qmd_index.py`
  // strips from qmd-index.tar, and assert the shared cache is already
  // populated. The intent of the symlink scheme (see
  // frankweiler-qmd-indexer main.rs) is that the ~1.6 GB of qmd model
  // files live once in ~/.cache/qmd-models and every data root just
  // symlinks them in. If the cache is empty here, qmd's first call
  // would silently redownload — a "passes after 30+s" near-failure.
  // Fail fast and loud instead.
  const sharedModels = path.join(
    process.env.HOME || ".",
    ".cache",
    "qmd-models",
  );
  const REQUIRED_MODELS = [
    "hf_ggml-org_embeddinggemma-300M-Q8_0.gguf",
    "hf_tobil_qmd-query-expansion-1.7B-q4_k_m.gguf",
  ];
  const missing = REQUIRED_MODELS.filter((name) => {
    const p = path.join(sharedModels, name);
    try {
      return fs.statSync(p).size === 0;
    } catch {
      return true;
    }
  });
  if (missing.length > 0) {
    // eslint-disable-next-line no-console
    console.error(
      `[prepare-fixture] missing qmd models in ${sharedModels}:\n` +
        missing.map((m) => `  - ${m}`).join("\n") +
        `\n\nPopulate the shared cache once by running the qmd indexer ` +
        `against any data root, e.g.:\n` +
        `  bazelisk run //frankweiler/backend/qmd_indexer -- --root <some-frankweiler-root>\n` +
        `or let qmd download them by invoking the CLI directly. The e2e ` +
        `suite refuses to trigger this download itself — that path is a ` +
        `silent multi-minute stall.`,
    );
    process.exit(3);
  }
  const modelsLink = path.join(outRoot, ".frankweiler", "qmd", "models");
  fs.mkdirSync(path.dirname(modelsLink), { recursive: true });
  try {
    fs.symlinkSync(sharedModels, modelsLink);
  } catch (e) {
    if (e.code !== "EEXIST") throw e;
  }

  // eslint-disable-next-line no-console
  console.log(outRoot);
}

main();
