// The page's own record in the server's log: one load of the app in
// one tab is a `ui` process, and what happens on it is `track`ed and
// posted to `POST /api/ui/events` in batches (`docs/dev/logging.md`).
//
// Reporting must never get in the way: a batch that fails to send is
// dropped, with one warning in the console, and nothing here throws.

import type { App } from "vue";
import { START_LOCATION, type Router } from "vue-router";
import { decodeColumns } from "@/router/columns";
import { isDesktopApp } from "@/desktop";
import { CAUSE_HEADER, chainOfFrameBeingHandled } from "@/live";

export const PAGE_HEADER = "X-Datalib-Page";

/// The events the UI reports. A closed set on this side so a callsite
/// cannot misspell one; the server takes any word and files it as
/// `ui.<name>`, so an older server never refuses a newer page.
export type PageEventName = "page_load" | "page_hide" | "navigate" | "error";

export type PageEventLevel = "info" | "warn" | "error";

export type PageEvent = {
  at: string;
  name: PageEventName;
  level?: PageEventLevel;
  msg?: string;
  fields?: Record<string, unknown>;
};

/// How long a queued event waits for company before the batch goes.
const FLUSH_AFTER_MS = 3_000;
/// A batch this full goes at once.
const FLUSH_AT = 50;
/// A stack trace is kept to this many characters.
const MAX_STACK = 4_000;

/// This load's process: minted here, kept for the page's life. A reload
/// is a new page, as a restart is a new launch of the server.
export const page = {
  process_id: crypto.randomUUID(),
  started_at: nowIso(),
};

let queue: PageEvent[] = [];
let timer: ReturnType<typeof setTimeout> | null = null;
let warned = false;

export function track(
  name: PageEventName,
  fields?: Record<string, unknown>,
  opts: { level?: PageEventLevel; msg?: string } = {},
): void {
  queue.push({ at: nowIso(), name, level: opts.level, msg: opts.msg, fields });
  if (queue.length >= FLUSH_AT) {
    void flush();
  } else if (!timer) {
    timer = setTimeout(() => void flush(), FLUSH_AFTER_MS);
  }
}

/// Send what is queued. `closing` also closes the page's process and
/// asks the browser to finish the request after the page is gone.
export async function flush(closing = false): Promise<void> {
  if (timer) {
    clearTimeout(timer);
    timer = null;
  }
  if (queue.length === 0 && !closing) return;
  const events = queue;
  queue = [];
  try {
    const r = await fetch("/api/ui/events", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ page, events, closing }),
      keepalive: true,
    });
    if (!r.ok && !warned) {
      warned = true;
      console.warn(
        `telemetry: /api/ui/events → ${r.status}; this page's actions are not being logged`,
      );
    }
  } catch (e) {
    if (!warned) {
      warned = true;
      console.warn(`telemetry: /api/ui/events failed: ${(e as Error).message}`);
    }
  }
}

/// What an uncaught error becomes: its message as the line, its stack
/// and where it came from as fields.
export function errorEvent(
  source: string,
  err: unknown,
  extra: Record<string, unknown> = {},
): PageEvent {
  const e = err instanceof Error ? err : null;
  const msg = e ? `${e.name}: ${e.message}` : String(err);
  const fields: Record<string, unknown> = { source, ...extra };
  if (e?.stack) fields.stack = e.stack.slice(0, MAX_STACK);
  return { at: nowIso(), name: "error", level: "error", msg, fields };
}

export function trackError(
  source: string,
  err: unknown,
  extra: Record<string, unknown> = {},
): void {
  const ev = errorEvent(source, err, extra);
  track(ev.name, ev.fields, { level: ev.level, msg: ev.msg });
}

/// The `navigate` line for a path: the path itself, and the card codes
/// it opens (the path *is* the column stack; see `router/columns.ts`).
export function navigateEvent(path: string, from: string | null): PageEvent {
  const codes = decodeColumns(path).map((c) => c.code);
  const fields: Record<string, unknown> = { path };
  if (codes.length) fields.cards = codes;
  if (from != null) fields.from = from;
  return { at: nowIso(), name: "navigate", msg: path, fields };
}

/// Wire the page up: the id on every same-origin request (and the
/// chain of the `root` frame that caused it, if one did), the load and
/// unload events, uncaught errors, and every route change. Once, at boot.
export function installTelemetry(router: Router, app: App): void {
  const origin = window.location.origin;
  const nativeFetch = window.fetch.bind(window);
  window.fetch = (input, init) => {
    const url = typeof input === "string" ? input : input instanceof URL ? input.href : input.url;
    if (isSameOrigin(url, origin)) {
      const headers = new Headers(
        init?.headers ?? (input instanceof Request ? input.headers : undefined),
      );
      headers.set(PAGE_HEADER, page.process_id);
      const chain = chainOfFrameBeingHandled();
      if (chain !== undefined) headers.set(CAUSE_HEADER, String(chain));
      init = { ...init, headers };
    }
    return nativeFetch(input, init);
  };

  track("page_load", {
    user_agent: navigator.userAgent,
    viewport: `${window.innerWidth}x${window.innerHeight}`,
    shell: isDesktopApp() ? "desktop" : "browser",
    path: window.location.pathname,
  });

  window.addEventListener("error", (ev) => {
    const where: Record<string, unknown> = {};
    if (ev.filename) where.file = ev.filename;
    if (ev.lineno) where.line = ev.lineno;
    trackError("window", ev.error ?? ev.message, where);
  });
  window.addEventListener("unhandledrejection", (ev) => {
    trackError("promise", ev.reason);
  });
  app.config.errorHandler = (err, _instance, info) => {
    trackError("vue", err, { info });
    console.error(err);
  };

  router.afterEach((to, from) => {
    // The first route comes from nowhere; `from` there is a placeholder.
    const ev = navigateEvent(to.fullPath, from === START_LOCATION ? null : from.fullPath);
    track(ev.name, ev.fields, { msg: ev.msg });
  });

  // `pagehide` is the last event a page reliably gets, in a tab close,
  // a reload and a navigation away alike; `visibilitychange` catches a
  // tab going to the background, from which it may never come back.
  window.addEventListener("pagehide", () => {
    track("page_hide");
    void flush(true);
  });
  document.addEventListener("visibilitychange", () => {
    if (document.visibilityState === "hidden") void flush();
  });
}

export function isSameOrigin(url: string, origin: string): boolean {
  try {
    return new URL(url, origin).origin === origin;
  } catch {
    return false;
  }
}

/// The page's clock, with the offset it is in: `2026-09-21T10:00:00.250-07:00`.
/// `toISOString()` would give UTC and lose where the page was.
export function nowIso(d: Date = new Date()): string {
  const pad = (n: number, w = 2) => String(n).padStart(w, "0");
  const offsetMin = -d.getTimezoneOffset();
  const sign = offsetMin >= 0 ? "+" : "-";
  const abs = Math.abs(offsetMin);
  return (
    `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}` +
    `T${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}.${pad(d.getMilliseconds(), 3)}` +
    `${sign}${pad(Math.floor(abs / 60))}:${pad(abs % 60)}`
  );
}
