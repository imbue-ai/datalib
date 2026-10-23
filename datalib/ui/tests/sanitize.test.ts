import { describe, expect, it } from "vitest";
import { renderDocument } from "../src/cards/renderDocument";
import { sanitizeRenderedHtml as sanitize } from "../src/cards/sanitize";

const sanitizeRenderedHtml = (html: string) => sanitize(html).html;

/** An attachment at its blob-store address: what a body may load from our origin. */
const OWN = `/data/mail/ingest/blob/${"ab".repeat(32)}`;

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
      `<audio controls src="${OWN}"></audio>` +
      `<video controls src="${OWN}"></video>` +
      `<a class="source-link" href="https://example.com/x" target="_blank" rel="noopener noreferrer">↗</a>` +
      `<a class="chip" href="slack://user?team=T1&amp;id=U2" title="Data (slack:T1/U2)" data-handle="slack:T1/U2">Data</a>` +
      `<a class="chip" href="datalib:group/slack">Slack</a>` +
      `<a href="tel:+15550123456">call</a>` +
      `<a href="gopher://old.example/">x</a>` +
      `<h1 class="page-title" data-page-title-uuid="p1">Title</h1>` +
      `<table><tr><th>a</th><td>b</td></tr></table>` +
      `<img src="${OWN}" alt="a" loading="lazy">`;
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
      // A chip link's two extra schemes, its title and its handle.
      'href="slack://user?team=T1&amp;id=U2"',
      'title="Data (slack:T1/U2)"',
      'data-handle="slack:T1/U2"',
      'href="datalib:group/slack"',
      'href="tel:+15550123456"',
    ]) {
      expect(out, keep).toContain(keep);
    }
    expect(out).not.toContain("gopher:");
  });

  it("keeps a diff document's wrappers and markers", () => {
    const md =
      `<div class="diff-modified"><div id="m-a" data-section-uuid="a" class="msg msg--contacts">` +
      `<p>Make it <del>so.</del><ins>so, Number One.</ins></p></div></div>` +
      `<div class="diff-removed"><p>gone</p></div><div class="diff-added"><p>new</p></div>`;
    expect(sanitizeRenderedHtml(md)).toBe(md);
  });

  // An `.html` a sender attached is a page on our origin; framed from a
  // message body it would run inside the app's chrome (audit 2026-10-02
  // finding 1).
  // No renderer frames anything, so no iframe survives.
  it("drops every iframe", () => {
    for (const src of [
      "blobs/0123456789abcdef.html",
      OWN,
      "plots/t.html",
      "https://evil.example/",
      "javascript:alert(1)",
    ]) {
      const out = sanitizeRenderedHtml(`<iframe src="${src}"></iframe>`);
      expect(out, src).not.toContain("iframe");
    }
  });

  it("keeps a chart's ids for the document view to draw", () => {
    const chart = `<div class="time-series-chart" data-time-series="a-1 b-2"></div>`;
    expect(sanitizeRenderedHtml(chart)).toBe(chart);
  });
});

/// Every URL in the rendered body the browser would fetch on its own,
/// as it would resolve it against the app's page: path and query.
function fetchedOnOpen(html: string): string[] {
  const root = document.createElement("div");
  root.innerHTML = html;
  const out: string[] = [];
  const add = (raw: string) => {
    const url = new URL(raw.trim(), location.href);
    if (url.protocol === "data:" || url.protocol === "blob:") return;
    out.push(url.origin === location.origin ? url.pathname + url.search : url.href);
  };
  for (const el of Array.from(root.querySelectorAll("*"))) {
    for (const attr of ["src", "poster", "background"]) {
      const v = el.getAttribute(attr);
      if (v) add(v);
    }
    for (const c of (el.getAttribute("srcset") ?? "").split(",")) {
      const u = c.trim().split(/\s+/)[0];
      if (u) add(u);
    }
    for (const m of (el.getAttribute("style") ?? "").matchAll(/url\(\s*['"]?([^'")]*)/gi))
      add(m[1]);
  }
  return out;
}

/// A body is whatever the sender wrote, and the frame it is drawn in
/// may load from our own origin with the person's cookie. Only an
/// attachment's blob-store address may load from there: anything else is
/// a GET the sender chose, sent the moment the document opens.
describe("a body's references to our own origin", () => {
  it("load nothing but a blob-store address", () => {
    for (const url of [
      "/api/manage/rows?refresh=1",
      "api/manage/rows?refresh=1",
      "../api/config",
      "blobs/a.png",
      "/applet/unified_document_index/asset/md-1/blobs/a.png",
      `/data/mail/ingest/blob/${"ab".repeat(32)}/../../../../api/config`,
      `/data/mail/ingest/blob/%2e%2e/%2e%2e/%2e%2e/api/config`,
      "/api/remote_media?url=https%3A%2F%2Fpixel.example%2Fa.gif&source=other",
      "/applet/unified_document_index/problems",
    ]) {
      const body =
        `<img src="${url}"><video poster="${url}"></video><audio src="${url}"></audio>` +
        `<img srcset="${url} 2x"><div style="background:url('${url}')"></div>`;
      expect(fetchedOnOpen(renderDocument(body).html), url).toEqual([]);
    }
  });

  it("keep a blob-store address, and a part of a stored email", () => {
    const blob = `/data/mail/ingest/blob/${"ab".repeat(32)}`;
    const part = `${blob}/part/${"cd".repeat(32)}`;
    const html = renderDocument(`<img src="${blob}"><audio controls src="${part}"></audio>`).html;
    expect(fetchedOnOpen(html)).toEqual([blob, part]);
  });
});
