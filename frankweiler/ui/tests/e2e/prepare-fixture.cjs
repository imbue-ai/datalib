#!/usr/bin/env node
// Materialize a frankweiler data root from the bazel-built ingested fixture.
//
// The bazel target //tests/fixtures:ingested_tng emits two byte-stable files:
//   bazel-bin/tests/fixtures/ingested/dump.sql   -- portable SQL dump
//   bazel-bin/tests/fixtures/ingested/qmd.tar    -- rendered conversation tree
//
// Backend layout expected at <root>:
//   <root>/dolt_db/           (initialized + dump loaded)
//   <root>/config.yaml        (ephemeral dolt port; backend reads via
//                              FRANKWEILER_CONFIG)
//   <root>/rendered_md/<provider>/...
//   <root>/qmd/index.sqlite
//
// The tar archive's entries are prefixed with `qmd/`, matching the directory
// the genrule writes into. We extract with `--strip-components=1` so
// `rendered_md/`, `qmd/`, etc. sit at <root>/.
//
// Usage:
//   node prepare-fixture.cjs <out-root>
//
// Resolves the bazel-bin paths from a checked-in well-known location relative
// to the workspace root.

const fs = require("node:fs");
const net = require("node:net");
const path = require("node:path");
const { execFileSync, execSync, spawnSync } = require("node:child_process");

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

function freePortSync() {
  // Synchronous ephemeral-port grab via a brief listen on :0.
  // Spawn node to keep this routine fully sync (we can't await here).
  const out = execFileSync("node", [
    "-e",
    "const s=require('net').createServer();s.listen(0,'127.0.0.1',()=>{process.stdout.write(String(s.address().port));s.close()});",
  ]).toString();
  return Number.parseInt(out, 10);
}

function loadDumpIntoDolt(dumpPath, repoDir) {
  // `dolt init` requires an identity even for throwaway repos. The dump
  // is a portable MySQL dialect (backticks, `LOCK TABLES`-free, no
  // dolt-only DDL) that `dolt sql` ingests in offline mode without
  // needing the sql-server up. The repo dir name becomes the database
  // name, hence the `USE dolt_repo` prepend.
  fs.mkdirSync(repoDir, { recursive: true });
  execFileSync(
    "dolt",
    [
      "init",
      "--name",
      "Frankweiler e2e",
      "--email",
      "e2e@frankweiler.local",
    ],
    { cwd: repoDir, stdio: "inherit" },
  );
  const dump = fs.readFileSync(dumpPath, "utf8");
  execSync("dolt sql", {
    cwd: repoDir,
    input: `USE dolt_db;\n${dump}`,
    stdio: ["pipe", "inherit", "inherit"],
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
  // <root>/qmd/index.sqlite, exactly where the backend expects it
  // (frankweiler_core::qmd::QMD_INDEX_REL).
  execFileSync(
    "tar",
    ["-xf", qmdIndex, "-C", outRoot, "--strip-components=1"],
    { stdio: "inherit" },
  );

  const doltPort = freePortSync();
  loadDumpIntoDolt(dump, path.join(outRoot, "dolt_db"));
  // Ephemeral dolt port avoids 3306 collisions when multiple bazel test
  // shards (or a host-side dev dolt) run concurrently. The backend reads
  // this via FRANKWEILER_CONFIG (set by playwright.config.ts).
  fs.writeFileSync(
    path.join(outRoot, "config.yaml"),
    `data_root: ${outRoot}\ndolt:\n  port: ${doltPort}\n`,
  );

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
  const modelsLink = path.join(outRoot, "qmd", "models");
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
