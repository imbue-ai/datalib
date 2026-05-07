import { describe, it, expect } from "vitest";
import { parse, toHash, toDeeplink, type Route } from "../src/router/deeplink";
import fixtures from "./deeplink-fixtures.json";

interface Fixture {
  name: string;
  deeplink: string;
  hash: string;
  route: Route;
}

describe("deeplink", () => {
  for (const f of fixtures as Fixture[]) {
    it(`parses ${f.name} from deeplink`, () => {
      expect(parse(f.deeplink)).toEqual(f.route);
    });
    it(`parses ${f.name} from hash`, () => {
      expect(parse("#" + f.hash)).toEqual(f.route);
    });
    it(`renders ${f.name} hash`, () => {
      expect(toHash(f.route)).toBe(f.hash);
    });
    it(`renders ${f.name} deeplink`, () => {
      expect(toDeeplink(f.route)).toBe(f.deeplink);
    });
  }

  it("rejects empty url", () => {
    expect(() => parse("#")).toThrow();
  });
  it("rejects unknown route", () => {
    expect(() => parse("#bogus")).toThrow();
  });
  it("rejects chat without uuid", () => {
    expect(() => parse("#chat/")).toThrow();
  });
});
