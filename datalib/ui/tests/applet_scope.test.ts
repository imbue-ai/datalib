// The naming rules that make one component store hold everybody's
// components. Both live in pure functions, so they can be pinned
// without loading the UI.
import { describe, expect, it } from "vitest";
import { componentName, parseCardSource } from "../src/cards/cardCall";
import { gallerySource } from "../src/cards/frontendRegistry";

const componentOf = (source: string) => componentName(parseCardSource(source).name);

describe("component namespaces", () => {
  it("sees the namespace a card references", () => {
    expect(componentOf('comp.slack_work.channels("slack_work")')).toEqual({
      ns: "slack_work",
      name: "channels",
    });
  });

  // The component name is a member, so two namespaces can both export
  // `channels` without competing — which is the whole reason the store
  // is namespaced rather than flat.
  it("does not treat a component name as a namespace", () => {
    expect(componentOf("comp.user.channels()")).toEqual({ ns: "user", name: "channels" });
  });

  it("ignores a bare identifier that merely looks like a namespace", () => {
    expect(componentOf("gridView()")).toBeNull();
    expect(componentOf("something.user.x()")).toBeNull();
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

  // A generated call has to survive being read back by the parser, or
  // a gallery pick would produce a card that cannot resolve itself.
  it("produces source the card-source parser reads back", () => {
    const src = gallerySource("slack_work", "channels", ["slack_work", { n: 1 }]);
    expect(parseCardSource(src)).toEqual({
      name: "comp.slack_work.channels",
      args: ["slack_work", { n: 1 }],
    });
  });
});
