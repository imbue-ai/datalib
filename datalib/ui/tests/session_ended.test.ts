import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { App } from "vue";
import type { Router } from "vue-router";

/// How the server answers every request; 401 is `auth.rs` refusing a
/// token that changed when the server restarted.
let status = 401;

beforeEach(() => {
  vi.useFakeTimers();
  vi.resetModules();
  status = 401;
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => new Response("unauthorized: the server's API token.\n", { status })),
  );
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

async function bootedPage() {
  const { installTelemetry } = await import("@/telemetry");
  installTelemetry({ afterEach() {} } as unknown as Router, { config: {} } as unknown as App);
  // Through `@/`, as the app imports them: under Bazel a relative path
  // loads a second copy of a module, with its own session and toasts.
  return {
    api: await import("@/api"),
    toasts: (await import("@/toasts")).toasts,
  };
}

const whyTheTabIsDead = expect.stringMatching(/restarted, so this tab can no longer reach it/);

describe("a tab whose server was restarted", () => {
  /// An agent restarted datalib under an open tab, and every card's
  /// refusal stacked up its own error toast, each naming its URL, none
  /// saying why, and the polls added more every few seconds.
  it("shows one toast saying why, however much the page asks", async () => {
    const { api, toasts } = await bootedPage();
    const askEverything = () =>
      Promise.allSettled([
        api.fetchConfig(),
        api.fetchPipelineDisk(),
        api.fetchRequests(),
        api.fetchManageRows(),
      ]);

    await askEverything();
    // Past the window in which an identical toast is dropped.
    await vi.advanceTimersByTimeAsync(10_000);
    await askEverything();

    expect(toasts.map((t) => t.message)).toEqual([whyTheTabIsDead]);
  });

  it("clears the toasts that came before it", async () => {
    status = 500;
    const { api, toasts } = await bootedPage();
    await Promise.allSettled([api.fetchConfig()]);
    expect(toasts).toHaveLength(1);

    status = 401;
    await Promise.allSettled([api.fetchPipelineDisk()]);
    expect(toasts.map((t) => t.message)).toEqual([whyTheTabIsDead]);
  });
});
