// What an applet's Browse on the Manage screen opens: the contacts app's
// own table, and nothing for an applet whose rows are not its own.

import { describe, expect, it } from "vitest";

import { appletBrowseSource } from "../src/cards/cardSources";

describe("appletBrowseSource", () => {
  it("opens the contacts app's table, and nothing for another applet", () => {
    expect(appletBrowseSource("datalib_contacts")).toBe("contactsView()");
    expect(appletBrowseSource("unified_index")).toBeNull();
  });
});
