// The sanitizer's treatment of remote references (issue #648): every
// way a body can make the browser fetch from another host is held
// back by default and put back through the proxy on request.

import { describe, expect, it } from "vitest";

import { decorateRemoteMedia, loadRemoteMedia } from "./remoteMedia";
import { sanitizeRenderedHtml } from "./sanitize";

function dom(html: string): HTMLElement {
  const div = document.createElement("div");
  div.innerHTML = html;
  return div;
}

const PIXEL = "https://pixel.example/open.gif?m=1";
const HERO = "https://cdn.example/hero.jpg";

describe("sanitizeRenderedHtml and remote references", () => {
  it("holds an image's remote src and reports it", () => {
    const { html, remote } = sanitizeRenderedHtml(
      `<p><img src="${HERO}" alt="Hero"><img src="blobs/own.png" alt="own"></p>`,
    );
    const imgs = dom(html).querySelectorAll("img");
    expect(imgs[0].getAttribute("src")).toBeNull();
    expect(imgs[0].getAttribute("data-remote-src")).toBe(HERO);
    expect(imgs[0].classList.contains("remote-blocked")).toBe(true);
    expect(imgs[1].getAttribute("src")).toBe("blobs/own.png");
    expect(imgs[1].classList.contains("remote-blocked")).toBe(false);
    expect(remote).toEqual([{ url: HERO, host: "cdn.example", kind: "image", loaded: false }]);
  });

  it("proxies instead when told the source is trusted", () => {
    const { html, remote } = sanitizeRenderedHtml(`<img src="${HERO}">`, { loadRemote: true });
    expect(dom(html).querySelector("img")!.getAttribute("src")).toBe(
      `/api/remote?url=${encodeURIComponent(HERO)}`,
    );
    expect(remote[0].loaded).toBe(true);
  });

  it("covers every attribute a browser fetches from", () => {
    const body = [
      `<video src="https://v.example/a.mp4" poster="https://v.example/a.jpg" controls></video>`,
      `<audio src="//a.example/x.mp3"></audio>`,
      `<picture><source srcset="https://s.example/a.jpg 1x, blobs/b.jpg 2x"><img src="blobs/c.jpg"></picture>`,
      `<table><tr><td background="https://t.example/bg.png">x</td></tr></table>`,
      `<p style="color: red; background-image: url('https://c.example/bg.png')">y</p>`,
      `<svg><image href="https://i.example/a.svg"></image></svg>`,
    ].join("");
    const { html, remote } = sanitizeRenderedHtml(body);
    const root = dom(html);
    // Nothing left points at a remote host.
    expect(html).not.toMatch(/(?<![\w-])(src|poster|background|href|srcset)="(https?:|\/\/)/);
    expect(html).not.toMatch(/ style="[^"]*url\(["']?(https?:|\/\/)/);
    expect(root.querySelector("p")!.getAttribute("style")).toBe(
      "color: red; background-image: none",
    );
    expect(root.querySelector("source")!.getAttribute("srcset")).toBeNull();
    expect(root.querySelector("td")!.getAttribute("background")).toBeNull();
    expect(remote.map((r) => r.url).sort()).toEqual(
      [
        "https://v.example/a.mp4",
        "https://v.example/a.jpg",
        "//a.example/x.mp3",
        "https://s.example/a.jpg",
        "https://t.example/bg.png",
        "https://c.example/bg.png",
        "https://i.example/a.svg",
      ].sort(),
    );
    expect(remote.find((r) => r.url === "https://v.example/a.mp4")!.kind).toBe("media");
    expect(remote.find((r) => r.url === "https://c.example/bg.png")!.kind).toBe("style");

    // And every one of them comes back proxied under the trusted policy.
    const loaded = sanitizeRenderedHtml(body, { loadRemote: true }).html;
    expect(loaded).not.toMatch(/(?<![\w-])(src|poster|background|href|srcset)="(https?:|\/\/)/);
    expect(loaded).toContain(
      `srcset="/api/remote?url=${encodeURIComponent("https://s.example/a.jpg")} 1x, blobs/b.jpg 2x"`,
    );
    expect(loaded).toContain(
      `background-image: url(&quot;/api/remote?url=${encodeURIComponent("https://c.example/bg.png")}&quot;)`,
    );
  });

  it("does not offer a reference the source itself claimed to have held", () => {
    const { html, remote } = sanitizeRenderedHtml(`<img data-remote-src="${HERO}" alt="x">`);
    expect(dom(html).querySelector("img")!.getAttribute("data-remote-src")).toBeNull();
    expect(remote).toEqual([]);
  });

  it("leaves links, own paths and data URIs alone", () => {
    const body = `<a href="https://x.example/">x</a><img src="data:image/png;base64,AAAA"><img src="/applet/u/asset/a.png">`;
    const { html, remote } = sanitizeRenderedHtml(body);
    expect(remote).toEqual([]);
    expect(html).toContain('href="https://x.example/"');
    expect(html).toContain('src="data:image/png;base64,AAAA"');
  });
});

describe("the placeholders", () => {
  it("name the host, call a 1×1 a tracking pixel, and load on request", () => {
    const { html } = sanitizeRenderedHtml(
      `<p><img src="${HERO}" alt="Hero"></p><img src="${PIXEL}" width="1" height="1" alt="">`,
    );
    const root = dom(html);
    decorateRemoteMedia(root);
    decorateRemoteMedia(root);
    const chips = root.querySelectorAll("button.remote-media");
    expect(chips).toHaveLength(2);
    expect(chips[0].textContent).toBe("🖼cdn.exampleHero");
    expect(chips[0].getAttribute("title")).toContain(HERO);
    expect(chips[1].textContent).toBe("🖼pixel.exampletracking pixel");
    expect(chips[1].classList.contains("remote-media--pixel")).toBe(true);

    expect(loadRemoteMedia(root, (u) => u === PIXEL)).toEqual([PIXEL]);
    expect(root.querySelectorAll("button.remote-media")).toHaveLength(1);
    const pixel = root.querySelectorAll("img")[1];
    expect(pixel.getAttribute("src")).toBe(`/api/remote?url=${encodeURIComponent(PIXEL)}`);
    expect(pixel.classList.contains("remote-blocked")).toBe(false);
    expect(root.querySelectorAll("img")[0].getAttribute("src")).toBeNull();

    expect(loadRemoteMedia(root, () => true)).toEqual([HERO]);
    expect(root.querySelectorAll("button.remote-media")).toHaveLength(0);
    expect(root.querySelectorAll(".remote-blocked")).toHaveLength(0);
  });
});
