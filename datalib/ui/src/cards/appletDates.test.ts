import { describe, expect, it, vi } from "vitest";
import { withDateChips } from "./appletDates";
import type { CardCtx, CardRender } from "./types";

function mount(render: CardRender) {
  const host = document.createElement("div");
  document.body.appendChild(host);
  const root = host.attachShadow({ mode: "open" });
  const openCards = vi.fn(() => []);
  const ctx = { host: { openCards } } as unknown as CardCtx;
  const teardown = withDateChips(render)(root, ctx);
  return { root, openCards, teardown };
}

/// The photo card's Taken date was plain text: an applet's card cannot
/// draw a chip, so the app draws the `<time>` it writes, even one written
/// after its fetch lands.
describe("withDateChips", () => {
  it("draws a date an applet's card writes later as a chip that opens its day", async () => {
    let write = () => {};
    const { root, openCards, teardown } = mount((r) => {
      write = () => {
        const t = document.createElement("time");
        t.setAttribute("datetime", "2369-04-14T12:05:00+01:00");
        t.textContent = "2369-04-14T12:05:00+01:00";
        r.appendChild(t);
      };
      return () => {};
    });
    write();
    await vi.waitFor(() => expect(root.querySelector("time.chip")).not.toBeNull());

    const chip = root.querySelector<HTMLElement>("time.chip")!;
    chip.dispatchEvent(new MouseEvent("dblclick", { bubbles: true, composed: true }));
    expect(openCards).toHaveBeenCalledTimes(1);
    expect(String((openCards.mock.calls[0] as unknown[])[0])).toMatch(/^searchView\(/);
    teardown();
  });

  it("leaves a time it cannot read as written", () => {
    const { root, teardown } = mount((r) => {
      const t = document.createElement("time");
      t.setAttribute("datetime", "sometime");
      t.textContent = "sometime";
      r.appendChild(t);
      return () => {};
    });
    expect(root.querySelector("time")!.textContent).toBe("sometime");
    teardown();
  });
});
