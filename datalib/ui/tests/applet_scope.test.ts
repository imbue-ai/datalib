// The naming rules that make one component store hold everybody's
// components. Both live in pure functions, so they can be pinned
// without loading the UI.
import { describe, expect, it } from "vitest";
import {
  gallerySource,
  referencedNamespaces,
} from "../src/cards/frontendRegistry";

describe("component namespaces", () => {
  it("sees the namespace a card references", () => {
    const ns = referencedNamespaces('comp.slack_work.channels("slack_work")');
    expect([...ns]).toEqual(["slack_work"]);
  });

  // Two instances of one applet, and a user component, in one card:
  // every namespace resolves and none shadows another.
  it("sees every namespace a card references", () => {
    const ns = referencedNamespaces(
      'combine(comp.slack_work.channels("a"), comp.slack_personal.channels("b"), comp.user.tetris())',
    );
    expect([...ns].sort()).toEqual(["slack_personal", "slack_work", "user"]);
  });

  // The component name is a member, so two namespaces can both export
  // `channels` without competing — which is the whole reason the store
  // is namespaced rather than flat.
  it("does not treat a component name as a namespace", () => {
    const ns = referencedNamespaces("comp.user.channels()");
    expect([...ns]).toEqual(["user"]);
  });

  it("ignores a bare identifier that merely looks like a namespace", () => {
    expect([...referencedNamespaces("gridView()")]).toEqual([]);
    expect([...referencedNamespaces("something.user.x()")]).toEqual([]);
  });

  it("tolerates whitespace around the dots", () => {
    expect([...referencedNamespaces("comp . user . tetris ()")]).toEqual(["user"]);
  });
});

describe("gallerySource", () => {
  // Arguments are stored as data and spelled into a call here, which is
  // what lets one component appear once per applet instance.
  it("builds a qualified call from stored arguments", () => {
    expect(gallerySource("slack_work", "channels", ["slack_work"])).toBe(
      'comp.slack_work.channels("slack_work")',
    );
    expect(gallerySource("user", "tetris", [])).toBe("comp.user.tetris()");
    expect(gallerySource("user", "tetris")).toBe("comp.user.tetris()");
  });

  it("serializes non-string arguments as JSON", () => {
    expect(gallerySource("ns", "c", [1, true, { a: 2 }, null])).toBe(
      'comp.ns.c(1, true, {"a":2}, null)',
    );
  });

  // A generated call has to survive being read back by the scanner, or
  // a gallery pick would produce a card that cannot resolve itself.
  it("produces source the namespace scanner can read", () => {
    const src = gallerySource("slack_work", "channels", ["slack_work"]);
    expect([...referencedNamespaces(src)]).toEqual(["slack_work"]);
  });
});
