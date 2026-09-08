// Removes the data roots playwright.config.ts minted for this run, but
// only in the mode where nothing else would. Under `bazel test` they sit
// in TEST_TMPDIR, which bazel keeps until the target's next run and wipes
// then — so the failing run's roots are still there to inspect, and
// deleting them here would buy nothing but cost that.
import { rmSync } from "node:fs";

// Declared locally rather than pulling in @types/node — same reason as
// global-setup.ts: tsconfig's `types` is deliberately narrow.
declare const process: { env: Record<string, string | undefined> };

export default function globalTeardown(): void {
  if (process.env.TEST_TMPDIR || process.env.FW_E2E_KEEP_ROOTS) return;
  const minted = JSON.parse(
    process.env.FW_E2E_MINTED_DIRS ?? "[]",
  ) as string[];
  for (const dir of minted) {
    // A root that will not delete is worth saying out loud, but not worth
    // failing a green run over, nor skipping the rest of the list.
    try {
      rmSync(dir, { recursive: true, force: true });
    } catch (e) {
      console.warn(`global-teardown: could not remove ${dir}: ${e}`);
    }
  }
}
