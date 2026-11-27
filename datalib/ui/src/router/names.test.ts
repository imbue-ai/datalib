import { describe, expect, it } from "vitest";
import { linkOfPath, pathOfLink, routeOfLink } from "./names";

const PAGE = "00000000-0000-8000-a000-000000000001";
const SECTION = "0190a5e1-0000-7000-8000-00000000000c";
const NODE = "00000000-0000-8000-a000-000000000003";
const HERE = "9b1d0000-0000-4000-8000-0000000000aa";
const THERE = "9b1d0000-0000-4000-8000-0000000000bb";

describe("the fixed address of a name", () => {
  it("is the link after /name/, query and fragment kept", () => {
    const link = `datalib:doc/${PAGE}?library=${HERE}#${SECTION}`;
    const path = pathOfLink(link);
    expect(path).toBe(`/name/doc/${PAGE}?library=${HERE}#${SECTION}`);
    expect(linkOfPath(path!)).toBe(link);
  });

  it("is no other path", () => {
    expect(linkOfPath("/chat/abc")).toBeNull();
    expect(linkOfPath("/names/doc/x")).toBeNull();
    expect(pathOfLink("mailto:riker@enterprise.org")).toBeNull();
  });
});

describe("what a link opens", () => {
  it("opens a page at its section in the library it names", () => {
    expect(routeOfLink(`datalib:doc/${PAGE}?library=${HERE}#${SECTION}`, HERE)).toEqual({
      open: { kind: "doc", page: PAGE, section: SECTION },
    });
  });

  it("opens a name with no library in this one", () => {
    expect(routeOfLink(`datalib:node/${NODE}`, HERE)).toEqual({
      open: { kind: "node", id: NODE },
    });
  });

  /// The id is compared whatever its case: a hand-typed link may shout.
  it("reads the library's id in either case", () => {
    const link = `datalib:node/${NODE}?library=${HERE.toUpperCase()}`;
    expect(routeOfLink(link, HERE)).toEqual({ open: { kind: "node", id: NODE } });
  });

  it("refuses a link for another library rather than opening it here", () => {
    const route = routeOfLink(`datalib:node/${NODE}?library=${THERE}`, HERE);
    expect(route).toHaveProperty("refused");
  });

  /// Until the server says which library this is, a link that names one
  /// cannot be said to be for it.
  it("refuses a link that names a library while this one is unknown", () => {
    expect(routeOfLink(`datalib:node/${NODE}?library=${HERE}`, null)).toHaveProperty("refused");
  });

  it("refuses a kind this build does not know", () => {
    expect(routeOfLink("datalib:ship/enterprise", HERE)).toHaveProperty("refused");
    expect(routeOfLink(`datalib:node/${NODE}?x=1`, HERE)).toHaveProperty("refused");
  });
});
