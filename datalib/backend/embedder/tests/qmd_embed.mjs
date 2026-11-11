// What qmd does with a text, for the embedder's parity test: the string it
// shows the model, the model's tokens for it, and the vector it stores.
//
// argv: <qmd-package-dir> <model-file>; stdin: {docs: [{text, title}],
// queries: [text]}; stdout: {docs: [{formatted, tokens, vector}],
// queries: [...]}. QMD_FORCE_CPU in the environment keeps qmd off the GPU.
//
// Run as a file, not with `node -e`: see qmd_indexer's qmd_sdk.mjs.
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const [pkgDir, modelFile] = process.argv.slice(2);
const llm = await import(pathToFileURL(join(pkgDir, "dist/llm.js")).href);

// The model is the file given, never one qmd fetches; nor does it build
// llama.cpp from source.
llm.setLlamaDirWritableForTest(false);
llm.LlamaCpp.prototype.resolveModel = async (uri) => {
  if (uri !== llm.DEFAULT_EMBED_MODEL_URI) throw new Error(`qmd asked for ${uri}`);
  return modelFile;
};

let input = "";
for await (const chunk of process.stdin) input += chunk;
const { docs, queries } = JSON.parse(input);

const cpp = new llm.LlamaCpp({});
const run = async (formatted) => {
  const embedded = await cpp.embed(formatted);
  if (!embedded) throw new Error(`qmd embedded nothing for ${JSON.stringify(formatted.slice(0, 60))}`);
  return {
    formatted,
    tokens: Array.from(await cpp.tokenize(formatted)),
    vector: embedded.embedding,
  };
};
const out = { docs: [], queries: [] };
for (const { text, title } of docs) {
  out.docs.push(await run(llm.formatDocForEmbedding(text, title)));
}
for (const q of queries) {
  out.queries.push(await run(llm.formatQueryForEmbedding(q)));
}
await cpp.dispose();
process.stdout.write(JSON.stringify(out));
