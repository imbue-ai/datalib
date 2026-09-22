// The sanitizer's treatment of remote references (issue #648): every
// way a body can make the browser fetch from another host is held
// back unless the caller says the server let it through — then it
// goes through the server, carrying what the body is — and the
// placeholders say what was held.

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
    expect(remote).toEqual([{ url: HERO, host: "cdn.example", kind: "image", loaded: false }]);
  });

  it("lets an accepted reference through, proxied with what the body is", () => {
    const { html, remote } = sanitizeRenderedHtml(`<img src="${HERO}"><img src="${PIXEL}">`, {
      accept: (u) => u === HERO,
      context: { document: "doc-1", source: "mail" },
    });
    const imgs = dom(html).querySelectorAll("img");
    expect(imgs[0].getAttribute("src")).toBe(
      `/api/remote_media?url=${encodeURIComponent(HERO)}&document=doc-1&source=mail`,
    );
    expect(imgs[0].classList.contains("remote-blocked")).toBe(false);
    expect(imgs[1].getAttribute("src")).toBeNull();
    expect(remote.map((r) => r.loaded)).toEqual([true, false]);
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

    // And every one of them comes back proxied when accepted.
    const loaded = sanitizeRenderedHtml(body, { accept: () => true }).html;
    expect(loaded).not.toMatch(/(?<![\w-])(src|poster|background|href|srcset)="(https?:|\/\/)/);
    expect(loaded).toContain(
      `srcset="/api/remote_media?url=${encodeURIComponent("https://s.example/a.jpg")} 1x, blobs/b.jpg 2x"`,
    );
    expect(loaded).toContain(
      `background-image: url(&quot;/api/remote_media?url=${encodeURIComponent("https://c.example/bg.png")}&quot;)`,
    );
    expect(loaded).toContain(
      `src="/api/remote_media?url=${encodeURIComponent("https://a.example/x.mp3")}"`,
    );
  });

  it("holds a srcset or a style whole unless every URL in it is accepted", () => {
    const body = `<img srcset="https://s.example/a.jpg 1x, https://t.example/b.jpg 2x"><p style="background: url(https://s.example/x.png), url(https://t.example/y.png)">z</p>`;
    const { html, remote } = sanitizeRenderedHtml(body, {
      accept: (u) => u.startsWith("https://s.example/"),
    });
    expect(html).not.toContain("/api/remote_media");
    expect(remote.every((r) => !r.loaded)).toBe(true);
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
    const chips = root.querySelectorAll("button.remote-media");
    expect(chips).toHaveLength(2);
    expect(chips[0].textContent).toBe("🖼cdn.exampleHero");
    expect(chips[0].getAttribute("title")).toContain(HERO);
    expect((chips[0] as HTMLElement).dataset.remoteUrl).toBe(HERO);
    expect(chips[1].textContent).toBe("🖼pixel.exampletracking pixel");
    expect(chips[1].classList.contains("remote-media--pixel")).toBe(true);
  });
});
