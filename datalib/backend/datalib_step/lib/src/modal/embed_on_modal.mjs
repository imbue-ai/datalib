// Embeds prompts on Modal's GPUs for `datalib-step`'s `embed` step
// (docs/dev/embed_on_modal.md § "How the step drives Modal"). Run by
// the step on the bundled Node with `--input-type=module --eval`.
//
// Reads `{"hash","prompt"}` lines on stdin and cuts them into slices; runs
// each slice through `embed-prompts` in a Modal Sandbox of its own, a few
// at once. On stdout, as each Sandbox returns: the `{"hash","vector","cut"}`
// lines it brought back; for the prompts of a slice still missing after a
// second Sandbox, one `{"failed":[…],"error":…}`; as each Sandbox ends,
// `{"sandbox_ms":…,"ok":<bool>}`, its life from when Modal placed it to its
// end, what Modal bills; or, when nothing could start, one `{"fatal":…,"sign_in":<bool>}`
// and exit 1. On stderr, what it is doing, never a prompt's text. SIGTERM
// ends every Sandbox it asked for, then the driver: those still waiting for
// a GPU too, found by the tag each is made with, since the SDK names a
// Sandbox only once Modal has placed it.
//
// A slice goes in and its vectors come out as files in the Sandbox, never
// through the Sandbox's stdin and stdout: on a GPU Sandbox, stdout lost most
// of a large answer and still ended cleanly (docs/dev/embed_on_modal.md
// § "How the step drives Modal"). So the Sandbox idles, the slice is written to it,
// `embed-prompts` runs with its stdin and stdout on files, and the answer is
// read once it has exited. Whatever comes back is checked line by line
// against what was sent, and what is missing is sent again once.
//
// A Sandbox that does not answer is not a Sandbox that died: one still
// starting, and a laptop with no network, look the same to the SDK. So a
// call that could not reach one asks Modal whether it is still running and
// calls again if so, and a second Sandbox is made only when the first has
// ended or stayed silent.
//
// DATALIB_MODAL_SDK_ENTRY names Modal's SDK (npm `modal`, from the runtime
// tree) or a test's stand-in; DATALIB_MODAL_OPTIONS holds `image`, `gpu`,
// `command`, `app`, `slice`, `sandboxes`, `timeoutMs`, `pauseMs`, `reachMs`
// and `offlineMs`.

import { randomUUID } from "node:crypto";
import { createInterface } from "node:readline";
import { setTimeout as sleep } from "node:timers/promises";
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

// Every Sandbox this run asks for carries this tag.
const TAGS = { datalib_embed_run: randomUUID().replaceAll("-", "") };

const live = new Set();
process.on("SIGTERM", async () => {
  const ending = new Map([...live].map((sb) => [sb.sandboxId, sb]));
  const placed = ending.size;
  try {
    for await (const sb of client.sandboxes.list({ appId: app.appId, tags: TAGS })) {
      if (!ending.has(sb.sandboxId)) ending.set(sb.sandboxId, sb);
    }
  } catch (e) {
    say(`stopping: could not list the Sandboxes still waiting for a GPU: ${messageOf(e)}`);
  }
  say(`stopping: ending ${placed} Sandbox(es) placed and ${ending.size - placed} not yet`);
  await Promise.all([...ending.values()].map((sb) => sb.terminate().catch(() => {})));
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

const LONGEST_PAUSE_MS = 30_000;

/// Waits longer each time it is called, and answers how long. A wait's
/// nominal length is what the limits below add up: the clock also counts
/// the hours a laptop slept.
function pauses() {
  let next = options.pauseMs;
  return async () => {
    const ms = next;
    next = Math.min(next * 2, LONGEST_PAUSE_MS);
    await sleep(ms);
    return ms;
  };
}

function messageOf(e) {
  return String(e?.message ?? e);
}

/// The gRPC codes the SDK retries: CANCELLED, UNKNOWN, DEADLINE_EXCEEDED,
/// INTERNAL and UNAVAILABLE.
const TRANSIENT = new Set([1, 2, 4, 13, 14]);
function transient(e) {
  return TRANSIENT.has(e?.code);
}

/// A Sandbox, once Modal has placed it: waits out a network that is gone.
async function place(n) {
  const pause = pauses();
  let waited = 0;
  for (;;) {
    try {
      const sb = await client.sandboxes.create(app, image, {
        gpu: options.gpu,
        command: ["sleep", String(Math.ceil(options.timeoutMs / 1000))],
        timeoutMs: options.timeoutMs,
        tags: TAGS,
        // The model is in the image and the prompts arrive as files, so
        // the code reading them is given no network to send them on.
        blockNetwork: true,
      });
      const placed = Date.now();
      return { sb, placed, limit: placed + options.timeoutMs };
    } catch (e) {
      if (!transient(e) || waited >= options.offlineMs) throw e;
      if (waited === 0) {
        say(`slice ${n}: waiting up to ${options.offlineMs / 1000} s for Modal: ${messageOf(e)}`);
      }
      waited += await pause();
    }
  }
}

/// `promise`, given up once the Sandbox is past its limit: the SDK waits on
/// a process for ever.
async function withinLife(box, promise) {
  const given = new AbortController();
  const left = Math.max(0, box.limit - Date.now());
  const over = sleep(left, undefined, { signal: given.signal }).then(() => {
    throw new Error(`${box.sb.sandboxId} is past its limit of ${options.timeoutMs / 1000} s`);
  });
  over.catch(() => {});
  try {
    return await Promise.race([promise, over]);
  } finally {
    given.abort();
  }
}

/// `call` on a Sandbox, made again for as long as Modal says the Sandbox is
/// running or cannot be asked, up to the Sandbox's own limit; `reachMs` of
/// waiting on one Modal says is running ends it sooner.
async function reach(n, box, call) {
  const { sandboxId } = box.sb;
  const pause = pauses();
  let silent = 0;
  let said = false;
  for (;;) {
    try {
      return await withinLife(box, call());
    } catch (e) {
      // The SDK's file calls throw one NotFoundError for every transient
      // code, whether the Sandbox is gone or only not there yet.
      if (!transient(e) && e?.name !== "NotFoundError") throw e;
      const [known, ended] = await withinLife(box, box.sb.poll()).then(
        (code) => [true, code],
        () => [false, null],
      );
      if (known && ended !== null) {
        throw new Error(`${sandboxId} ended with ${ended}: ${messageOf(e)}`);
      }
      if (silent >= options.reachMs) {
        throw new Error(`${sandboxId} is running and does not answer: ${messageOf(e)}`);
      }
      if (Date.now() >= box.limit) throw e;
      if (!said) {
        said = true;
        const state = known ? "is running and does not answer yet" : "cannot be reached, and neither can Modal";
        say(`slice ${n}: ${sandboxId} ${state}, trying it again: ${messageOf(e)}`);
      }
      const ms = await pause();
      if (known) silent += ms;
    }
  }
}

/// Ends a Sandbox, waiting out a network that is gone: one that is not
/// ended is billed up to its limit.
async function end(box) {
  const pause = pauses();
  while (Date.now() < box.limit) {
    try {
      return await box.sb.terminate();
    } catch (e) {
      if (!transient(e)) return;
      await pause();
    }
  }
}

/// What one Sandbox brought back of `lines`, which may be short of them
/// all; a throw when its `embed-prompts` did not finish.
async function runSandbox(n, lines) {
  const box = await place(n);
  const { sb } = box;
  live.add(sb);
  let ok = false;
  try {
    // Until it answers. An `exec` says which code it failed with, which a
    // file call does not.
    await reach(n, box, () => sb.exec(["true"]));
    await reach(n, box, () =>
      sb.filesystem.writeText(lines.map((l) => l.text).join("\n") + "\n", IN),
    );
    // Each try writes a file of its own: one whose answer was lost may be
    // running.
    let out;
    let tries = 0;
    const run = await reach(n, box, () => {
      out = `${WORK}/out-${++tries}.jsonl`;
      return sb.exec([
        "sh",
        "-c",
        `exec "$0" "$@" < ${IN} > ${out} 2> ${ERR}`,
        ...options.command,
      ]);
    });
    const code = await reach(n, box, () => run.wait());
    if (code !== 0) {
      const err = await sb.filesystem.readText(ERR).catch(() => "");
      const tail = err.trim().split("\n").slice(-3).join(" | ");
      throw new Error(`${sb.sandboxId} exited ${code}: ${tail}`);
    }
    const rows = answers(await reach(n, box, () => sb.filesystem.readText(out)), lines);
    ok = rows.length === lines.length;
    const secs = ((Date.now() - box.placed) / 1000).toFixed(1);
    return { rows, sandboxId: sb.sandboxId, secs };
  } finally {
    await end(box);
    live.delete(sb);
    await emit({ sandbox_ms: Math.min(Date.now(), box.limit) - box.placed, ok });
  }
}

/// A slice: its vectors out as each Sandbox brings them, what is missing
/// sent once more, and what is missing after that out as failed.
async function handle(n, lines) {
  let pending = lines;
  let error;
  for (const attempt of [1, 2]) {
    try {
      const { rows, sandboxId, secs } = await runSandbox(n, pending);
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
      error = messageOf(e);
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
// An SDK call given up on may still be trying, which would keep Node alive.
process.stdout.write("", () => process.exit(0));
