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
      // A chip link's two extra schemes and its title. Its marks come
      // from the renderer alone (below).
      'href="slack://user?team=T1&amp;id=U2"',
      'title="Data (slack:T1/U2)"',
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

/// A link in a body is the person's to click, and the app decides where
/// it goes (`DocCard.ce.vue` `onFrameLink`). What the sanitizer keeps is
/// the link a renderer writes, `<a href>`, in the schemes the app opens.
describe("a body's links", () => {
  it("keep a target only when it opens a new window", () => {
    const out = renderDocument(
      ["_blank", "_top", "_parent", "_self", "named", "_BLANK"]
        .map((t) => `<a href="https://x.example/${t}" target="${t}">${t}</a>`)
        .join("") +
        `<svg><a xlink:href="https://x.example/svg" target="_top"><text>s</text></a></svg>`,
    ).html;
    const targets = Array.from(dom(out).querySelectorAll("a"), (a) => a.getAttribute("target"));
    expect(targets).toEqual(["_blank", null, null, null, null, null, null]);
  });

  it("are never an image map or MathML", () => {
    const out = renderDocument(
      `<map name="m"><area shape="rect" coords="0,0,9,9" href="https://x.example/a" target="_top"></map>` +
        `<img usemap="#m" src="data:,">` +
        `<math href="https://x.example/m"><mi href="https://x.example/mi">x</mi></math>`,
    ).html;
    expect(out).not.toContain("x.example");
    expect(out).not.toMatch(/<(area|map|math|mi)\b/);
  });

  it("are only in the schemes the app opens", () => {
    const kept = [
      "https://x.example/",
      "http://x.example/",
      "mailto:a@x.example",
      "tel:+15550123456",
    ];
    const chips = ["slack://user?team=T1&id=U2", "datalib:group/slack"];
    const dropped = [
      "sms:+15550123456",
      "callto:picard",
      "cid:part1@x.example",
      "xmpp:picard@x.example",
      "matrix:u/picard:x.example",
      "ftp://x.example/",
      "facetime:picard@x.example",
      "file:///etc/passwd",
    ];
    for (const href of [...kept, ...chips]) {
      const a = dom(renderDocument(`<a href="${href}">x</a>`).html).querySelector("a")!;
      expect(a.getAttribute("href"), href).toBe(href);
    }
    for (const href of dropped) {
      const a = dom(renderDocument(`<a href="${href}">x</a>`).html).querySelector("a")!;
      expect(a.getAttribute("href"), href).toBeNull();
    }
  });
});

/// A chip is a link the app resolves to a person, a group or a step, and
/// only the app's own renderer marks one (`chipLinks.js`). A sender's
/// HTML claiming to be a chip would draw as someone it is not.
describe("a chip's marks", () => {
  it("come from the renderer, never from the body's own HTML", () => {
    const html = renderDocument(
      `[Picard](mailto:picard@enterprise.example)\n\n` +
        `<a class="chip x" data-handle="email:picard@enterprise.example" href="https://x.example/">Picard</a>` +
        `<a class="chip" data-entity="datalib:group/mail" href="https://x.example/">Mail</a>`,
    ).html;
    const links = Array.from(dom(html).querySelectorAll("a"));
    expect(
      links.map((a) => [a.className, a.dataset.handle ?? null, a.dataset.entity ?? null]),
    ).toEqual([
      ["chip", "email:picard@enterprise.example", null],
      ["x", null, null],
      ["", null, null],
    ]);
    expect(html).not.toContain("data-chip-mark");
  });
});

/// What a style can load, read the way a browser reads it: an escape is
/// the letter it stands for, a quoted URL may hold a `)`, and
/// `image-set()` takes a bare string as a URL.
describe("a style's hidden references", () => {
  it("load nothing, however the URL is spelled", () => {
    for (const style of [
      `background:url("https://track.example/a)b")`,
      `background:url('https://track.example/a)b')`,
      `background-image:image-set("https://track.example/a.png" 1x)`,
      `background-image:-webkit-image-set("https://track.example/a.png" 1x)`,
      `background:\\75 rl(https://track.example/a.png)`,
      `background:u\\72 l(https://track.example/a.png)`,
      `background:url(https\\3a //track.example/a.png)`,
      `background:url(/api/manage/rows\\3f refresh=1)`,
      `background:\\75 rl(/api/manage/rows?refresh=1)`,
      `background:image-set("/api/manage/rows?refresh=1" 1x)`,
      `list-style:url("/api/config")`,
    ]) {
      const html = renderDocument(`<div style='${style}'>x</div>`).html;
      const kept = dom(html).querySelector("div")!.getAttribute("style") ?? "";
      expect(kept, style).not.toMatch(/track\.example|\/api\/|\\|image-set/i);
    }
  });

  it("keep what draws nothing from elsewhere", () => {
    const html = renderDocument(`<p style="color: red; padding: 2px">x</p>`).html;
    const style = dom(html).querySelector("p")!.getAttribute("style") ?? "";
    expect(style).toMatch(/color: red/);
    expect(style).toMatch(/padding: 2px/);
  });
});

/// An SVG filter's image is a fetch like an `<img>`'s.
describe("an SVG filter's image", () => {
  it("is held like any other remote image, and loads nothing from our origin", () => {
    const { html, remote } = renderDocument(
      `<svg><filter id="f"><feImage href="https://track.example/a.png"/>` +
        `<feImage xlink:href="/api/manage/rows?refresh=1"/></filter></svg>`,
    );
    const images = Array.from(dom(html).querySelectorAll("feImage"));
    expect(images).toHaveLength(2);
    for (const el of images) {
      expect(el.getAttribute("href")).toBeNull();
      expect(el.getAttribute("xlink:href")).toBeNull();
    }
    expect(remote.map((r) => r.url)).toEqual(["https://track.example/a.png"]);
  });
});

/// An SVG attribute like `fill` reads as CSS, so a `url(…)` in it is a
/// fetch, unless it names a part of the body itself.
describe("an SVG shape's paint", () => {
  it("names only a part of the body", () => {
    const html = renderDocument(
      `<svg><rect id="a" fill="url(#grad)" stroke="url('#g2')"/>` +
        `<rect id="b" fill="url(/api/manage/rows?refresh=1#x)" filter="url(https://track.example/f.svg#f)"` +
        ` mask="\\75 rl(/api/config#m)" stroke="red"/></svg>`,
    ).html;
    const root = dom(html);
    const attrs = (id: string) => {
      const el = root.querySelector(`#${id}`)!;
      return Object.fromEntries(el.getAttributeNames().map((n) => [n, el.getAttribute(n)]));
    };
    expect(attrs("a")).toEqual({ id: "a", fill: "url(#grad)", stroke: "url('#g2')" });
    expect(attrs("b")).toEqual({ id: "b", stroke: "red" });
  });
});

function dom(html: string): HTMLElement {
  const root = document.createElement("div");
  root.innerHTML = html;
  return root;
}
