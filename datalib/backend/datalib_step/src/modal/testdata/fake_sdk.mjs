// A stand-in for Modal's JavaScript SDK (npm `modal`), for the `embed`
// step's tests: the part the driver uses (`src/modal/embed_on_modal.mjs`),
// with each Sandbox a local process running the command it was given, and
// a folder of its own standing in for its filesystem: a path the driver
// names under `/tmp/datalib-embed` lands in that folder, in file calls and
// in an `exec`'s arguments alike.
//
//   FAKE_MODAL_LOG        a file that gets one JSON line per Sandbox made:
//                         its image, gpu, cpu, memory, command and the
//                         lines it was sent
//   FAKE_MODAL_SIGNED_OUT `apps.fromName` throws as the SDK does with no
//                         credentials
//   FAKE_MODAL_FAIL       how many Sandboxes, the first ones made, fail:
//                         their command exits 1
//   FAKE_MODAL_SHORT      how many Sandboxes, the first ones made, hand
//                         back only the first half of their output (rounded
//                         up), however it is read: what a GPU Sandbox's
//                         stdout did on Modal, with no error

import { spawn } from "node:child_process";
import {
  appendFileSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

const log = process.env.FAKE_MODAL_LOG;
const REMOTE = "/tmp/datalib-embed";

function nextSandbox() {
  // Counted in a file beside the log, so a test can fail the first ones.
  const counter = `${log}.count`;
  let n = 0;
  try {
    n = Number(readFileSync(counter, "utf8"));
  } catch {}
  writeFileSync(counter, String(n + 1));
  return n + 1;
}

/// The first half of `text`'s lines, rounded up.
function halved(text) {
  const lines = text.split("\n").filter((l) => l);
  return lines.slice(0, Math.ceil(lines.length / 2)).map((l) => l + "\n").join("");
}

/// The part of a stream the driver uses.
function readable(stream, short) {
  stream.setEncoding("utf8");
  let text = "";
  stream.on("data", (chunk) => (text += chunk));
  const ended = new Promise((resolve) => stream.on("end", resolve));
  return { readText: async () => (await ended, short ? halved(text) : text) };
}

function run(argv, stdio) {
  const [program, ...args] = argv;
  const child = spawn(program, args, { stdio });
  const exit = new Promise((resolve) => child.on("close", (code) => resolve(code ?? 1)));
  return { child, exit };
}

class Sandbox {
  constructor(n, params) {
    this.sandboxId = `sb-fake-${n}`;
    this.fail = n <= Number(process.env.FAKE_MODAL_FAIL ?? 0);
    this.short = n <= Number(process.env.FAKE_MODAL_SHORT ?? 0);
    this.dir = mkdtempSync(join(tmpdir(), "fake-modal-sb-"));
    this.lines = 0;
    const local = (path) => {
      if (!path.startsWith(REMOTE)) throw new Error(`the stand-in has no ${path}`);
      return join(this.dir, path.slice(REMOTE.length));
    };
    const { child, exit } = run(
      this.fail ? [process.execPath, "-e", "process.exit(1)"] : params.command,
      ["pipe", "pipe", "pipe"],
    );
    child.stdin.on("error", () => {});
    this.child = child;
    this.exit = exit;
    this.stdin = {
      writeText: async (text) => {
        this.lines += text.split("\n").filter((l) => l).length;
        await new Promise((resolve) => child.stdin.write(text, resolve));
      },
      close: async () => {
        await new Promise((resolve) => child.stdin.end(resolve));
      },
    };
    this.stdout = readable(child.stdout, this.short);
    this.stderr = readable(child.stderr, false);
    this.filesystem = {
      writeText: async (text, path) => {
        this.lines += text.split("\n").filter((l) => l).length;
        mkdirSync(dirname(local(path)), { recursive: true });
        writeFileSync(local(path), text);
      },
      readText: async (path) => {
        const text = readFileSync(local(path), "utf8");
        return this.short ? halved(text) : text;
      },
    };
    this.execs = [];
  }
  async exec(command) {
    const argv = this.fail
      ? [process.execPath, "-e", "process.exit(1)"]
      : command.map((a) => a.replaceAll(REMOTE, this.dir));
    const { child, exit } = run(argv, ["ignore", "pipe", "pipe"]);
    this.execs.push(command);
    return {
      stdout: readable(child.stdout, false),
      stderr: readable(child.stderr, false),
      wait: async () => exit,
    };
  }
  async wait() {
    return this.exit;
  }
  async terminate() {
    this.child.kill();
  }
}

export class ModalClient {
  constructor() {
    this.apps = {
      fromName: async (name) => {
        if (process.env.FAKE_MODAL_SIGNED_OUT) {
          throw new Error(
            "Profile is missing credentials. Please set them in .modal.toml, as environment variables, or via the ModalClient constructor.",
          );
        }
        return { name };
      },
    };
    this.images = { fromRegistry: (tag) => ({ tag }) };
    this.sandboxes = {
      create: async (app, image, params) => {
        const n = nextSandbox();
        const sb = new Sandbox(n, params);
        sb.exit.then(() => {
          mkdirSync(dirname(log), { recursive: true });
          appendFileSync(
            log,
            JSON.stringify({
              app: app.name,
              image: image.tag,
              gpu: params.gpu,
              cpu: params.cpu,
              memoryMiB: params.memoryMiB,
              command: params.command,
              execs: sb.execs,
              lines: sb.lines,
            }) + "\n",
          );
        });
        return sb;
      },
    };
  }
}
