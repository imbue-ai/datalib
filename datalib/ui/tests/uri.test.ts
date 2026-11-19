import { describe, expect, it } from "vitest";
import { parseDoc, parsePhoto, titleSlug, writeDoc, writePhoto, type DocName } from "../src/uri";
import fixtures from "./uri-fixtures.json";

/// The cases the Rust `datalib_uri` crate reads and writes too.
/// A `title` is what the link was written with; Rust reads such a link
/// and writes none.
type Fixture = { name: string; link: string; doc: DocName | null; title?: string; photo?: string };

describe("datalib names", () => {
  for (const f of fixtures as Fixture[]) {
    it(`reads ${f.name}`, () => {
      expect(parseDoc(f.link)).toEqual(f.doc);
    });
    it(`reads ${f.name} as a photo or not`, () => {
      expect(parsePhoto(f.link)).toEqual(f.photo ?? null);
    });
    if (f.doc) {
      const doc = f.doc;
      it(`writes ${f.name}`, () => {
        expect(writeDoc(doc, f.title)).toBe(f.link);
      });
    }
    if (f.photo) {
      const photo = f.photo;
      it(`writes ${f.name}`, () => {
        expect(writePhoto(photo)).toBe(f.link);
      });
    }
  }
});

describe("titleSlug", () => {
  /// A long title stops at a whole word, and one long word is cut.
  it("keeps a slug short", () => {
    const long = "Captain's log stardate 41153 point 7 our destination is planet Deneb IV";
    expect(titleSlug(long)).toBe("Captain-s-log-stardate-41153-point-7-our-destination-is");
    expect(titleSlug("x".repeat(80))).toBe("x".repeat(60));
    expect(titleSlug("")).toBe("");
  });
});
