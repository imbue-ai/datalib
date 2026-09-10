import { describe, expect, it } from "vitest";
import { INGEST_METHODS, ingestLabel, ingestReach, methodsHeld } from "../src/config/ingestMethods";
import { CATALOG } from "../src/config/catalog";

describe("ingestReach", () => {
  it("reads a table by presence and a flag only when on", () => {
    expect(ingestReach("slack", { api: {} })).toBe("origin");
    expect(ingestReach("slack", {})).toBeNull();
    // A table the provider never declared is not a method.
    expect(ingestReach("slack", { export: { path: "/x" } })).toBeNull();

    const exp = { export: { path: "/export" } };
    expect(ingestReach("linkedin", exp)).toBe("local");
    expect(ingestReach("linkedin", { ...exp, fetch_photos: true })).toBe("origin");
    expect(ingestReach("linkedin", { ...exp, fetch_photos: false })).toBe("local");
  });

  it("tells email's server modes from its mbox", () => {
    expect(ingestReach("email", { gmail_api: { user_id: "me" } })).toBe("origin");
    expect(ingestReach("email", { jmap: { hostname: "api.fastmail.com" } })).toBe("origin");
    expect(ingestReach("email", { mbox: { path: "/mail.mbox" } })).toBe("local");
    expect(methodsHeld("email", { mbox: { path: "/mail.mbox" } })).toHaveLength(1);
  });

  it("tells claude's two methods apart", () => {
    expect(ingestReach("claude", { api: {} })).toBe("origin");
    expect(ingestReach("claude", { export: { path: "~/claude-export" } })).toBe("local");
  });

  it("knows nothing about a type with no provider", () => {
    expect(ingestReach("carrier_pigeon", { api: {} })).toBeNull();
    expect(ingestReach(null, { api: {} })).toBeNull();
    // The spellings retired with the method tables are not types.
    expect(ingestReach("slack_api", { sync: {} })).toBeNull();
  });
});

describe("ingestLabel", () => {
  it("says Download or Import, and nothing for a step naming no method", () => {
    expect(ingestLabel("claude", { api: {} })).toBe("Download");
    expect(ingestLabel("pdf", { fswalk: { path: "~/Documents" } })).toBe("Import");
    expect(ingestLabel("pdf", {})).toBeNull();
  });
});

describe("the generated mirror", () => {
  /// Every type the picker offers is one the backend declared methods
  /// for; a catalog entry with none would show "Ingest" forever.
  it("covers every catalog type", () => {
    const missing = [...new Set(CATALOG.map((e) => e.type))].filter(
      (t) => !(INGEST_METHODS[t]?.length > 0),
    );
    expect(missing).toEqual([]);
  });

  /// An entry's `method` is one of the tables its provider declares, or
  /// the form would write a table `datalib-step` does not read.
  it("names a declared method on every entry that has one", () => {
    for (const e of CATALOG) {
      if (!e.method) continue;
      expect(INGEST_METHODS[e.type].map((m) => m.path), e.type).toContain(e.method);
    }
  });
});
