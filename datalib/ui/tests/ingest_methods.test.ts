import { describe, expect, it } from "vitest";
import { INGEST_METHODS, ingestLabel, ingestReach, methodsHeld } from "../src/config/ingestMethods";
import { CATALOG } from "../src/config/catalog";

describe("ingestReach", () => {
  it("reads a table by presence and a flag only when on", () => {
    expect(ingestReach("slack_api", { sync: {} })).toBe("origin");
    expect(ingestReach("slack_api", {})).toBeNull();
    // A table the provider never declared is not a method.
    expect(ingestReach("slack_api", { common: { input_path: "/x" } })).toBeNull();

    const exp = { common: { input_path: "/export" } };
    expect(ingestReach("linkedin", exp)).toBe("local");
    expect(ingestReach("linkedin", { ...exp, fetch_photos: true })).toBe("origin");
    expect(ingestReach("linkedin", { ...exp, fetch_photos: false })).toBe("local");
  });

  it("tells email's server modes from its mbox", () => {
    expect(ingestReach("email", { gmail_api: { user_id: "me" } })).toBe("origin");
    expect(ingestReach("email", { sync: { hostname: "api.fastmail.com" } })).toBe("origin");
    expect(ingestReach("email", { common: { input_path: "/mail.mbox" }, mbox: {} })).toBe("local");
    expect(methodsHeld("email", { common: { input_path: "/mail.mbox" }, mbox: {} })).toHaveLength(2);
  });

  it("knows nothing about a type with no provider", () => {
    expect(ingestReach("carrier_pigeon", { sync: {} })).toBeNull();
    expect(ingestReach(null, { sync: {} })).toBeNull();
  });
});

describe("ingestLabel", () => {
  it("says Download or Import, and nothing for a step naming no method", () => {
    expect(ingestLabel("claude_api", { sync: {} })).toBe("Download");
    expect(ingestLabel("pdf", { common: { input_path: "~/Documents" } })).toBe("Import");
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
});
