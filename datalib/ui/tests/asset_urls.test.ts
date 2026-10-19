import { describe, expect, it } from "vitest";
import { assetUrl, isAbsoluteOrUrl } from "../src/cards/asset_urls";

describe("isAbsoluteOrUrl", () => {
  it("recognizes things that must not be rewritten", () => {
    for (const s of [
      "https://cdn.example/lib-3.1.0.min.js",
      "//cdn.example/x.js",
      "/applet/unified_document_index/asset/u/blobs/a.png",
      "data:image/png;base64,AAAA",
      "#anchor",
    ]) {
      expect(isAbsoluteOrUrl(s), s).toBe(true);
    }
  });
  it("treats renderer-relative paths as rewritable", () => {
    for (const s of ["blobs/a.png", "files/report.pdf", "./x.gif"]) {
      expect(isAbsoluteOrUrl(s), s).toBe(false);
    }
  });
});

describe("assetUrl", () => {
  it("percent-encodes each path segment but keeps the separators", () => {
    expect(assetUrl("u-1", "blobs/a.png")).toBe(
      "/applet/unified_document_index/asset/u-1/blobs/a.png",
    );
    expect(assetUrl("u/1", "a b/c&d.png")).toBe(
      "/applet/unified_document_index/asset/u%2F1/a%20b/c%26d.png",
    );
  });
});
