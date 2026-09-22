// The sanitizer's treatment of remote references (issue #648): every
// way a body can make the browser fetch from another host is held
// back, and the placeholders say what was held.

import { describe, expect, it } from "vitest";

import { decorateRemoteMedia } from "./remoteMedia";
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
    expect(remote).toEqual([{ url: HERO, host: "cdn.example", kind: "image" }]);
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
    expect(remote.find((r) => r.url === "//a.example/x.mp3")!.host).toBe("a.example");
  });

  it("does not show a reference the source itself claimed to have held", () => {
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
  it("name the host, keep the URL on hover, and call a 1×1 a tracking pixel", () => {
    const { html } = sanitizeRenderedHtml(
      `<p><img src="${HERO}" alt="Hero"></p><img src="${PIXEL}" width="1" height="1" alt="">`,
    );
    const root = dom(html);
    decorateRemoteMedia(root);
    decorateRemoteMedia(root);
    const chips = root.querySelectorAll(".remote-media");
    expect(chips).toHaveLength(2);
    expect(chips[0].textContent).toBe("🖼cdn.exampleHero");
    expect(chips[0].getAttribute("title")).toBe(HERO);
    expect(chips[1].textContent).toBe("🖼pixel.exampletracking pixel");
    expect(chips[1].classList.contains("remote-media--pixel")).toBe(true);
  });
});
