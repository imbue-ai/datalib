// Embeds prompts on Modal's GPUs for `datalib-step`'s `embed` step
// (docs/dev/plans/embed_on_modal.md § "How the step drives Modal"). Run by
// the step on the bundled Node with `--input-type=module --eval`.
//
// Reads `{"hash","prompt"}` lines on stdin and cuts them into slices; runs
// each slice through `embed-prompts` in a Modal Sandbox of its own, a few
// at once. On stdout, as each Sandbox returns: the `{"hash","vector","cut"}`
// lines it brought back; for the prompts of a slice still missing after a
// second Sandbox, one `{"failed":[…],"error":…}`; as each Sandbox ends,
// `{"sandbox_ms":…,"ok":<bool>}`, its life from creation to end, what Modal
// bills; or, when nothing could start, one `{"fatal":…,"sign_in":<bool>}`
// and exit 1. On stderr, what it is doing, never a prompt's text. SIGTERM
// ends every Sandbox it started, then the driver.
//
// A slice goes in and its vectors come out as files in the Sandbox, never
// through the Sandbox's stdin and stdout: on a GPU Sandbox, stdout lost most
// of a large answer and still ended cleanly (docs/dev/plans/embed_on_modal.md
// § "Measured on Modal"). So the Sandbox idles, the slice is written to it,
// `embed-prompts` runs with its stdin and stdout on files, and the answer is
// read once it has exited. Whatever comes back is checked line by line
// against what was sent, and what is missing is sent again once.
//
// DATALIB_MODAL_SDK_ENTRY names Modal's SDK (npm `modal`, from the runtime
// tree) or a test's stand-in; DATALIB_MODAL_OPTIONS holds `image`, `gpu`,
// `command`, `app`, `slice`, `sandboxes` and `timeoutMs`.

import { createInterface } from "node:readline";
import { pathToFileURL } from "node:url";

const options = JSON.parse(process.env.DATALIB_MODAL_OPTIONS);
const sdkEntry = process.env.DATALIB_MODAL_SDK_ENTRY;

function say(line) {
  process.stderr.write(`[modal] ${line}\n`);
}

// Every slice writing while stdout is full waits on this one drain, not
// on a listener of its own: past ten, Node warns on every write.
let draining = null;
async function emit(obj) {
  while (draining) await draining;
  if (!process.stdout.write(JSON.stringify(obj) + "\n")) {
    draining = new Promise((resolve) =>
      process.stdout.once("drain", () => {
        draining = null;
        resolve();
      }),
    );
    await draining;
  }
}

async function fatal(message, signIn) {
  await emit({ fatal: message, sign_in: signIn });
  process.exit(1);
}

const live = new Set();
process.on("SIGTERM", async () => {
  say(`stopping: ending ${live.size} Sandbox(es)`);
  await Promise.all([...live].map((sb) => sb.terminate().catch(() => {})));
  process.exit(143);
});

let sdk;
try {
  sdk = await import(pathToFileURL(sdkEntry).href);
} catch (e) {
  await fatal(`could not load Modal's SDK from ${sdkEntry}: ${e?.message ?? e}`, false);
}

let client;
let app;
let image;
try {
  client = new sdk.ModalClient();
  app = await client.apps.fromName(options.app, { createIfMissing: true });
  image = client.images.fromRegistry(options.image);
} catch (e) {
  const message = String(e?.message ?? e);
  await fatal(message, /credential|token|unauthenticated|not authorized/i.test(message));
}

const WORK = "/tmp/datalib-embed";
const IN = `${WORK}/in.jsonl`;
const OUT = `${WORK}/out.jsonl`;
const ERR = `${WORK}/err.log`;

/// The rows of `out` that answer a prompt of `lines`, each hash once.
function answers(out, lines) {
  const wanted = new Set(lines.map((l) => l.hash));
  const rows = [];
  for (const text of out.split("\n")) {
    if (text.length === 0) continue;
    let row;
    try {
      row = JSON.parse(text);
    } catch {
      continue;
    }
    if (wanted.delete(row?.hash) && Array.isArray(row.vector)) rows.push(row);
  }
  return rows;
}

/// What one Sandbox brought back of `lines`, which may be short of them
/// all; a throw when its `embed-prompts` did not finish.
async function runSandbox(lines) {
  const started = Date.now();
  const sb = await client.sandboxes.create(app, image, {
    gpu: options.gpu,
    command: ["sleep", String(Math.ceil(options.timeoutMs / 1000))],
    timeoutMs: options.timeoutMs,
  });
  live.add(sb);
  let ok = false;
  try {
    await sb.filesystem.writeText(lines.map((l) => l.text).join("\n") + "\n", IN);
    const run = await sb.exec([
      "sh",
      "-c",
      `exec "$0" "$@" < ${IN} > ${OUT} 2> ${ERR}`,
      ...options.command,
    ]);
    const code = await run.wait();
    if (code !== 0) {
      const err = await sb.filesystem.readText(ERR).catch(() => "");
      const tail = err.trim().split("\n").slice(-3).join(" | ");
      throw new Error(`${sb.sandboxId} exited ${code}: ${tail}`);
    }
    const rows = answers(await sb.filesystem.readText(OUT), lines);
    ok = rows.length === lines.length;
    return { rows, sandboxId: sb.sandboxId, secs: ((Date.now() - started) / 1000).toFixed(1) };
  } finally {
    live.delete(sb);
    await sb.terminate().catch(() => {});
    await emit({ sandbox_ms: Date.now() - started, ok });
  }
}

/// A slice: its vectors out as each Sandbox brings them, what is missing
/// sent once more, and what is missing after that out as failed.
async function handle(n, lines) {
  let pending = lines;
  let error;
  for (const attempt of [1, 2]) {
    try {
      const { rows, sandboxId, secs } = await runSandbox(pending);
      for (const row of rows) {
        await emit({ hash: row.hash, vector: row.vector, cut: row.cut });
      }
      const came = new Set(rows.map((r) => r.hash));
      const sent = pending.length;
      pending = pending.filter((l) => !came.has(l.hash));
      say(`slice ${n}: ${rows.length} of ${sent} prompts on ${options.gpu} in ${secs} s (${sandboxId})`);
      if (pending.length === 0) return;
      error = `${sandboxId} returned ${rows.length} vectors for ${sent} prompts`;
    } catch (e) {
      error = String(e?.message ?? e);
      say(`slice ${n}, attempt ${attempt}: ${error}`);
    }
  }
  await emit({ failed: pending.map((l) => l.hash), error });
}

const inflight = new Set();
let slices = 0;
async function launch(lines) {
  while (inflight.size >= options.sandboxes) {
    await Promise.race(inflight);
  }
  const n = ++slices;
  const p = handle(n, lines).finally(() => inflight.delete(p));
  inflight.add(p);
}

let slice = [];
for await (const text of createInterface({ input: process.stdin, crlfDelay: Infinity })) {
  if (text.length === 0) continue;
  slice.push({ text, hash: JSON.parse(text).hash });
  if (slice.length >= options.slice) {
    await launch(slice);
    slice = [];
  }
}
if (slice.length > 0) await launch(slice);
await Promise.all(inflight);
say(`${slices} slice(s) done`);
