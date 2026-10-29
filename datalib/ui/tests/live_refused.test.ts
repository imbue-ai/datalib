import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

/// An `EventSource` whose server answers every connection a millisecond
/// later, the way a local server does: `refuse` closes it as the browser
/// does on a 401 or a 502, `accept` opens it.
class FakeEventSource {
  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSED = 2;
  static opened: FakeEventSource[] = [];
  static answer: "refuse" | "accept" = "refuse";

  readyState = FakeEventSource.CONNECTING;
  onerror: (() => void) | null = null;
  onopen: (() => void) | null = null;

  constructor(readonly url: string) {
    FakeEventSource.opened.push(this);
    setTimeout(() => {
      if (this.readyState === FakeEventSource.CLOSED) return;
      if (FakeEventSource.answer === "refuse") {
        this.readyState = FakeEventSource.CLOSED;
        this.onerror?.();
      } else {
        this.readyState = FakeEventSource.OPEN;
        this.onopen?.();
      }
    }, 1);
  }

  addEventListener() {}

  close() {
    this.readyState = FakeEventSource.CLOSED;
  }
}

/// What `/api/health` answers: a status, or `down` for no server at all.
let health: number | "down" = 200;
let healthAsked = 0;

beforeEach(() => {
  vi.useFakeTimers();
  vi.resetModules();
  FakeEventSource.opened = [];
  FakeEventSource.answer = "refuse";
  healthAsked = 0;
  vi.stubGlobal("EventSource", FakeEventSource);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (input: RequestInfo | URL) => {
      if (String(input) === "/api/health") {
        healthAsked += 1;
        if (health === "down") throw new TypeError("Failed to fetch");
        return new Response("{}", { status: health });
      }
      return new Response("{}", { status: 200 });
    }),
  );
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("a stream the server refuses", () => {
  /// An agent restarted datalib under an open tab. The new server had a
  /// new token, so it refused the tab's stream; the tab reconnected the
  /// moment each refusal came back and refetched every card each time:
  /// 35,000 requests in 25 seconds, until someone reloaded.
  it("stops, and says so, once the server no longer knows the tab", async () => {
    health = 401;
    const { subscribeLive } = await import("../src/live");
    const { sessionEnded } = await import("../src/session");
    const resync = vi.fn();
    subscribeLive({ resync });

    await vi.advanceTimersByTimeAsync(1_000);
    expect(FakeEventSource.opened.length).toBe(1);

    // Past the watchdog, which would otherwise start it all again.
    await vi.advanceTimersByTimeAsync(60_000);
    expect(FakeEventSource.opened.length).toBe(1);
    expect(healthAsked).toBe(1);
    expect(resync).not.toHaveBeenCalled();
    expect(sessionEnded()).toBe(true);
  });

  /// The same loop, with the server merely gone: a proxy in front of it
  /// answers 502, which closes the stream just as a 401 does.
  it("waits longer between tries while the server is away", async () => {
    health = "down";
    const { subscribeLive } = await import("../src/live");
    const resync = vi.fn();
    subscribeLive({ resync });

    await vi.advanceTimersByTimeAsync(10_000);
    // 0 s, then 1, 2 and 4 s later: four tries in ten seconds.
    expect(FakeEventSource.opened.length).toBe(4);
    expect(resync).not.toHaveBeenCalled();

    FakeEventSource.answer = "accept";
    health = 200;
    await vi.advanceTimersByTimeAsync(8_000);
    expect(FakeEventSource.opened.length).toBe(5);
    // What the page missed is fetched once, when the stream is back.
    expect(resync).toHaveBeenCalledTimes(1);
  });

  it("starts the wait over once a stream has opened", async () => {
    health = "down";
    const { subscribeLive } = await import("../src/live");
    subscribeLive({});

    await vi.advanceTimersByTimeAsync(10_000);
    FakeEventSource.answer = "accept";
    await vi.advanceTimersByTimeAsync(8_000);
    const reopened = FakeEventSource.opened.length;

    FakeEventSource.answer = "refuse";
    FakeEventSource.opened.at(-1)!.readyState = FakeEventSource.CLOSED;
    FakeEventSource.opened.at(-1)!.onerror?.();
    await vi.advanceTimersByTimeAsync(1_100);
    expect(FakeEventSource.opened.length).toBe(reopened + 1);
  });
});
