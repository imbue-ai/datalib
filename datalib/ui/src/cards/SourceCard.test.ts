import { flushPromises, mount } from "@vue/test-utils";
import { afterEach, describe, expect, it, vi } from "vitest";

import SourceCard from "./SourceCard.ce.vue";

// What the card draws inside it; nothing here is about them.
vi.mock("@/components/RunLogPanel.ce.vue", () => ({ default: {} }));
vi.mock("./GridCard.ce.vue", () => ({ default: {} }));
vi.mock("./TimeChart.ce.vue", () => ({ default: {} }));
vi.mock("./EntityChip.ce.vue", () => ({ default: {} }));
import type { CardCtx } from "./types";

/// The live stream, counted: one is open while anything listens.
class CountedEventSource {
  static CLOSED = 2;
  static open = 0;
  readyState = 0;
  constructor() {
    CountedEventSource.open += 1;
  }
  addEventListener() {}
  close() {
    CountedEventSource.open -= 1;
  }
}

const ctx = {
  cardId: "0192f6a0-0000-7000-8000-000000000000",
  cardType: "sourceView",
  initialState: "",
  setTitle: () => {},
  setHelp: () => {},
  bus: { publish: () => {}, subscribe: () => () => {} },
  host: { openCards: () => [], hrefFor: () => "#", setState: () => {} },
} as unknown as CardCtx;

afterEach(() => vi.unstubAllGlobals());

describe("SourceCard", () => {
  /** The card listened for live updates only once its first load came
   *  back; closed before then, it listened for good, and every visit
   *  that did so added one more reload to every update. */
  it("stops listening when it closes before its first load lands", async () => {
    vi.stubGlobal("EventSource", CountedEventSource);
    let release!: () => void;
    const held = new Promise<void>((r) => (release = r));
    vi.stubGlobal("fetch", async () => {
      await held;
      return new Response("{}", { status: 404 });
    });
    const card = mount(SourceCard, { props: { ctx, opts: { group: "slack" } } });
    card.unmount();
    release();
    await flushPromises();
    expect(CountedEventSource.open).toBe(0);
  });
});
