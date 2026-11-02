import { describe, expect, it } from "vitest";
import { parseDoc, parsePhoto, writeDoc, writePhoto, type DocName } from "../src/uri";
import fixtures from "./uri-fixtures.json";

/// The cases the Rust `datalib_uri` crate reads and writes too.
type Fixture = { name: string; link: string; doc: DocName | null; photo?: string };

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
        expect(writeDoc(doc)).toBe(f.link);
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
