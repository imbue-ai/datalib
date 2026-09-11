// Stop the backends `playwright.config.ts` started. Playwright's
// `webServer` would own this, but it cannot own these servers: it needs
// a URL before the server exists, and each of these picks its own port
// by binding it.
declare const process: {
  env: Record<string, string | undefined>;
  kill(pid: number, signal?: string): void;
};

export default function globalTeardown(): void {
  const servers = JSON.parse(process.env.FW_E2E_SERVERS ?? "[]") as {
    pid: number;
  }[];
  for (const s of servers) {
    try {
      process.kill(s.pid, "SIGTERM");
    } catch {
      // Already gone, which is what a server that died mid-run looks
      // like. The run is over; there is nothing to report it to.
    }
  }
}
