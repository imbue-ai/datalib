// Run qmd's embedding pass through the @tobilu/qmd SDK, reporting
// progress as NDJSON on stdout — one JSON object per line.
//
// Why not `qmd embed`: the CLI has this same progress, and writes it
// only when `process.stderr.isTTY` (dist/cli/qmd.js), as a `\r`-redrawn
// bar. Under a pipe it emits nothing at all for the length of the run.
// `store.embed({ onProgress })` is the same numbers before they reach
// that gate.
//
// Run by `node --input-type=module -e <this>`, so the arguments start
// at argv[1] — there is no script path in front of them.
//
// argv: <package-dir> <index.sqlite> [index.yml]
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const [pkgDir, dbPath, configPath] = process.argv.slice(1);
// Imported by absolute path rather than by package name: the staged
// runtime is not an npm tree this script is inside of. `pathToFileURL`
// is what makes a path containing spaces work.
const load = (rel) => import(pathToFileURL(join(pkgDir, rel)).href);
const emit = (o) => process.stdout.write(`${JSON.stringify(o)}\n`);

const { createStore } = await load("dist/index.js");
// Not re-exported from `dist/index.js`: the lock belongs to the CLI's
// embed command, so driving the SDK directly means taking it ourselves.
// Without it, two embeds race on vectors_vec's UNIQUE constraint.
const { tryAcquireEmbedLock, embedLockPathForDb } = await load("dist/cli/embed-lock.js");

const lock = tryAcquireEmbedLock(embedLockPathForDb(dbPath));
if (!lock) {
  emit({ event: "busy" });
  process.exitCode = 75;
} else {
  let store;
  try {
    store = await createStore(configPath ? { dbPath, configPath } : { dbPath });
    const result = await store.embed({
      // `failures` repeats every failed chunk on every callback; the
      // terminal event carries the list once.
      onProgress: ({ failures: _drop, ...p }) => emit({ event: "progress", ...p }),
    });
    emit({ event: "done", ...result, failures: result.failures ?? [] });
  } catch (err) {
    emit({ event: "error", message: err instanceof Error ? err.message : String(err) });
    process.exitCode = 1;
  } finally {
    if (store) await store.close();
    lock.release();
  }
}
