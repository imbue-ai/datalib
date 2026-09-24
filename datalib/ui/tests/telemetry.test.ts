import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  errorEvent,
  flush,
  isSameOrigin,
  navigateEvent,
  nowIso,
  page,
  stampRequest,
  track,
} from "../src/telemetry";

/** A fake `fetch` that records each POST body; `status` is what it answers. */
function fakeFetch(status = 204) {
  const bodies: unknown[] = [];
  const spy = vi.fn(async (_url: unknown, init?: RequestInit) => {
    bodies.push(JSON.parse(init!.body as string));
    return new Response(null, { status });
  });
  vi.stubGlobal("fetch", spy);
  return { spy, bodies };
}

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(async () => {
  // Drain anything a test left queued so it cannot leak into the next.
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => new Response(null, { status: 204 })),
  );
  await flush();
  vi.unstubAllGlobals();
  vi.useRealTimers();
  vi.restoreAllMocks();
});

describe("nowIso", () => {
  it("stamps the page's clock with its offset, not UTC", () => {
    const s = nowIso(new Date(2026, 8, 21, 10, 0, 0, 250));
    // The wall time as the page saw it, and the offset it was in — the
    // shape `datalib_time::parse_strict` takes on the other side.
    expect(s.startsWith("2026-09-21T10:00:00.250")).toBe(true);
    expect(s).toMatch(/[+-]\d\d:\d\d$/);
    expect(s.endsWith("Z")).toBe(false);
  });
});

describe("the page", () => {
  it("has one id for its life, a UUID", () => {
    expect(page.process_id).toMatch(/^[0-9a-f-]{36}$/);
    expect(() => new Date(page.started_at)).not.toThrow();
  });
});

describe("track", () => {
  it("batches: one event waits for company, then goes with the page on it", async () => {
    const { spy, bodies } = fakeFetch();
    track("navigate", { path: "/cards" }, { msg: "/cards" });
    track("navigate", { path: "/data_sources" }, { msg: "/data_sources" });
    expect(spy).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(3_000);
    expect(spy).toHaveBeenCalledTimes(1);
    const [url, init] = spy.mock.calls[0] as [string, RequestInit];
    expect(url).toBe("/api/ui/events");
    expect(init.method).toBe("POST");
    expect(init.keepalive).toBe(true);
    const body = bodies[0] as { page: typeof page; events: unknown[]; closing: boolean };
    expect(body.page).toEqual(page);
    expect(body.closing).toBe(false);
    expect(body.events).toHaveLength(2);
    expect(body.events[0]).toMatchObject({
      name: "navigate",
      msg: "/cards",
      fields: { path: "/cards" },
    });
  });

  it("sends a full batch at once", async () => {
    const { spy } = fakeFetch();
    for (let i = 0; i < 50; i++) track("navigate", { i });
    await vi.advanceTimersByTimeAsync(0);
    expect(spy).toHaveBeenCalledTimes(1);
  });

  it("closing goes even with nothing queued, and says so", async () => {
    const { spy, bodies } = fakeFetch();
    await flush(true);
    expect(spy).toHaveBeenCalledTimes(1);
    expect((bodies[0] as { closing: boolean }).closing).toBe(true);
  });

  it("a refused batch is dropped with one warning, never thrown", async () => {
    fakeFetch(503);
    const warn = vi.spyOn(console, "warn").mockImplementation(() => {});
    track("navigate");
    await vi.advanceTimersByTimeAsync(3_000);
    track("navigate");
    await vi.advanceTimersByTimeAsync(3_000);
    expect(warn).toHaveBeenCalledTimes(1);
  });
});

describe("navigateEvent", () => {
  it("names the cards a path opens", () => {
    const ev = navigateEvent("/sourcesView()::/gridView():1.5:q%3Dhello", "/");
    expect(ev.name).toBe("navigate");
    expect(ev.msg).toBe("/sourcesView()::/gridView():1.5:q%3Dhello");
    expect(ev.fields).toEqual({
      path: "/sourcesView()::/gridView():1.5:q%3Dhello",
      cards: ["sourcesView()", "gridView()"],
      from: "/",
    });
  });

  it("an empty stack has no cards and a first load no `from`", () => {
    expect(navigateEvent("/", null).fields).toEqual({ path: "/" });
  });
});

describe("errorEvent", () => {
  it("keeps the message as the line and the stack in fields", () => {
    const err = new TypeError("x is not a function");
    const ev = errorEvent("vue", err, { info: "setup" });
    expect(ev.level).toBe("error");
    expect(ev.msg).toBe("TypeError: x is not a function");
    expect(ev.fields).toMatchObject({ source: "vue", info: "setup" });
    expect(typeof ev.fields!.stack).toBe("string");
  });

  it("a thrown non-Error is still a line", () => {
    const ev = errorEvent("promise", "just a string");
    expect(ev.msg).toBe("just a string");
    expect(ev.fields).toEqual({ source: "promise" });
  });
});

describe("isSameOrigin", () => {
  const origin = "http://127.0.0.1:8731";
  it("is true for relative and same-origin absolute URLs only", () => {
    expect(isSameOrigin("/api/dag", origin)).toBe(true);
    expect(isSameOrigin("applet/unified_index/rows?q=x", origin)).toBe(true);
    expect(isSameOrigin(`${origin}/api/health`, origin)).toBe(true);
    expect(isSameOrigin("https://example.com/x", origin)).toBe(false);
    expect(isSameOrigin("http://127.0.0.1:9999/api/health", origin)).toBe(false);
  });
});

describe("stampRequest", () => {
  it("names the card a request is for, beside the page", () => {
    const h = stampRequest(new Headers(), undefined, {
      id: "0192f6a0-0000-7000-8000-000000000000",
      type: "gridView",
    });
    expect(h.get("X-Datalib-Page")).toBe(page.process_id);
    expect(h.get("X-Datalib-Card")).toBe("0192f6a0-0000-7000-8000-000000000000");
    expect(h.get("X-Datalib-Card-Type")).toBe("gridView");
    expect(h.get("X-Datalib-Cause")).toBeNull();
  });

  it("says nothing about a card when no card asked", () => {
    const h = stampRequest(new Headers(), 3, null);
    expect(h.get("X-Datalib-Card")).toBeNull();
    expect(h.get("X-Datalib-Cause")).toBe("3");
  });
});
