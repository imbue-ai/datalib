import { mount } from "@vue/test-utils";
import { describe, expect, it, vi } from "vitest";

import ChipMenu from "./ChipMenu.ce.vue";
import { menuPlacement } from "./chipActions";

describe("ChipMenu", () => {
  /** A second right-click closed the menu and went on to the document
   *  under it, which opened its own menu in the chip menu's place. */
  it("keeps the right-click that closes it from reaching the page", async () => {
    const page = document.createElement("div");
    const pageMenu = vi.fn();
    page.addEventListener("contextmenu", pageMenu);
    document.body.append(page);
    const menu = mount(ChipMenu, {
      props: { entries: [{ id: "copy-id", label: "Copy" }], x: 10, y: 10 },
      attachTo: page,
    });
    await menu.find(".chip-menu-overlay").trigger("contextmenu");
    expect(menu.emitted("close")).toHaveLength(1);
    expect(pageMenu).not.toHaveBeenCalled();
    menu.unmount();
    page.remove();
  });
});

describe("menuPlacement", () => {
  const viewport = { width: 800, height: 600 };

  it("opens at the pointer where the menu fits", () => {
    expect(menuPlacement({ x: 100, y: 100 }, { width: 300, height: 200 }, viewport)).toEqual({
      left: 100,
      top: 100,
    });
  });

  /** A 320px menu was kept only 260px from the right edge, so its
   *  longest entries ran off the screen. */
  it("moves left and up by the menu's own size near an edge", () => {
    expect(menuPlacement({ x: 700, y: 550 }, { width: 320, height: 226 }, viewport)).toEqual({
      left: 472,
      top: 366,
    });
  });

  it("never starts off the top-left corner", () => {
    expect(menuPlacement({ x: 50, y: 50 }, { width: 900, height: 700 }, viewport)).toEqual({
      left: 0,
      top: 0,
    });
  });
});
