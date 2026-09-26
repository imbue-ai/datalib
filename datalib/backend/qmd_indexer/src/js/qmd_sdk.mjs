// Drive the @tobilu/qmd SDK for one operation, reporting as NDJSON on
// stdout — one JSON object per line. Three verbs:
//
//   register <index.yml> <root> [<name> <glob>]...  register collections
//   update <index.yml> <root> [<name> <glob>]...    register, then keyword-index
//   embed [group]                                   embed one collection, or all
//
// Why the SDK and not the CLI: `qmd update` cannot be scoped to one
// collection, and `qmd embed` reports progress only to a terminal
// (docs/dev/qmd_behaviour.md, findings 1 and 11).
//
// `embed` opens the store DB-only (`{ dbPath }`), which reads the
// registry table and never touches `index.yml`. `register` and `update`
// pass the YAML, and registering writes it through: `qmd mcp` reconciles
// the registry against that file when it starts, so the two must agree.
// Both run under the runner's one-slot `qmd_keyword` lock, since each
// write rewrites the whole file.
//
// Run as a file — `node <this.mjs> …` — and not via `node -e`. `-e`
// needs `--input-type=module`, and node hands that flag down to every
// process anything below us forks (it drops the `-e` but keeps the
// `--input-type`). node-llama-cpp probes its prebuilt binary by forking
// exactly such a child, and on linux-x64 that child then fails to
// start, which surfaces as NoBinaryFoundError and no embeddings at all.
//
// argv: <package-dir> <index.sqlite> <verb> <args>...
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const [pkgDir, dbPath, verb, ...args] = process.argv.slice(2);
// Imported by absolute path rather than by package name: the staged
// runtime is not an npm tree this script is inside of. `pathToFileURL`
// is what makes a path containing spaces work.
const load = (rel) => import(pathToFileURL(join(pkgDir, rel)).href);
const emit = (o) => process.stdout.write(`${JSON.stringify(o)}\n`);

const { createStore } = await load("dist/index.js");

// `pairs` alternates a collection's name and its glob. Idempotent: an
// existing collection is updated in place, its documents untouched.
async function addCollections(store, root, pairs) {
  const names = [];
  for (let i = 0; i + 1 < pairs.length; i += 2) {
    await store.addCollection(pairs[i], { path: root, pattern: pairs[i + 1] });
    names.push(pairs[i]);
  }
  return names;
}

async function register(store, root, pairs) {
  const names = await addCollections(store, root, pairs);
  emit({ event: "done", collections: names.length });
}

// A scoped embed over a name the registry lacks matches nothing and
// reports success having done nothing, so an unregistered collection
// is an error rather than an empty pass.
async function requireRegistered(store, group) {
  const names = (await store.listCollections()).map((c) => c.name);
  if (!names.includes(group)) {
    throw new Error(
      `collection ${JSON.stringify(group)} is not registered in this index; ` +
        "its keyword_index step registers it",
    );
  }
}

// Registers first, so a keyword update never depends on the step that
// registers every source having run before it: the runner does not
// hold a step back for an input that is itself still waiting.
async function update(store, root, pairs) {
  const r = await store.update({
    collections: await addCollections(store, root, pairs),
    onProgress: ({ current, total }) => emit({ event: "progress", current, total }),
  });
  emit({
    event: "done",
    indexed: r.indexed,
    updated: r.updated,
    unchanged: r.unchanged,
    removed: r.removed,
    skipped: r.skipped,
  });
}

// `store.embed()` is `generateEmbeddings` minus its `maxDurationMs`, so
// through it every embed stops itself after 30 minutes and returns as if
// it had finished (finding 6). Called directly, `0` turns the cap off.
// Calling `store.embed()` again instead does not work: a document with a
// chunk that always fails loses its other chunks at the end of each pass
// (`removeIncompleteEmbeddings`), so every pass embeds something and the
// loop never ends.
async function embed(store, group) {
  if (group !== undefined) await requireRegistered(store, group);
  const { generateEmbeddings } = await load("dist/store.js");
  const r = await generateEmbeddings(store.internal, {
    collection: group,
    maxDurationMs: 0,
    // `failures` repeats every failed chunk on every callback.
    onProgress: ({ failures: _drop, ...p }) => emit({ event: "progress", ...p }),
  });
  emit({
    event: "done",
    docsProcessed: r.docsProcessed,
    chunksEmbedded: r.chunksEmbedded,
    errors: r.errors,
  });
}

async function run() {
  switch (verb) {
    case "register":
    case "update": {
      const [configPath, root, ...pairs] = args;
      const store = await createStore({ dbPath, configPath });
      try {
        await (verb === "register" ? register : update)(store, root, pairs);
      } finally {
        await store.close();
      }
      return;
    }
    case "embed": {
      const store = await createStore({ dbPath });
      try {
        await embed(store, args[0]);
      } finally {
        await store.close();
      }
      return;
    }
    default:
      throw new Error(`unknown verb ${JSON.stringify(verb)}`);
  }
}

// Not re-exported from `dist/index.js`: the lock belongs to the CLI's
// embed command, so driving the SDK directly means taking it ourselves.
// Two embeds race on vectors_vec's UNIQUE constraint; the runner keeps
// them apart already, and this is what says so if something else did not.
const { tryAcquireEmbedLock, embedLockPathForDb } = await load("dist/cli/embed-lock.js");
const lock = verb === "embed" ? tryAcquireEmbedLock(embedLockPathForDb(dbPath)) : { release() {} };
if (!lock) {
  emit({ event: "busy" });
  process.exitCode = 75;
} else {
  try {
    await run();
  } catch (err) {
    emit({ event: "error", message: err instanceof Error ? err.message : String(err) });
    process.exitCode = 1;
  } finally {
    lock.release();
  }
}
