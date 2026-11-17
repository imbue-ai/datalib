// A stand-in for Modal's JavaScript SDK (npm `modal`), for the `embed`
// step's tests: the part the driver uses (`src/modal/embed_on_modal.mjs`),
// with each Sandbox a local process running the command it was given.
//
//   FAKE_MODAL_LOG        a file that gets one JSON line per Sandbox made:
//                         its image, gpu, command and the lines it was sent
//   FAKE_MODAL_SIGNED_OUT `apps.fromName` throws as the SDK does with no
//                         credentials
//   FAKE_MODAL_FAIL       how many Sandboxes, the first ones made, exit 1

import { spawn } from "node:child_process";
import { appendFileSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";

const log = process.env.FAKE_MODAL_LOG;

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

/// The part of a Sandbox's streams the driver uses.
function readable(stream) {
  stream.setEncoding("utf8");
  let text = "";
  stream.on("data", (chunk) => (text += chunk));
  const ended = new Promise((resolve) => stream.on("end", resolve));
  return { readText: async () => (await ended, text) };
}

class Sandbox {
  constructor(n, params) {
    this.sandboxId = `sb-fake-${n}`;
    const fail = n <= Number(process.env.FAKE_MODAL_FAIL ?? 0);
    const [program, ...args] = fail ? [process.execPath, "-e", "process.exit(1)"] : params.command;
    const child = spawn(program, args, { stdio: ["pipe", "pipe", "pipe"] });
    child.stdin.on("error", () => {});
    this.child = child;
    this.exit = new Promise((resolve) => child.on("close", (code) => resolve(code ?? 1)));
    this.lines = 0;
    this.stdin = {
      writeText: async (text) => {
        this.lines += text.split("\n").filter((l) => l).length;
        await new Promise((resolve) => child.stdin.write(text, resolve));
      },
      close: async () => {
        await new Promise((resolve) => child.stdin.end(resolve));
      },
    };
    this.stdout = readable(child.stdout);
    this.stderr = readable(child.stderr);
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
              command: params.command,
              lines: sb.lines,
            }) + "\n",
          );
        });
        return sb;
      },
    };
  }
}
