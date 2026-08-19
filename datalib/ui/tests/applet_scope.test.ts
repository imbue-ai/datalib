// The naming rule that makes two instances of one applet coexist:
// an applet id is a name in card scope, and a component name is a
// *member* of it. These tests pin the two properties that rule rests
// on, both of which live in pure functions.
import { describe, expect, it } from "vitest";
import { referencedIdentifiers } from "../src/cards/identifiers";
import { referencedApplets } from "../src/cards/appletRegistry";

describe("applet scoping", () => {
  // If the identifier scan counted `channels` as a free name, two
  // applets exporting a component of the same name would collide in
  // the injected scope — which is exactly what scoping by id avoids.
  it("sees the applet id but not the component name", () => {
    const ids = referencedIdentifiers('slack_work.channels("slack_work")');
    expect(ids.has("slack_work")).toBe(true);
    expect(ids.has("channels")).toBe(false);
  });

  it("resolves only the applets a card actually mentions", () => {
    const configured = ["slack_work", "slack_personal", "grid"];
    const referenced = referencedIdentifiers('slack_work.channels("slack_work")');
    expect(referencedApplets(configured, referenced)).toEqual(["slack_work"]);
  });

  // Two instances of one command in one card: both namespaces resolve,
  // and neither shadows the other.
  it("resolves both instances when a card uses both", () => {
    const configured = ["slack_work", "slack_personal"];
    const referenced = referencedIdentifiers(
      'combine(slack_work.channels("slack_work"), slack_personal.channels("slack_personal"))',
    );
    expect(referencedApplets(configured, referenced).sort()).toEqual([
      "slack_personal",
      "slack_work",
    ]);
  });

  it("ignores an applet id that appears only as a member", () => {
    const configured = ["grid"];
    const referenced = referencedIdentifiers("something.grid()");
    expect(referencedApplets(configured, referenced)).toEqual([]);
  });

  it("resolves nothing when no applets are configured", () => {
    const referenced = referencedIdentifiers('slack_work.channels("x")');
    expect(referencedApplets([], referenced)).toEqual([]);
  });
});
