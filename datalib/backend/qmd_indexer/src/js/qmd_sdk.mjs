// Drive the @tobilu/qmd SDK for one operation, reporting as NDJSON on
// stdout — one JSON object per line: `progress` any number of times, then
// one of `done`, `error` or `busy`.
//
// argv: <package-dir> <index.sqlite> <verb> <json>, where <json> is:
//
//   register  {config, root, collections: [{name, glob}]}  exactly these, no others
//   update    {config, root, collections: [{name, glob}]}  registers them first
//   embed     {collections: [name]}                        in one process: one model load
//   status    {}
//
// Why the SDK and not the CLI: `qmd update` cannot be scoped to one
// collection, and `qmd embed` reports progress only to a terminal
// (docs/dev/qmd_behaviour.md, findings 1 and 11).
//
// `register` and `update` open the store with `index.yml` (`config`),
// and registering writes it through: `qmd mcp` reconciles the registry
// against that file when it starts, so the two must agree. Both run
// under the runner's one-slot `qmd_keyword` lock, since each write
// rewrites the whole file. `embed` and `status` open the store DB-only,
// which reads the registry and never touches the file.
//
// Run as a file — `node <this.mjs> …` — and not via `node -e`. `-e`
// needs `--input-type=module`, and node hands that flag down to every
// process anything below us forks (it drops the `-e` but keeps the
// `--input-type`). node-llama-cpp probes its prebuilt binary by forking
// exactly such a child, and on linux-x64 that child then fails to
// start, which surfaces as NoBinaryFoundError and no embeddings at all.
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const [pkgDir, dbPath, verb, json] = process.argv.slice(2);
const args = JSON.parse(json ?? "{}");
// Imported by absolute path rather than by package name: the runtime is
// not an npm tree this script is inside of. `pathToFileURL` is what makes
// a path containing spaces work.
const load = (rel) => import(pathToFileURL(join(pkgDir, rel)).href);
const emit = (o) => process.stdout.write(`${JSON.stringify(o)}\n`);

const { createStore } = await load("dist/index.js");
// Not exported from `dist/index.js`, and needed: `generateEmbeddings`
// takes the time cap `store.embed()` drops, `removeCollection` deletes a
// collection's documents where `store.removeCollection()` only
// unregisters it, and `getHashesNeedingEmbedding` is qmd's own count of
// what is left to embed.
const { generateEmbeddings, removeCollection, getHashesNeedingEmbedding } =
  await load("dist/store.js");

async function addCollections(store, root, collections) {
  for (const { name, glob } of collections) {
    await store.addCollection(name, { path: root, pattern: glob });
  }
  return collections.map((c) => c.name);
}

// Leaves the registry naming exactly `collections`. Any other collection
// is retired with its documents: unregistered ones would still be
// searched.
async function register(store, { root, collections }) {
  const keep = new Set(await addCollections(store, root, collections));
  const retired = (await store.listCollections()).map((c) => c.name).filter((n) => !keep.has(n));
  // After registering, not before: removing a collection also deletes
  // every body no remaining document names, so the others must already
  // claim theirs.
  for (const name of retired) {
    await store.removeCollection(name);
    removeCollection(store.internal.db, name);
  }
  emit({ event: "done", retired });
}

// Registers first, so a keyword update never depends on the step that
// registers every source having run before it: the runner does not hold
// a step back for an input that is itself still waiting.
async function update(store, { root, collections }) {
  const r = await store.update({
    collections: await addCollections(store, root, collections),
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

// A scoped embed over a name the registry lacks matches nothing and
// reports success having done nothing, so it is an error instead.
async function requireRegistered(store, name) {
  if (!(await store.listCollections()).some((c) => c.name === name)) {
    throw new Error(
      `collection ${JSON.stringify(name)} is not registered in this index; ` +
        "its keyword_index step registers it",
    );
  }
}

// qmd's embed progress counts bytes and chunks, never documents, so the
// documents done are its own count of what is left, taken again as the
// run goes. That query scans every vector in the index, so it is re-asked
// only once the run has gone 20 times as long as the last one took — a
// twentieth of the run at most, however large the index.
function documentCounter(store, collection) {
  const model = store.internal.llm?.embedModelName;
  const left = () => getHashesNeedingEmbedding(store.internal.db, collection, model);
  const totalDocs = left();
  let docsEmbedded = 0;
  let nextAt = 0;
  return {
    read({ fresh = false } = {}) {
      const start = Date.now();
      if (fresh || start >= nextAt) {
        docsEmbedded = Math.max(0, totalDocs - left());
        nextAt = Date.now() + 20 * (Date.now() - start);
      }
      return { docsEmbedded, totalDocs };
    },
  };
}

// `store.embed()` is `generateEmbeddings` minus its `maxDurationMs`, so
// through it every embed stops itself after 30 minutes and returns as if
// it had finished (finding 6); `0` turns the cap off. Calling
// `store.embed()` again instead does not work: a document with a chunk
// that always fails loses its other chunks at the end of each pass
// (`removeIncompleteEmbeddings`), so every pass embeds something and the
// loop never ends.
async function embed(store, { collections }) {
  for (const collection of collections) await requireRegistered(store, collection);
  const total = { event: "done", documents: 0, chunks: 0, errors: 0 };
  for (const collection of collections) {
    const docs = documentCounter(store, collection);
    let qmd = {};
    const report = (read) => emit({ event: "progress", collection, ...qmd, ...read });
    report(docs.read());
    const r = await generateEmbeddings(store.internal, {
      collection,
      maxDurationMs: 0,
      // `failures` repeats every failed chunk on every callback.
      onProgress: ({ failures: _drop, ...p }) => {
        qmd = p;
        report(docs.read());
      },
    });
    report(docs.read({ fresh: true }));
    total.documents += r.docsProcessed;
    total.chunks += r.chunksEmbedded;
    total.errors += r.errors;
  }
  emit(total);
}

async function status(store) {
  const model = store.internal.llm?.embedModelName;
  const collections = (await store.listCollections()).map((c) => ({
    name: c.name,
    documents: c.active_count,
    needsEmbedding: getHashesNeedingEmbedding(store.internal.db, c.name, model),
  }));
  emit({ event: "done", collections });
}

const VERBS = {
  register: { run: register, withConfig: true },
  update: { run: update, withConfig: true },
  embed: { run: embed, withConfig: false },
  status: { run: status, withConfig: false },
};

async function run() {
  const v = VERBS[verb];
  if (!v) throw new Error(`unknown verb ${JSON.stringify(verb)}`);
  const store = await createStore(v.withConfig ? { dbPath, configPath: args.config } : { dbPath });
  try {
    await v.run(store, args);
  } finally {
    await store.close();
  }
}

// Not exported from `dist/index.js`: the lock belongs to the CLI's embed
// command, so driving the SDK directly means taking it ourselves. Two
// embeds race on vectors_vec's UNIQUE constraint; the runner keeps them
// apart already, and this is what says so if something else did not.
const { tryAcquireEmbedLock, embedLockPathForDb } = await load("dist/cli/embed-lock.js");
const lock = verb === "embed" ? tryAcquireEmbedLock(embedLockPathForDb(dbPath)) : { release() {} };
if (!lock) {
  emit({ event: "busy" });
  process.exitCode = 75;
} else {
  // Stopping a step signals its whole process group, and node's default
  // for either signal exits without reaching `finally`, which left the
  // lock file behind.
  for (const signal of ["SIGINT", "SIGTERM"]) {
    process.on(signal, () => {
      lock.release();
      process.exit(130);
    });
  }
  try {
    await run();
  } catch (err) {
    emit({ event: "error", message: err instanceof Error ? err.message : String(err) });
    process.exitCode = 1;
  } finally {
    lock.release();
  }
}
