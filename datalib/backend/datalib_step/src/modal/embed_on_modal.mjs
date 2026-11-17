// Embeds prompts on Modal's GPUs for `datalib-step`'s `embed` step
// (docs/dev/plans/embed_on_modal.md § "How the step drives Modal"). Run by
// the step on the bundled Node with `--input-type=module --eval`.
//
// Reads `{"hash","prompt"}` lines on stdin and cuts them into slices; runs
// each slice through `embed-prompts` in a Modal Sandbox of its own, a few
// at once. On stdout, as each slice returns: its `{"hash","vector","cut"}`
// lines, or for a slice that failed twice one `{"failed":[…],"error":…}`;
// or, when nothing could start, one `{"fatal":…,"sign_in":<bool>}` and
// exit 1. On stderr, what it is doing, never a prompt's text. SIGTERM ends
// every Sandbox it started, then the driver.
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

async function emit(obj) {
  if (!process.stdout.write(JSON.stringify(obj) + "\n")) {
    await new Promise((resolve) => process.stdout.once("drain", resolve));
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

/// One slice's replies, or a throw saying why there are none.
async function runSlice(n, lines) {
  const started = Date.now();
  const sb = await client.sandboxes.create(app, image, {
    gpu: options.gpu,
    command: options.command,
    timeoutMs: options.timeoutMs,
  });
  live.add(sb);
  try {
    const stdout = sb.stdout.readText();
    const stderr = sb.stderr.readText();
    await sb.stdin.writeText(lines.map((l) => l.text).join("\n") + "\n");
    await sb.stdin.close();
    const [out, err, code] = await Promise.all([stdout, stderr, sb.wait()]);
    if (code !== 0) {
      const tail = err.trim().split("\n").slice(-3).join(" | ");
      throw new Error(`${sb.sandboxId} exited ${code}: ${tail}`);
    }
    const rows = out
      .split("\n")
      .filter((l) => l.length > 0)
      .map((l) => JSON.parse(l));
    const got = new Set(rows.map((r) => r.hash));
    if (rows.length !== lines.length || !lines.every((l) => got.has(l.hash))) {
      throw new Error(`${sb.sandboxId} returned ${rows.length} vectors for ${lines.length} prompts`);
    }
    const secs = ((Date.now() - started) / 1000).toFixed(1);
    say(`slice ${n}: ${rows.length} prompts on ${options.gpu} in ${secs} s (${sb.sandboxId})`);
    return rows;
  } finally {
    live.delete(sb);
    await sb.terminate().catch(() => {});
  }
}

/// A slice, sent again once if it fails; then its vectors or its failure, out.
async function handle(n, lines) {
  let rows;
  let error;
  for (const attempt of [1, 2]) {
    try {
      rows = await runSlice(n, lines);
      break;
    } catch (e) {
      error = String(e?.message ?? e);
      say(`slice ${n}, attempt ${attempt}: ${error}`);
    }
  }
  if (rows) {
    for (const row of rows) {
      await emit({ hash: row.hash, vector: row.vector, cut: row.cut });
    }
  } else {
    await emit({ failed: lines.map((l) => l.hash), error });
  }
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
