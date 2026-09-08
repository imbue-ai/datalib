// TEMPORARY diagnostic instrumentation for the manager2-sync flake.
// Not part of the suite's contract; delete once the cause is settled.

import type { APIRequestContext, Page } from "@playwright/test";

declare const process: { env: Record<string, string | undefined> };

/// Record every /api/sync/jobs fetch and every SSE frame the page sees,
/// stamped with absolute epoch ms so they line up with the status log.
export async function instrument(page: Page) {
  await page.addInitScript(() => {
    const w = window as unknown as {
      __net: unknown[];
      fetch: typeof fetch;
      EventSource: typeof EventSource;
    };
    w.__net = [];
    const brief = (j: Record<string, unknown>) =>
      `${String(j.id).slice(0, 8)}|${j.source_name}|${j.state}|c=${j.created_at}|s=${j.started_at}|f=${j.finished_at}`;
    const origFetch = w.fetch.bind(window);
    w.fetch = async (...args: Parameters<typeof fetch>) => {
      const url = typeof args[0] === "string" ? args[0] : String((args[0] as Request).url);
      const method = (args[1] as RequestInit | undefined)?.method ?? "GET";
      const started = Date.now();
      const r = await origFetch(...args);
      if (/\/api\/(sync\/jobs|dag)/.test(url)) {
        let body: unknown = null;
        try {
          body = await r.clone().json();
        } catch {
          /* not json */
        }
        let summary: unknown = null;
        if (Array.isArray(body))
          summary = body.slice(0, 4).map((j) => brief(j as Record<string, unknown>));
        else if (body && (body as Record<string, unknown>).id)
          summary = [brief(body as Record<string, unknown>)];
        else if (body && (body as Record<string, unknown>).run)
          summary = JSON.stringify((body as Record<string, unknown>).run);
        w.__net.push({ kind: "fetch", method, url, started, settled: Date.now(), summary });
      }
      return r;
    };
    const OrigES = w.EventSource;
    const Patched = function (this: unknown, url: string, init?: EventSourceInit) {
      const es = new OrigES(url, init);
      // A logging listener alongside whatever the app attaches. `onmessage`
      // and addEventListener both fire, so nothing has to be intercepted.
      for (const type of ["message", "root"]) {
        es.addEventListener(type, (ev: Event) => {
          w.__net.push({
            kind: "sse",
            type,
            at: Date.now(),
            data: String((ev as MessageEvent).data).slice(0, 400),
          });
        });
      }
      return es;
    } as unknown as typeof EventSource;
    Patched.prototype = OrigES.prototype;
    w.EventSource = Patched;
  });
}

/// Hold every `/api/sync/jobs/all` response back by N ms *after* the
/// server answered, so the snapshot the page commits is that much older
/// than the page's other sources. Models a slow answer from the
/// single-connection jobs store under contention.
export async function delayJobList(page: Page) {
  const ms = Number(process.env.FW_E2E_DELAY_JOBS ?? "0");
  if (!ms) return;
  await page.route("**/api/sync/jobs/all*", async (route) => {
    const response = await route.fetch();
    await new Promise((r) => setTimeout(r, ms));
    await route.fulfill({ response });
  });
}

export async function netLog(page: Page): Promise<unknown[]> {
  return page.evaluate(() => (window as unknown as { __net?: unknown[] }).__net ?? []);
}

/// Frame timestamps recorded alongside `__statusLog`.
export async function frameTimes(page: Page, id: string): Promise<number[]> {
  return page.evaluate(
    (id: string) =>
      (window as unknown as { __statusTimes?: Record<string, number[]> }).__statusTimes?.[id] ??
      [],
    id,
  );
}

/// Every job the backend has ever had, newest first.
export async function serverJobs(request: APIRequestContext): Promise<string[]> {
  const rows = (await (await request.get("/api/sync/jobs/all?limit=100")).json()) as Record<
    string,
    unknown
  >[];
  return rows.slice(0, 12).map(
    (j) =>
      `${String(j.id).slice(0, 8)}|${j.source_name}|${j.state}|c=${j.created_at}|s=${j.started_at}|f=${j.finished_at}`,
  );
}

export async function dumpProbe(
  page: Page,
  request: APIRequestContext,
  label: string,
  extra: Record<string, unknown>,
) {
  const payload = {
    label,
    ...extra,
    frameTimesUp: await frameTimes(page, "pdfs/raw"),
    frameTimesDown: await frameTimes(page, "pdfs/rendered_md"),
    serverJobs: await serverJobs(request),
    net: await netLog(page),
  };
  const text = JSON.stringify(payload, null, 2);
  // A directory the caller made. Writing a copy there is a convenience
  // for comparing passing runs against failing ones; a failure carries
  // the same text in its assertion message either way, so a missing or
  // unwritable directory must not take the test down with it.
  const dir = process.env.FW_E2E_PROBE_DIR;
  if (dir) {
    try {
      const fs = await import("node:fs");
      fs.writeFileSync(`${dir}/probe-${Date.now()}-${label}.json`, text);
    } catch {
      /* no probe copy on disk; the assertion message still has it */
    }
  }
  return text;
}
