import { describe, expect, it } from "vitest";
import {
  parseDoc,
  parseNode,
  parsePhoto,
  splitLibrary,
  titleSlug,
  withLibrary,
  writeDoc,
  writeNode,
  writePhoto,
  type DocName,
} from "../src/uri";
import fixtures from "./uri-fixtures.json";

/// The cases the Rust `datalib_uri` crate reads and writes too.
/// A `title` is what the link was written with; Rust reads such a link
/// and writes none.
type Fixture = {
  name: string;
  link: string;
  doc: DocName | null;
  title?: string;
  photo?: string;
  node?: string;
  library?: string;
};

describe("datalib names", () => {
  for (const f of fixtures as Fixture[]) {
    const split = splitLibrary(f.link);
    const name = split?.name ?? "";
    it(`reads ${f.name}`, () => {
      expect(parseDoc(name)).toEqual(f.doc);
    });
    it(`reads ${f.name} as a photo or not`, () => {
      expect(parsePhoto(name)).toEqual(f.photo ?? null);
    });
    it(`reads ${f.name} as a node or not`, () => {
      expect(parseNode(name)).toEqual(f.node ?? null);
    });
    if (f.doc || f.photo || f.node) {
      it(`reads ${f.name}'s library`, () => {
        expect(split?.library ?? null).toEqual(f.library ?? null);
      });
    }
    const inLibrary = (written: string) => (f.library ? withLibrary(written, f.library) : written);
    if (f.doc) {
      const doc = f.doc;
      it(`writes ${f.name}`, () => {
        expect(inLibrary(writeDoc(doc, f.title))).toBe(f.link);
      });
    }
    if (f.photo) {
      const photo = f.photo;
      it(`writes ${f.name}`, () => {
        expect(inLibrary(writePhoto(photo))).toBe(f.link);
      });
    }
    if (f.node) {
      const node = f.node;
      it(`writes ${f.name}`, () => {
        expect(inLibrary(writeNode(node))).toBe(f.link);
      });
    }
  }

  /// A reader that has not split the library off refuses the link
  /// rather than reading the library's uuid as the page's.
  it("a name reader refuses a link that carries a library", () => {
    const link = withLibrary(writeDoc({ page: PAGE, section: null }), LIB);
    expect(parseDoc(link)).toBeNull();
    expect(parseNode(withLibrary(writeNode(PAGE), LIB))).toBeNull();
  });
});

const PAGE = "00000000-0000-8000-a000-000000000001";
const LIB = "9b1d0000-0000-4000-8000-0000000000aa";

describe("titleSlug", () => {
  /// A long title stops at a whole word, and one long word is cut.
  it("keeps a slug short", () => {
    const long = "Captain's log stardate 41153 point 7 our destination is planet Deneb IV";
    expect(titleSlug(long)).toBe("Captain-s-log-stardate-41153-point-7-our-destination-is");
    expect(titleSlug("x".repeat(80))).toBe("x".repeat(60));
    expect(titleSlug("")).toBe("");
  });
});
