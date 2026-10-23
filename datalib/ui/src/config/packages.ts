// What a library's packages mean for its config and its "Add source"
// picker: the `[[packages]]` entries the Packages dialog writes, and the
// catalog entries a package's sources add. docs/dev/packages.md.

import { shallowRef } from "vue";
import {
  fetchPackages,
  type PackageApplet,
  type PackageLookup,
  type PackageManifest,
  type PackagesResponse,
} from "@/api";
import { setPackageEntries, type CatalogEntry } from "./catalog";
import {
  listGroups,
  listPackages,
  listSteps,
  removeSteps,
  slugify,
  suggestId,
} from "./sourceSteps";
import { quote } from "./tomlText";
import { setIn } from "./tomlPatch";

/// The catalog entries of every package unpacked here. A package still
/// being fetched adds nothing yet: its form is in its manifest.
export function catalogEntriesFrom(res: PackagesResponse): CatalogEntry[] {
  return res.packages.flatMap((p) => (p.manifest ? entriesOfManifest(p.id, p.manifest) : []));
}

export function entriesOfManifest(id: string, m: PackageManifest): CatalogEntry[] {
  return m.sources.map((s) => {
    const wizard = s.wizard as Partial<CatalogEntry>;
    const label = typeof wizard.label === "string" ? wizard.label : s.type;
    return {
      keywords: [],
      icon: null,
      defaultName: s.type,
      nameHint: label,
      blurb: m.description ?? "",
      ...wizard,
      label,
      type: s.type,
      kind: "package",
      wizard: true,
      rendersDocuments: "render_markdown" in s.commands,
      package: {
        id,
        name: m.name,
        version: m.version,
        commands: s.commands,
        applets: s.applets ?? [],
      },
    } as CatalogEntry;
  });
}

/// The id a new package gets: its name, made path-safe, unless taken.
export function packageIdFor(text: string, m: PackageManifest): string {
  const taken = new Set(listPackages(text).map((p) => p.id));
  return suggestId(taken, slugify(m.name), "package");
}

/// `text` with the looked-up package added as `[[packages]]` entry `id`.
/// The entry goes after the last package, or above the first table when
/// there is none, so the packages read first. Its applets come with each
/// source that brings them (`addSourceApplets`), not with the package.
export function addPackage(
  text: string,
  opts: { id: string; source: string; lookup: PackageLookup },
): string {
  const { id, source, lookup } = opts;
  const entry = [
    "[[packages]]",
    `id = ${quote(id)}`,
    `source = ${quote(source.trim())}`,
    `url = ${quote(lookup.url)}`,
    `sha256 = ${quote(lookup.sha256)}`,
  ].join("\n");
  const existing = listPackages(text).filter((p) => p.end > 0);
  const at = existing.length ? Math.max(...existing.map((p) => p.end)) : firstTableStart(text);
  const next =
    at === null
      ? `${text.replace(/\s*$/, "")}\n\n${entry}\n`
      : `${text.slice(0, at).replace(/\s*$/, "")}\n\n${entry}\n\n${text.slice(at).replace(/^\s*/, "")}`;
  return next.replace(/^\s+/, "");
}

/// `text` with an `[[applets]]` entry for each of `applets`, filed under
/// source `group` and running package `pkg`. Its id is the source's id
/// and the applet's, made a JavaScript identifier, unless that is taken;
/// `params.source` tells it which source it serves.
export function addSourceApplets(
  text: string,
  group: string,
  pkg: string,
  applets: PackageApplet[],
): string {
  const taken = new Set(
    listSteps(text)
      .filter((s) => s.kind === "applet")
      .map((s) => s.id),
  );
  let next = text;
  for (const a of applets) {
    const base = `${group}_${a.id}`.replace(/[^A-Za-z0-9_]/g, "_").replace(/^(?=\d)/, "_");
    const appletId = suggestId(taken, base, base).replace(/-/g, "_");
    taken.add(appletId);
    next = `${next.replace(/\s*$/, "")}\n\n${appletBlock(appletId, group, pkg, a)}\n`;
  }
  return next;
}

function appletBlock(id: string, group: string, pkg: string, a: PackageApplet): string {
  const lines = [
    "[[applets]]",
    `id = ${quote(id)}`,
    `group = ${quote(group)}`,
    `package = ${quote(pkg)}`,
    `command = ${quote(a.command)}`,
    "[applets.params]",
    `source = ${quote(group)}`,
  ];
  for (const [k, v] of Object.entries(a.params ?? {})) {
    if (k === "source") continue;
    if (typeof v === "string" || typeof v === "number" || typeof v === "boolean") {
      lines.push(`${k} = ${typeof v === "string" ? quote(v) : String(v)}`);
    }
  }
  return lines.join("\n");
}

/// Where the first `[[…]]` table starts, its banner comments included.
function firstTableStart(text: string): number | null {
  const m = /^\[\[/m.exec(text);
  if (!m) return null;
  let at = m.index;
  for (;;) {
    const prevEnd = text.lastIndexOf("\n", at - 1);
    if (prevEnd < 0) break;
    const prevStart = text.lastIndexOf("\n", prevEnd - 1) + 1;
    const line = text.slice(prevStart, prevEnd).trim();
    if (line !== "" && !line.startsWith("#")) break;
    at = prevStart;
    if (prevStart === 0) break;
  }
  return at;
}

/// `text` with package `id` pinned to the looked-up manifest instead, and
/// each of its sources' applets brought in line with the new version's:
/// one it no longer has goes, a new one is added. An applet is matched by
/// its command, since the config may hold it under another id; a kept one
/// keeps its entry as it is.
export function updatePackage(text: string, id: string, lookup: PackageLookup): string {
  const index = listPackages(text).findIndex((p) => p.id === id);
  if (index < 0) throw new Error(`No package ${id} in the config.`);
  const ref = { array: "packages", index };
  let next = setIn(text, ref, ["url"], quote(lookup.url));
  next = setIn(next, ref, ["sha256"], quote(lookup.sha256));
  for (const group of sourcesUsing(next, id)) {
    const type = listGroups(next).find((g) => g.id === group)?.type;
    const wanted = lookup.manifest.sources.find((s) => s.type === type)?.applets ?? [];
    const have = listSteps(next).filter(
      (s) => s.kind === "applet" && s.package === id && s.group === group,
    );
    const gone = have.filter((s) => !wanted.some((a) => a.command === s.command));
    if (gone.length) next = removeSteps(next, gone);
    const added = wanted.filter((a) => !have.some((s) => s.command === a.command));
    next = addSourceApplets(next, group, id, added);
  }
  return next;
}

/// The groups whose steps run a package's programs. A package they use
/// cannot be removed without them.
export function sourcesUsing(text: string, id: string): string[] {
  return [
    ...new Set(
      listSteps(text)
        .filter((s) => s.kind === "step" && s.package === id)
        .map((s) => s.group ?? s.id),
    ),
  ];
}

/// `text` without package `id` and the applets that run it. Refused
/// (throws) while a source still does.
export function removePackage(text: string, id: string): string {
  const using = sourcesUsing(text, id);
  if (using.length) {
    throw new Error(
      `Remove ${using.join(", ")} first: ${using.length === 1 ? "it runs" : "they run"} this package.`,
    );
  }
  const pkg = listPackages(text).find((p) => p.id === id);
  if (!pkg) return text;
  const applets = listSteps(text).filter((s) => s.kind === "applet" && s.package === id);
  return removeSteps(text, [pkg, ...applets]);
}

/// The library's packages as the server last listed them, shared by the
/// Sources card, its Packages dialog and the "Add source" picker.
export const packagesState = shallowRef<PackagesResponse | null>(null);

let pollTimer: ReturnType<typeof setTimeout> | undefined;

/// Ask the server again, and put the packages' sources in the catalog.
/// While one is still being fetched, ask again shortly: the listing
/// itself is what starts a fetch, and it says when one is done.
export async function refreshPackages(): Promise<PackagesResponse | null> {
  clearTimeout(pollTimer);
  try {
    const res = await fetchPackages();
    packagesState.value = res;
    setPackageEntries(catalogEntriesFrom(res));
    if (res.packages.some((p) => p.state === "fetching")) {
      pollTimer = setTimeout(() => void refreshPackages(), 2000);
    }
    return res;
  } catch {
    return packagesState.value;
  }
}
