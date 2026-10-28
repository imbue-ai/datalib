import { describe, expect, it } from "vitest";
import { apiTab, isNew, startTabs, tabAnswered, tabPicked, tabRecounted } from "./searchTabs";

const ready = (total: number) => ({ status: "ready", total, seen: false }) as const;

describe("search tabs", () => {
  it("open on the best answer as the search starts", () => {
    const s = startTabs("warp");
    expect(s.open).toBe("best");
    expect(apiTab(s.open!)).toBeNull();
  });

  /// The point of the tabs: an answer arriving never moves what is shown.
  it("never switch by themselves once one is open", () => {
    let s = startTabs("warp");
    s = tabAnswered(s, "fields", ready(3));
    s = tabAnswered(s, "words", ready(200));
    expect(s.open).toBe("best");
    expect(isNew(s, "words")).toBe(true);
    s = tabAnswered(s, "best", ready(210));
    expect(s.open).toBe("best");
    expect(isNew(s, "best")).toBe(false);
  });

  it("stay on the best answer when every one comes back empty", () => {
    let s = startTabs("zzz");
    for (const tab of ["meaning", "words", "fields", "best"] as const) {
      s = tabAnswered(s, tab, ready(0));
    }
    expect(s.open).toBe("best");
  });

  /// The merged answer needs every ranking; when it cannot be had, the
  /// reader still sees the ones that came back.
  it("open the first tab with rows when the best answer fails", () => {
    let s = startTabs("warp");
    s = tabAnswered(s, "words", ready(0));
    s = tabAnswered(s, "best", { status: "failed", why: "no index" });
    expect(s.open).toBeNull();
    s = tabAnswered(s, "meaning", ready(7));
    expect(s.open).toBe("meaning");
  });

  it("stop being new once looked at", () => {
    let s = startTabs("warp");
    s = tabAnswered(s, "fields", ready(3));
    s = tabAnswered(s, "words", ready(200));
    s = tabPicked(s, "words");
    expect(s.open).toBe("words");
    expect(isNew(s, "words")).toBe(false);
    s = tabPicked(s, "fields");
    expect(isNew(s, "words")).toBe(false);
  });

  it("are new again after a recount finds more rows, and not before", () => {
    let s = startTabs("warp");
    s = tabAnswered(s, "fields", ready(3));
    s = tabAnswered(s, "words", ready(200));
    s = tabPicked(tabPicked(s, "words"), "fields");
    s = tabRecounted(s, "words", ready(200));
    expect(isNew(s, "words")).toBe(false);
    s = tabRecounted(s, "words", ready(205));
    expect(isNew(s, "words")).toBe(true);
    expect(s.open).toBe("fields");
  });

  it("can be picked while pending", () => {
    let s = startTabs("warp");
    s = tabAnswered(s, "fields", ready(3));
    s = tabPicked(s, "meaning");
    expect(s.open).toBe("meaning");
    expect(s.answers.meaning.status).toBe("pending");
  });
});
