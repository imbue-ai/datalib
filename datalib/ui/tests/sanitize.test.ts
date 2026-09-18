import { describe, expect, it } from "vitest";
import { sanitizeRenderedHtml } from "../src/cards/sanitize";

/// The page holds the API session, so a message body that runs script is
/// a message that owns the user's data. These are the shapes that must
/// not survive, and the renderer vocabulary that must.
describe("sanitizeRenderedHtml", () => {
  it("strips what could run", () => {
    const out = sanitizeRenderedHtml(
      `<div class="msg"><script>alert(1)</script>` +
        `<img src=x onerror="alert(1)">` +
        `<a href="javascript:alert(1)">x</a>` +
        `<iframe src="https://evil.example/"></iframe>` +
        `<iframe srcdoc="<script>alert(1)</script>"></iframe>` +
        `<svg><script>alert(1)</script></svg>` +
        `<form action="/api/config"><input name="text"></form></div>`,
    );
    expect(out).not.toContain("<script");
    expect(out).not.toContain("onerror");
    expect(out).not.toContain("javascript:");
    expect(out).not.toContain("evil.example");
    expect(out).not.toContain("srcdoc");
    expect(out).not.toContain("<form");
  });

  it("keeps the section wrappers and their ids", () => {
    const wrapper =
      `<div id="m-abc" data-section-uuid="abc" class="msg msg--slack">` +
      `<h2>Picard</h2><p>Engage.</p></div>`;
    expect(sanitizeRenderedHtml(wrapper)).toBe(wrapper);
  });

  it("keeps the rest of the renderer vocabulary", () => {
    const md =
      `<details open><summary>3 tool calls</summary><p>x</p></details>` +
      `<time class="msg-ts" datetime="2364-04-11T00:00:00+00:00" title="full">short</time>` +
      `<audio controls src="/applet/unified_index/asset/u/blobs/a.m4a"></audio>` +
      `<video controls src="/applet/unified_index/asset/u/blobs/a.mp4"></video>` +
      `<a class="source-link" href="https://example.com/x" target="_blank" rel="noopener noreferrer">↗</a>` +
      `<h1 class="page-title" data-page-title-uuid="p1">Title</h1>` +
      `<table><tr><th>a</th><td>b</td></tr></table>` +
      `<img src="/applet/unified_index/asset/u/blobs/a.png" alt="a" loading="lazy">`;
    const out = sanitizeRenderedHtml(md);
    for (const keep of [
      '<details open="">',
      "<summary>",
      'datetime="2364-04-11T00:00:00+00:00"',
      "<audio controls",
      "<video controls",
      'target="_blank"',
      'rel="noopener noreferrer"',
      'data-page-title-uuid="p1"',
      "<table>",
      'loading="lazy"',
    ]) {
      expect(out, keep).toContain(keep);
    }
  });

  it("keeps a diff document's wrappers and markers", () => {
    const md =
      `<div class="diff-modified"><div id="m-a" data-section-uuid="a" class="msg msg--contacts">` +
      `<p>Make it <del>so.</del><ins>so, Number One.</ins></p></div></div>` +
      `<div class="diff-removed"><p>gone</p></div><div class="diff-added"><p>new</p></div>`;
    expect(sanitizeRenderedHtml(md)).toBe(md);
  });

  it("keeps an iframe only when its src is one of our own paths", () => {
    const own = sanitizeRenderedHtml(
      `<iframe src="/applet/unified_index/asset/u/plots/t.html" title="t" width="100%" height="520" style="border:1px solid gray"></iframe>`,
    );
    expect(own).toContain('src="/applet/unified_index/asset/u/plots/t.html"');
    expect(own).toContain('height="520"');
    const relative = sanitizeRenderedHtml(`<iframe src="plots/t.html"></iframe>`);
    expect(relative).toContain('src="plots/t.html"');
    for (const foreign of [
      "https://evil.example/",
      "//evil.example/",
      "javascript:alert(1)",
      "data:text/html,<script>alert(1)</script>",
    ]) {
      const out = sanitizeRenderedHtml(`<iframe src="${foreign}"></iframe>`);
      expect(out, foreign).not.toContain("src=");
    }
  });
});
