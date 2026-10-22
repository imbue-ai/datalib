import { describe, expect, it } from "vitest";
import { blobAddress } from "../src/cards/blob_address";
import { renderDocument } from "../src/cards/renderDocument";

describe("an attachment's address", () => {
  // A renderer links an attachment its source's blob store holds at
  // `/data/<group>/<function>/blob/<hash>`, which the server answers
  // itself: the page must load it as written, from this origin.
  const address = `/data/mail/ingest/blob/${"ab".repeat(32)}`;
  it("reaches the page unchanged, as an image, a link and a player", () => {
    const md = `![scan](${address})\n\n[report](${address})\n\n<audio controls src="${address}"></audio>\n`;
    const div = document.createElement("div");
    div.innerHTML = renderDocument(md).html;
    expect(div.querySelector("img")?.getAttribute("src")).toBe(address);
    expect(div.querySelector("a")?.getAttribute("href")).toBe(address);
    expect(div.querySelector("audio")?.getAttribute("src")).toBe(address);
  });
});

describe("blobAddress", () => {
  const hash = "ab".repeat(32);
  it("keeps exactly an attachment's address, and nothing else under /data/", () => {
    expect(blobAddress(`/data/mail/ingest/blob/${hash}`)).toBe(`/data/mail/ingest/blob/${hash}`);
    for (const s of [
      `/data/mail/ingest/blob/${hash}/extra`,
      `/data/mail/blob/${hash}`,
      `/data/mail/ingest/blob/${hash.toUpperCase()}`,
      "/data/../api/config",
      `/data/%2e%2e/%2e%2e/api/blob/${hash}`,
      "/data/mail/ingest/other/x",
    ]) {
      expect(blobAddress(s), s).toBeNull();
    }
  });

  it("keeps exactly a part of a stored email, and nothing longer or shorter", () => {
    const part = "cd".repeat(32);
    const address = `/data/mail/ingest/blob/${hash}/part/${part}`;
    expect(blobAddress(address)).toBe(address);
    for (const s of [
      `/data/mail/ingest/blob/${hash}/part`,
      `/data/mail/ingest/blob/${hash}/part/`,
      `/data/mail/ingest/blob/${hash}/parts/${part}`,
      `/data/mail/ingest/blob/${hash}/part/${part}/`,
      `/data/mail/ingest/blob/${hash}/part/${part}/part/${part}`,
      `/data/mail/ingest/blob/${hash}/part/${part.toUpperCase()}`,
      `/data/mail/ingest/blob/${hash}/part/${part.slice(1)}`,
      `/data/mail/ingest/blob/${hash}/part/%2e%2e/%2e%2e/%2e%2e/%2e%2e/api/config`,
    ]) {
      expect(blobAddress(s), s).toBeNull();
    }
  });
});

describe("a reference that is not a blob-store address", () => {
  // A copy beside a page (`blobs/a.png`) and the route that served one
  // name nothing the server answers, so a body naming either loads
  // nothing.
  it("is dropped, relative or not", () => {
    for (const s of [
      "blobs/a.png",
      "./a.png",
      "../../api/config",
      "/applet/unified_document_index/asset/u/blobs/a.png",
    ]) {
      expect(blobAddress(s), s).toBeNull();
    }
    const div = document.createElement("div");
    div.innerHTML = renderDocument("![old](blobs/a.png)\n").html;
    expect(div.querySelector("img")?.getAttribute("src") ?? null).toBeNull();
  });
});
