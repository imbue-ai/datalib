// What a chip naming a group or a step of the config is (docs/dev/plans/
// chips.md): `entities`, the resolver behind datalib-http's
// `/api/entities`, and the rules for how such a chip looks, copies and
// opens. The rules are pure and unit-tested; `entityCell` is the one
// function that makes DOM, through the same `drawChip` a person's chip
// uses.

import type { StatusView } from "@/api";
import { changed, subscribeLive, type RootEvent } from "@/live";
import { pushToast } from "@/toasts";
import { entityFromUri } from "./chipLinks";
import { drawChip, type ChipLook } from "./contacts";
import { logSource, syncDashboardSource } from "./cardSources";
import { Resolver } from "./resolver";

/** `datalib_http::manage::EntityView`, hand-kept: a group or a step as
 *  the config and the loop's record say it is now. */
export type EntityView = {
  label: string;
  icon: string | null;
  detail: string | null;
  status: StatusView;
};

export const entities = new Resolver<EntityView>(
  async (uris) => {
    followLive();
    const r = await fetch("/api/entities", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ entities: uris }),
    });
    if (!r.ok) throw new Error(`entities → ${r.status}: ${await r.text()}`);
    const body = (await r.json()) as { entities: Record<string, EntityView> };
    return new Map(Object.entries(body.entities));
  },
  // The toast dedupes itself, so a page of chips failing says so once.
  (e) => pushToast(`Chips: ${e.message}`),
);

/** Whether a live frame can have moved what a group or step chip shows:
 *  its status (the loop's record, which the Manage rows read) or its
 *  name (the config). */
export function movesEntities(e: RootEvent): boolean {
  return changed(e, "manage.rows") || e.kind === "config_changed";
}

/// A status moves on its own while a sync runs, so the answers held are
/// asked again on every frame that can have moved one. Subscribed on the
/// first question, for the life of the page: the toolbar keeps the one
/// live connection open regardless.
let following = false;
function followLive() {
  if (following) return;
  following = true;
  subscribeLive({
    root: (e) => {
      if (movesEntities(e)) entities.revalidate();
    },
    resync: () => entities.revalidate(),
  });
}

/// The status words worth a mark on the chip itself; the rest are on hover.
const LOUD = new Set(["running", "queued", "failed", "blocked", "interrupted"]);

/** How a group or step chip looks: the name the config gives it now (or,
 *  until the answer lands, the one the producer sent), the mark of its
 *  type or its phase, and a status worth noticing. */
export function entityLook(
  uri: string,
  shown: string,
  view: EntityView | undefined,
  fallbackIcon: string | null,
): ChipLook {
  const entity = entityFromUri(uri);
  const text = view?.label || shown || entity?.id || uri;
  const status = view?.status;
  const what = entity?.kind === "step" ? "step" : "source";
  const look = {
    text,
    ariaLabel: status ? `${text}, ${what}, ${status.label}` : `${text}, ${what}`,
    classes: [
      "handle-chip",
      "entity-chip",
      ...(status && LOUD.has(status.key) ? [`entity-${status.key}`] : []),
    ],
    initial: null,
    icon: view?.icon ?? fallbackIcon,
    photo: null,
    title: "",
  };
  return { ...look, title: entityTitle(uri, look, view) };
}

/** The hover: what it is, and where it stands. */
export function entityTitle(
  uri: string,
  look: Pick<ChipLook, "text">,
  view: EntityView | undefined,
): string {
  const id = entityFromUri(uri)?.id ?? uri;
  const lines = [`${look.text} (${id})`];
  if (view?.detail) lines.push(view.detail);
  if (view?.status) {
    lines.push(
      view.status.detail ? `${view.status.label}: ${view.status.detail}` : view.status.label,
    );
  }
  return lines.join("\n");
}

/** A chip as copied text: its name and its URI, so a paste loses neither. */
export function entityCopyText(uri: string, name: string): string {
  return `${name} (${uri})`;
}

/** What double-clicking a chip opens: a group's sync dashboard, a step's
 *  log. Null for a URI naming neither. */
export function entityCardSource(uri: string): string | null {
  const e = entityFromUri(uri);
  if (!e) return null;
  if (e.kind === "group") return syncDashboardSource({ group: e.id });
  return logSource({ step: e.id, jumpToEnd: true });
}

/** The right-click menu for a group or step chip, as ids a surface binds:
 *  copy the name, the id or both, and open it. */
export type EntityMenuId = "copy-name" | "copy-id" | "copy-both" | "open" | "browse";
export type EntityMenuEntry = { id: EntityMenuId; label: string; separator?: boolean };

export function entityMenu(uri: string, name: string): EntityMenuEntry[] {
  const e = entityFromUri(uri);
  if (!e) return [];
  const out: EntityMenuEntry[] = [
    { id: "copy-name", label: `Copy “${name}”` },
    { id: "copy-id", label: `Copy ${e.id}` },
    { id: "copy-both", label: `Copy “${entityCopyText(uri, name)}”` },
    {
      id: "open",
      label: e.kind === "group" ? "Open its sync dashboard" : "Show its log",
      separator: true,
    },
  ];
  if (e.kind === "group") out.push({ id: "browse", label: `Browse ${name}` });
  return out;
}

/** The search that lists a group's documents, as its Browse does. */
export function browseQuery(uri: string): string | null {
  const e = entityFromUri(uri);
  return e?.kind === "group" ? `source_id:${e.id} is:document` : null;
}

/** A group or step chip for a grid cell, drawn from what is known now. */
export function entityCell(
  uri: string,
  shown: string,
  view: EntityView | undefined,
  fallbackIcon: string | null,
): HTMLAnchorElement {
  const a = document.createElement("a");
  a.className = "chip";
  a.href = uri;
  a.dataset.entity = uri;
  a.dataset.shownAs = shown;
  if (fallbackIcon) a.dataset.icon = fallbackIcon;
  drawChip(a, entityLook(uri, shown, view, fallbackIcon));
  return a;
}

/** Re-draw every group or step chip under `root` from `entities`,
 *  asking about the ones not yet known: a document's decorate pass. */
export async function decorateEntities(root: HTMLElement): Promise<void> {
  const chips = Array.from(root.querySelectorAll<HTMLElement>("a.chip[data-entity]"));
  if (chips.length === 0) return;
  for (const c of chips) {
    if (c.dataset.shownAs === undefined) c.dataset.shownAs = c.textContent ?? "";
  }
  await entities.ask(new Set(chips.map((c) => c.dataset.entity ?? "")));
  for (const c of chips) {
    if (!c.isConnected) continue;
    const uri = c.dataset.entity ?? "";
    const view = entities.get(uri);
    drawChip(c, entityLook(uri, c.dataset.shownAs ?? "", view, c.dataset.icon ?? null));
  }
}
