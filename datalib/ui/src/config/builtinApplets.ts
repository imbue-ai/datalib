// The applets datalib ships that a person turns on or off, rather than
// ones every library has (the document index) or that come with a source
// (the time-series index, a package's). The "Add data source" picker
// lists them as apps, and a source whose data one shows offers it on its
// form (`CatalogEntry.applet`). docs/user/applets.md says what each serves.

import { listSteps, removeSteps } from "./sourceSteps";
import { quote } from "./tomlText";

export type BuiltinApplet = {
  /// The `[[applets]]` entry's id, and the word after `datalib-applet`.
  id: string;
  /// The group its Manage row is filed under, if any.
  group: string | null;
  label: string;
  blurb: string;
  keywords: string[];
  /// `ui/src/assets/<icon>.svg`, or null for the fallback mark.
  icon: string | null;
  /// What turning it on gives a person, shown before they do.
  about: string;
};

export const CONTACTS_APP: BuiltinApplet = {
  id: "datalib_contacts",
  group: "unified_document_index",
  label: "Contacts app",
  blurb: "Your own record of who each email address, number and handle is.",
  keywords: ["contacts", "people", "person", "handles", "link", "labels", "app"],
  icon: "contacts",
  about:
    "Links the addresses, numbers and handles your sources see to the people they belong to: " +
    "every chip then shows the person's name, and a search for them finds them under every " +
    "handle. Your links live in a store of their own; no source changes.",
};

export const PHOTOS_APP: BuiltinApplet = {
  id: "photos",
  group: null,
  label: "Photos card",
  blurb: "A contact sheet of the photos and videos your photo sources hold.",
  keywords: ["photos", "pictures", "images", "videos", "lightroom", "gallery", "app"],
  icon: null,
  about:
    "Adds a Photos card to the card gallery: every photo and video your photo sources hold, " +
    "newest first, from Lightroom catalogs today. It reads what the sources have already synced.",
};

export const BUILTIN_APPLETS: BuiltinApplet[] = [CONTACTS_APP, PHOTOS_APP];

export function builtinApplet(id: string): BuiltinApplet | undefined {
  return BUILTIN_APPLETS.find((a) => a.id === id);
}

/// The apps whose label, id or keywords match the picker's filter box.
export function filterApps(query: string): BuiltinApplet[] {
  const q = query.trim().toLowerCase();
  if (!q) return BUILTIN_APPLETS;
  return BUILTIN_APPLETS.filter((a) =>
    [a.label, a.id, ...a.keywords].some((s) => s.toLowerCase().includes(q)),
  );
}

/// The `[[applets]]` entry that runs `applet`.
export function appletToml(applet: BuiltinApplet): string {
  return [
    "[[applets]]",
    ...(applet.group ? [`group = ${quote(applet.group)}`] : []),
    `id = ${quote(applet.id)}`,
    `command = ${quote(`datalib-applet ${applet.id}`)}`,
  ].join("\n");
}

/// The ids of every `[[applets]]` entry in `text`.
export function configuredAppletIds(text: string): Set<string> {
  return new Set(
    listSteps(text)
      .filter((s) => s.kind === "applet")
      .map((s) => s.id),
  );
}

/// `text` with `applet` turned on or off: its entry added at the end when
/// on and missing, every entry with its id taken out when off. An entry
/// already there is left as written, params and all.
export function setApplet(text: string, applet: BuiltinApplet, on: boolean): string {
  const entries = listSteps(text).filter((s) => s.kind === "applet" && s.id === applet.id);
  if (on) {
    return entries.length ? text : `${text.replace(/\s*$/, "")}\n\n${appletToml(applet)}\n`;
  }
  return entries.length ? removeSteps(text, entries).replace(/\s*$/, "\n") : text;
}
