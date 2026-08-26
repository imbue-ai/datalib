import { describe, expect, it } from "vitest";
import {
  assetUrl,
  isAbsoluteOrUrl,
  rewriteIframeSrcs,
} from "../src/cards/asset_urls";

describe("isAbsoluteOrUrl", () => {
  it("recognizes things that must not be rewritten", () => {
    for (const s of [
      "https://cdn.plot.ly/plotly-3.1.0.min.js",
      "//cdn.example/x.js",
      "/applet/unified_index/asset/u/blobs/a.png",
      "data:image/png;base64,AAAA",
      "#anchor",
    ]) {
      expect(isAbsoluteOrUrl(s), s).toBe(true);
    }
  });
  it("treats renderer-relative paths as rewritable", () => {
    for (const s of ["blobs/a.png", "plots/temperature.html", "./x.gif"]) {
      expect(isAbsoluteOrUrl(s), s).toBe(false);
    }
  });
});

describe("assetUrl", () => {
  it("percent-encodes each path segment but keeps the separators", () => {
    expect(assetUrl("u-1", "plots/temperature.html")).toBe(
      "/applet/unified_index/asset/u-1/plots/temperature.html",
    );
    expect(assetUrl("u/1", "a b/c&d.png")).toBe(
      "/applet/unified_index/asset/u%2F1/a%20b/c%26d.png",
    );
  });
});

describe("rewriteIframeSrcs", () => {
  const uuid = "ea19f544-e204-5348-9d08-3acdf605dc8a";

  it("rewrites the yolink renderer's plot embed", () => {
    // The exact markup `render/render.rs` emits.
    const html =
      '<iframe src="plots/temperature.html" title="Temperature" width="100%" ' +
      'height="520" loading="lazy" sandbox="allow-scripts allow-downloads" ' +
      'style="border:1px solid rgba(128,128,128,.35);border-radius:6px"></iframe>';
    const out = rewriteIframeSrcs(html, uuid);
    expect(out).toContain(`src="/applet/unified_index/asset/${uuid}/plots/temperature.html"`);
    // Every other attribute survives untouched — notably `sandbox`,
    // which is what keeps the frame on an opaque origin.
    expect(out).toContain('sandbox="allow-scripts allow-downloads"');
    expect(out).toContain('title="Temperature"');
  });

  it("rewrites several frames in one chunk", () => {
    const html =
      '<iframe src="plots/a.html"></iframe><iframe src="plots/b.html"></iframe>';
    const out = rewriteIframeSrcs(html, uuid);
    expect(out).toContain(`/applet/unified_index/asset/${uuid}/plots/a.html`);
    expect(out).toContain(`/applet/unified_index/asset/${uuid}/plots/b.html`);
  });

  it("leaves absolute srcs and other elements alone", () => {
    const html =
      '<iframe src="https://example.com/x"></iframe>' +
      '<iframe src="/applet/unified_index/asset/other/x.html"></iframe>' +
      '<video src="clips/a.mp4"></video>';
    expect(rewriteIframeSrcs(html, uuid)).toBe(html);
  });

  it("is a no-op without a markdown uuid", () => {
    const html = '<iframe src="plots/a.html"></iframe>';
    expect(rewriteIframeSrcs(html, null)).toBe(html);
    expect(rewriteIframeSrcs(html, undefined)).toBe(html);
  });
});
