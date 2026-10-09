import { describe, expect, it } from "vitest";
import { countOf, loadingText } from "@/config/probeProgress";

describe("loadingText", () => {
  it("says only what it is loading until something has come back", () => {
    expect(loadingText("channels", null, 0.4)).toBe("Loading channels…");
    expect(loadingText("channels", { done: 0, total: null }, 0.4)).toBe("Loading channels…");
  });

  it("counts what has come so far when the total is unknown", () => {
    expect(loadingText("channels", { done: 400, total: null }, 12.7)).toBe(
      "Loading channels… 400 so far · 12s",
    );
  });

  it("counts against the total when the service says one", () => {
    expect(loadingText("conversations", { done: 120, total: 480 }, 3)).toBe(
      "Loading conversations… 120 of 480 · 3s",
    );
  });
});

describe("countOf", () => {
  it("counts one in the singular and the rest in the plural", () => {
    expect(countOf(1, "conversations")).toBe("1 conversation");
    expect(countOf(1, "address books")).toBe("1 address book");
    expect(countOf(3, "channels")).toBe("3 channels");
    expect(countOf(0, "labels")).toBe("0 labels");
  });
});
