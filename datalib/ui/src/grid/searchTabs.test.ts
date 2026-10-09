import { describe, expect, it } from "vitest";
import { isNew, startTabs, tabAnswered, tabPicked, tabRecounted } from "./searchTabs";

const ready = (total: number) => ({ status: "ready", total, seen: false }) as const;

describe("search tabs", () => {
  it("open the first tab that comes back with rows", () => {
    let s = startTabs("warp");
    s = tabAnswered(s, "words", ready(0));
    expect(s.open).toBeNull();
    s = tabAnswered(s, "meaning", ready(7));
    expect(s.open).toBe("meaning");
  });

  /// The point of the tabs: an answer arriving never moves what is shown.
  it("never switch by themselves once one is open", () => {
    let s = startTabs("warp");
    s = tabAnswered(s, "fields", ready(3));
    s = tabAnswered(s, "words", ready(200));
    expect(s.open).toBe("fields");
    expect(isNew(s, "words")).toBe(true);
    expect(isNew(s, "fields")).toBe(false);
  });

  it("open the first tab when every one comes back empty", () => {
    let s = startTabs("zzz");
    for (const tab of ["meaning", "words", "fields"] as const) {
      s = tabAnswered(s, tab, ready(0));
    }
    expect(s.open).toBe("fields");
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
