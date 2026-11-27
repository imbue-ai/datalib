// Two things that have to be true before any worker starts, and a
// record of how busy the machine was for the rest of the run.
import { request } from "@playwright/test";
import { CONFIG_MUTATING } from "./config-mutating";
import { appendFileSync, existsSync, mkdirSync, readFileSync, readdirSync } from "node:fs";

// Declared locally rather than pulling in @types/node — same reason as
// api-token.spec.ts: tsconfig's `types` is deliberately narrow.
declare const process: { env: Record<string, string | undefined> };

/// A spec that writes `config.toml` and is not in `CONFIG_MUTATING`
/// runs against the *shared* fixture root, where its edits are visible
/// to every other spec running beside it. The symptom is some unrelated
/// spec failing intermittently, which is worth catching by name here
/// rather than by bisecting a flake later.
function assertOnlyKnownSpecsWriteTheConfig(dir: string): void {
  const offenders: string[] = [];
  for (const file of readdirSync(dir)) {
    if (!file.endsWith(".spec.ts")) continue;
    const base = file.replace(/\.spec\.ts$/, "");
    if ((CONFIG_MUTATING as readonly string[]).includes(base)) continue;
    const text = readFileSync(`${dir}/${file}`, "utf8");
    if (
      text.includes(".m2-editor") ||
      text.includes("Saved the config") ||
      text.includes("writeFileSync")
    ) {
      offenders.push(file);
    }
  }
  if (offenders.length > 0) {
    throw new Error(
      `these specs write config.toml but are not in CONFIG_MUTATING, so they ` +
        `share a data root with every other spec and will corrupt it under ` +
        `parallel workers: ${offenders.join(", ")}. Add them to ` +
        `tests/e2e/config-mutating.ts — each entry gets its own root and backend.`,
    );
  }
}

type Server = { name: string; url: string; log: string };
// `/api/config`'s `upgrade`, the part of it read here.
type Upgrade = {
  settled: boolean;
  steps: { step: string; state: string; error: string | null }[];
};

/// A backend prints its URL as soon as it has bound a port, which is
/// several seconds before it can answer anything — so this is where the
/// suite waits for the servers `playwright.config.ts` started. It is
/// what Playwright's `webServer` readiness probe would do, if the
/// servers could be run under `webServer` at all.
///
/// A prepare pass that ended with a step it could not migrate is an
/// error here, not a banner: every spec on that root would run under "N
/// sources could not be updated", and the cause is in the server's log,
/// where no spec looks.
async function awaitHealthy(servers: Server[], token: string): Promise<void> {
  const deadline = Date.now() + 60_000;
  for (const server of servers) {
    const ctx = await request.newContext({ baseURL: server.url });
    try {
      for (;;) {
        // Listening is not enough: the first boot of a build runs every
        // step's command with `--prepare` appended before it syncs, and a
        // spec that syncs would find its Sync disabled until then.
        const upgrade = await ctx
          .get(`/api/config`, { headers: { authorization: `Bearer ${token}` } })
          .then(async (res) => (res.ok() ? ((await res.json()).upgrade as Upgrade) : undefined))
          .catch(() => undefined);
        if (upgrade?.settled === true) {
          const failed = upgrade.steps.filter((s) => s.state === "failed");
          if (failed.length > 0) {
            throw new Error(
              `backend ${server.name} (${server.url}) could not migrate ` +
                `${failed.length} of the ${upgrade.steps.length} steps its launch pass asked, ` +
                `so its specs would run under the "could not be updated" banner. ` +
                `First: ${failed[0].step}: ${failed[0].error}. Its output is in ${server.log}`,
            );
          }
          break;
        }
        if (Date.now() >= deadline) {
          throw new Error(
            `backend ${server.name} (${server.url}) never answered ` +
              `/api/config with its launch pass settled — its output is in ${server.log}`,
          );
        }
        await new Promise((resolve) => setTimeout(resolve, 100));
      }
    } finally {
      await ctx.dispose();
    }
  }
}

/// Spawn each sandbox backend's `unified_grid_and_search_index` applet before its spec
/// asks for it.
///
/// An empty `q` is answered from SQL and never reaches the ranked search, which is
/// what is wanted: no spec on a sandbox root issues a free-text query,
/// so none of them should pay a model load.
async function warmApplets(urls: string[], token: string): Promise<void> {
  await Promise.all(
    urls.map(async (base) => {
      const ctx = await request.newContext({
        baseURL: base,
        extraHTTPHeaders: { authorization: `Bearer ${token}` },
      });
      // Unasserted: a spec that needs the applet will say so far more
      // clearly than a setup step can, and one that doesn't should not
      // be blocked by a warm-up.
      await ctx.get("/applet/unified_grid_and_search_index/search?q=&limit=1").catch(() => {});
      await ctx.dispose();
    }),
  );
}

/// Ask every backend for the Manage table once, and wait for each answer.
///
/// `upgrade.settled` is not ready for the first spec on a root: its first
/// page is the sources card, which draws from `/api/manage/rows`, and
/// specs give that card five seconds. On a busy CI runner the first
/// answer after a launch has taken longer, and the first spec of a
/// sandboxed root failed on a card still saying "Loading the sources…".
async function awaitManageRows(servers: Server[], token: string): Promise<void> {
  await Promise.all(
    servers.map(async (server) => {
      const ctx = await request.newContext({
        baseURL: server.url,
        extraHTTPHeaders: { authorization: `Bearer ${token}` },
      });
      try {
        const res = await ctx.get("/api/manage/rows", { timeout: 60_000 });
        if (!res.ok()) {
          throw new Error(
            `backend ${server.name} (${server.url}) answered /api/manage/rows with ` +
              `${res.status()} — its output is in ${server.log}`,
          );
        }
      } finally {
        await ctx.dispose();
      }
    }),
  );
}

/// How busy the machine is, once a second for the whole run, so a
/// failure's trace can be laid against it: a page that painted nothing
/// for seconds while every core was taken was starved, not slow. Linux
/// only (it reads `/proc`), and only under bazel, where the file lands
/// in the report zip CI keeps for a red run
/// (`tests/e2e/README-artifacts.md`).
function sampleCpu(): () => void {
  const out = process.env.TEST_UNDECLARED_OUTPUTS_DIR;
  if (!out || !existsSync("/proc/stat")) return () => {};
  const dir = `${out}/diagnostics`;
  mkdirSync(dir, { recursive: true });
  const file = `${dir}/cpu.tsv`;
  appendFileSync(file, "utc\tbusy_percent\tloadavg_1m\n");
  const counters = (): { busy: number; total: number } => {
    const fields = readFileSync("/proc/stat", "utf8")
      .split("\n")[0]
      .trim()
      .split(/\s+/)
      .slice(1)
      .map(Number);
    const total = fields.reduce((a, b) => a + b, 0);
    const idle = fields[3] + (fields[4] ?? 0);
    return { busy: total - idle, total };
  };
  let last = counters();
  const timer = setInterval(() => {
    const now = counters();
    const busy = (100 * (now.busy - last.busy)) / Math.max(1, now.total - last.total);
    last = now;
    const load = readFileSync("/proc/loadavg", "utf8").split(" ")[0];
    appendFileSync(file, `${new Date().toISOString()}\t${busy.toFixed(0)}\t${load}\n`);
  }, 1_000);
  return () => clearInterval(timer);
}

export default async function globalSetup(): Promise<() => void> {
  const stopSampling = sampleCpu();
  try {
    // `decodeURIComponent`, because a file: URL percent-encodes: a checkout
    // whose path contains a space arrives as `%20` and `readdirSync` fails
    // with ENOENT. Only bites when cwd is the workspace — i.e. under
    // `bazel run`, which is how the snapshot-update workflow is invoked, so
    // `bazel test` never saw it.
    assertOnlyKnownSpecsWriteTheConfig(decodeURIComponent(new URL(".", import.meta.url).pathname));
    const token = process.env.DATALIB_TOKEN ?? "";
    const servers = JSON.parse(process.env.DATALIB_TEST_E2E_SERVERS ?? "[]") as Server[];
    await awaitHealthy(servers, token);
    await awaitManageRows(servers, token);
    const sandboxes = JSON.parse(process.env.DATALIB_TEST_E2E_SANDBOXES ?? "[]") as {
      url: string;
    }[];
    await warmApplets(
      sandboxes.map((s) => s.url),
      token,
    );
  } catch (err) {
    stopSampling();
    throw err;
  }
  // Playwright runs a function globalSetup returns as one more teardown.
  return stopSampling;
}
