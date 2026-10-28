import { describe, expect, it } from "vitest";
import { parseDoc, writeDoc, type DocName } from "../src/uri";
import fixtures from "./uri-fixtures.json";

/// The cases the Rust `datalib_uri` crate reads and writes too.
type Fixture = { name: string; link: string; doc: DocName | null };

describe("datalib names", () => {
  for (const f of fixtures as Fixture[]) {
    it(`reads ${f.name}`, () => {
      expect(parseDoc(f.link)).toEqual(f.doc);
    });
    if (f.doc) {
      const doc = f.doc;
      it(`writes ${f.name}`, () => {
        expect(writeDoc(doc)).toBe(f.link);
      });
    }
  }
});
