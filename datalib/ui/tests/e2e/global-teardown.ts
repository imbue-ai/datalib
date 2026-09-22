// Stop the backends `playwright.config.ts` started, and fail the run if
// one does not stop. Playwright's `webServer` would own this, but it
// cannot own these servers: it needs a URL before the server exists, and
// each of these picks its own port by binding it.
//
// The wait is the point. A backend that ignores SIGTERM still goes away
// once this runner exits — its stdin is our pipe, and the parent-gone
// deadline ends it — so without the wait a broken shutdown is invisible
// here, which is how it stayed broken for a person at a terminal.
declare const process: {
  env: Record<string, string | undefined>;
  kill(pid: number, signal?: string | number): void;
};

// Well past `SHUTDOWN_DEADLINE` in `datalib/backend/http/src/main.rs`;
// a busy runner adds seconds, a hang adds forever.
const EXIT_TIMEOUT_MS = 20_000;

type Server = { name: string; pid: number; log: string };

function alive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export default async function globalTeardown(): Promise<void> {
  const servers = JSON.parse(process.env.DATALIB_TEST_E2E_SERVERS ?? "[]") as Server[];
  for (const s of servers) {
    try {
      process.kill(s.pid, "SIGTERM");
    } catch {
      // Already gone, which is what a server that died mid-run looks
      // like. The run is over; there is nothing to report it to.
    }
  }
  const deadline = Date.now() + EXIT_TIMEOUT_MS;
  let stillUp = servers.filter((s) => alive(s.pid));
  while (stillUp.length > 0 && Date.now() < deadline) {
    await sleep(100);
    stillUp = stillUp.filter((s) => alive(s.pid));
  }
  if (stillUp.length > 0) {
    throw new Error(
      `${stillUp.length} backend(s) still running ${EXIT_TIMEOUT_MS}ms after SIGTERM:\n` +
        stillUp.map((s) => `  ${s.name} (pid ${s.pid}, log ${s.log})`).join("\n"),
    );
  }
}
