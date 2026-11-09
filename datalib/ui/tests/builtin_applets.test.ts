// The apps the "Add data source" picker turns on and off, and the one a
// Lightroom form offers: each is one `[[applets]]` entry, added once and
// taken out whole.
import { describe, expect, it } from "vitest";
import {
  BUILTIN_APPLETS,
  CONTACTS_APP,
  PHOTOS_APP,
  builtinApplet,
  configuredAppletIds,
  filterApps,
  setApplet,
} from "../src/config/builtinApplets";
import { CATALOG } from "../src/config/catalog";
import { listSteps } from "../src/config/sourceSteps";

const BASE = `[[groups]]
id = "unified_document_index"

[[steps]]
group = "unified_document_index"
function = "grid_index"
inputs = []

[[applets]]
group = "unified_document_index"
id = "unified_document_index"
command = "datalib-applet unified_document_index"
`;

describe("setApplet", () => {
  it("adds the entry once, running the applet of the same name", () => {
    const on = setApplet(BASE, PHOTOS_APP, true);
    expect(configuredAppletIds(on)).toEqual(new Set(["unified_document_index", "photos"]));
    expect(on).toContain('[[applets]]\nid = "photos"\ncommand = "datalib-applet photos"');
    expect(setApplet(on, PHOTOS_APP, true)).toBe(on);
  });

  it("files the contacts app under the document index, as the starter config does", () => {
    const on = setApplet(BASE, CONTACTS_APP, true);
    const entry = listSteps(on).find((s) => s.id === "datalib_contacts");
    expect(entry?.group).toBe("unified_document_index");
    expect(entry?.type).toBe("datalib_contacts");
  });

  it("takes out exactly what turning it on put in", () => {
    const on = setApplet(BASE, CONTACTS_APP, true);
    expect(setApplet(on, CONTACTS_APP, false)).toBe(BASE);
    expect(setApplet(BASE, CONTACTS_APP, false)).toBe(BASE);
  });

  it("leaves a hand-written entry's params alone when already on", () => {
    const text = `${BASE}\n[[applets]]\nid = "photos"\ncommand = "datalib-applet photos"\n[applets.params]\nx = 1\n`;
    expect(setApplet(text, PHOTOS_APP, true)).toBe(text);
    expect(configuredAppletIds(setApplet(text, PHOTOS_APP, false)).has("photos")).toBe(false);
  });
});

describe("the apps", () => {
  it("are found by the picker's filter", () => {
    expect(filterApps("contact")).toEqual([CONTACTS_APP]);
    expect(filterApps("lightroom")).toEqual([PHOTOS_APP]);
    expect(filterApps("")).toEqual(BUILTIN_APPLETS);
  });

  it("are what every catalog entry's `applet` names", () => {
    const offered = CATALOG.filter((e) => e.applet);
    expect(offered.map((e) => e.type)).toEqual(["lightroom"]);
    for (const e of offered) expect(builtinApplet(e.applet!)).toBeDefined();
  });
});
