// Two things that have to be true before any worker starts.
import { request } from "@playwright/test";
import { CONFIG_MUTATING } from "./config-mutating";
import { readFileSync, readdirSync } from "node:fs";

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

/// A backend announces its URL as soon as it has bound a port, which is
/// several seconds before it can answer anything — so this is where the
/// suite waits for the servers `playwright.config.ts` started. It is
/// what Playwright's `webServer` readiness probe would do, if the
/// servers could be run under `webServer` at all.
async function awaitHealthy(servers: Server[], token: string): Promise<void> {
  const deadline = Date.now() + 60_000;
  for (const server of servers) {
    const ctx = await request.newContext({ baseURL: server.url });
    try {
      for (;;) {
        const ok = await ctx
          .get(`/api/health?token=${token}`)
          .then((res) => res.ok())
          .catch(() => false);
        if (ok) break;
        if (Date.now() >= deadline) {
          throw new Error(
            `backend ${server.name} (${server.url}) never answered ` +
              `/api/health — its output is in ${server.log}`,
          );
        }
        await new Promise((resolve) => setTimeout(resolve, 100));
      }
    } finally {
      await ctx.dispose();
    }
  }
}

/// Spawn each sandbox backend's `unified_index` applet before its spec
/// asks for it.
///
/// An empty `q` is answered from SQL and never reaches qmd, which is
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
      await ctx.get("/applet/unified_index/search?q=&limit=1").catch(() => {});
      await ctx.dispose();
    }),
  );
}

export default async function globalSetup(): Promise<void> {
  // `decodeURIComponent`, because a file: URL percent-encodes: a checkout
  // whose path contains a space arrives as `%20` and `readdirSync` fails
  // with ENOENT. Only bites when cwd is the workspace — i.e. under
  // `bazel run`, which is how the snapshot-update workflow is invoked, so
  // `bazel test` never saw it.
  assertOnlyKnownSpecsWriteTheConfig(
    decodeURIComponent(new URL(".", import.meta.url).pathname),
  );
  const token = process.env.DATALIB_TOKEN ?? "";
  await awaitHealthy(
    JSON.parse(process.env.FW_E2E_SERVERS ?? "[]") as Server[],
    token,
  );
  const sandboxes = JSON.parse(process.env.FW_E2E_SANDBOXES ?? "[]") as {
    url: string;
  }[];
  await warmApplets(
    sandboxes.map((s) => s.url),
    token,
  );
}
