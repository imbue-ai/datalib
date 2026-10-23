import { flushPromises, mount } from "@vue/test-utils";
import { afterEach, describe, expect, it, vi } from "vitest";
import { defineComponent, h } from "vue";

import { useDashboard } from "./useDashboard";
import type { CardCtx } from "./types";

// Reached through the log card's source; nothing here draws it.
vi.mock("@/components/RunLogPanel.ce.vue", () => ({ default: {} }));

/// The live stream, with a way to send a `root` frame through it.
class FramingEventSource {
  static CLOSED = 2;
  static last: FramingEventSource | null = null;
  readyState = 1;
  private root: ((m: MessageEvent) => void) | null = null;
  constructor() {
    FramingEventSource.last = this;
  }
  addEventListener(kind: string, f: (m: MessageEvent) => void) {
    if (kind === "root") this.root = f;
  }
  close() {}
  frame(data: unknown) {
    this.root?.(new MessageEvent("root", { data: JSON.stringify(data) }));
  }
}

const ctx = { cardId: "0192f6a0-0000-7000-8000-000000000000" } as unknown as CardCtx;

afterEach(() => vi.unstubAllGlobals());

describe("useDashboard", () => {
  /** The loop sends a burst of "rows changed" frames as a run's steps
   *  move, and each of the Dashboard's sections read the whole table
   *  again on every one of them. */
  it("reads the rows once for a burst of frames, and once more after", async () => {
    vi.stubGlobal("EventSource", FramingEventSource);
    let asked = 0;
    let release!: () => void;
    vi.stubGlobal("fetch", async (url: string) => {
      if (String(url).includes("/api/manage/rows")) {
        asked += 1;
        await new Promise<void>((r) => (release = r));
      }
      return new Response("{}", { status: 200 });
    });
    const card = mount(
      defineComponent({
        setup: () => {
          useDashboard(ctx, { recent: false });
          return () => h("div");
        },
      }),
    );
    release();
    await flushPromises();
    asked = 0;

    for (let i = 0; i < 5; i++) {
      FramingEventSource.last!.frame({ kind: "table_changed", table: "manage.rows" });
    }
    await flushPromises();
    expect(asked).toBe(1);
    release();
    await flushPromises();
    expect(asked).toBe(2);
    release();
    await flushPromises();
    expect(asked).toBe(2);
    card.unmount();
  });
});
