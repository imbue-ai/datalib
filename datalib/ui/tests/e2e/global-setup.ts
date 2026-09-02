// Two things that have to be true before any worker starts.
//
// Runs in the process that loaded `playwright.config.ts`, after
// `webServer` has every backend up, and before the `warmup` setup
// project — so the env that config cached is here to read.
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
///
/// The tell is the config editor: `.m2-editor` (the Pipeline screen's
/// Advanced pane) or the older Sources screen's save banner. Both are
/// how a spec can change the file at all.
function assertOnlyKnownSpecsWriteTheConfig(dir: string): void {
  const offenders: string[] = [];
  for (const file of readdirSync(dir)) {
    if (!file.endsWith(".spec.ts")) continue;
    const base = file.replace(/\.spec\.ts$/, "");
    if ((CONFIG_MUTATING as readonly string[]).includes(base)) continue;
    const text = readFileSync(`${dir}/${file}`, "utf8");
    if (text.includes(".m2-editor") || text.includes("Saved the config")) {
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

/// Spawn each sandbox backend's `unified_index` applet before its spec
/// asks for it.
///
/// The gateway starts an applet on the first request that needs one,
/// and `/api/health` — which is what `webServer` waits on — answers
/// before any of that has happened, so the first `/applet/...` request
/// can land on a 502. `qmd-warmup.setup.ts` absorbs that for the shared
/// backend; these are the other five.
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
  assertOnlyKnownSpecsWriteTheConfig(new URL(".", import.meta.url).pathname);
  const sandboxes = JSON.parse(process.env.FW_E2E_SANDBOXES ?? "[]") as {
    url: string;
  }[];
  await warmApplets(
    sandboxes.map((s) => s.url),
    process.env.DATALIB_TOKEN ?? "",
  );
}
