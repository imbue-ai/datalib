// What a chip naming a group or a step of the config is (docs/dev/plans/
// chips.md): `entities`, the resolver behind datalib-http's
// `/api/entities`, and the rules for how such a chip looks, copies and
// opens. The rules are pure and unit-tested; `entityCell` is the one
// function that makes DOM, through the same `drawChip` a person's chip
// uses.

import type { ManageRow, StatusView } from "@/api";
import { changed, subscribeLive, type RootEvent } from "@/live";
import { pushToast } from "@/toasts";
import { entityFromUri } from "./chipLinks";
import { drawChip, type ChipLook } from "./contacts";
import { logSource, syncDashboardSource } from "./cardSources";
import { Resolver } from "./resolver";

/** `datalib_http::manage::EntityView`, hand-kept: a group or a step as
 *  the config and the loop's record say it is now, and its row of the
 *  Sources table, whose actions are the chip's menu. */
export type EntityView = {
  label: string;
  icon: string | null;
  detail: string | null;
  status: StatusView;
  row?: ManageRow;
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
const LOUD = new Set(["running", "waiting", "queued", "failed", "blocked", "interrupted"]);

/** Where a chip is drawn changes what it needs to say. `keepShown`
 *  keeps the producer's text where the context already names the rest —
 *  a step drawn under its group reads "Download", not "Work Slack ·
 *  Download" — and leaves the whole name to the hover. `quiet` drops the
 *  status dot where a status sits right beside the chip. */
export type EntityContext = { keepShown?: boolean; quiet?: boolean };

/** How a group or step chip looks: the name the config gives it now (or,
 *  until the answer lands, the one the producer sent), the mark of its
 *  type or its phase, and a status worth noticing. */
export function entityLook(
  uri: string,
  shown: string,
  view: EntityView | undefined,
  fallbackIcon: string | null,
  { keepShown = false, quiet = false }: EntityContext = {},
): ChipLook {
  const entity = entityFromUri(uri);
  const name = view?.label || shown || entity?.id || uri;
  const text = (keepShown && shown) || name;
  const status = view?.status;
  // What the mark stands for — a source's type, a step's phase — so a
  // screen reader hears what the eye reads off the mark.
  const kind = entity?.kind === "step" ? "step" : "source";
  const what = view?.detail ? `${view.detail} ${kind}` : kind;
  const look = {
    text,
    ariaLabel: status ? `${text}, ${what}, ${status.label}` : `${text}, ${what}`,
    classes: [
      "handle-chip",
      "entity-chip",
      ...(status && !quiet && LOUD.has(status.key) ? [`entity-${status.key}`] : []),
    ],
    initial: null,
    icon: view?.icon ?? fallbackIcon,
    photo: null,
    title: "",
  };
  return { ...look, title: entityTitle(uri, { text: name }, view) };
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

/** The group a step id is filed under: its first segment. */
function groupOf(stepId: string): string {
  return stepId.split("/")[0];
}

/** What double-clicking a chip opens: a group's sync dashboard, or for a
 *  step, its group's dashboard at the step's own section. Null for a URI
 *  naming neither. */
export function entityCardSource(uri: string): string | null {
  const e = entityFromUri(uri);
  if (!e) return null;
  if (e.kind === "group") return syncDashboardSource({ group: e.id });
  return syncDashboardSource({ group: groupOf(e.id), step: e.id });
}

/** A step's log, at its end: a step chip's "Show its log". */
export function entityLogSource(uri: string): string | null {
  const e = entityFromUri(uri);
  return e?.kind === "step" ? logSource({ step: e.id, jumpToEnd: true }) : null;
}

/** The right-click menu for a group or step chip, as ids `chipActions`
 *  binds: open it, a step's log, copy the id, the name or both, and
 *  browse a group's documents — in the order `CHIP_ACTIONS` gives every
 *  chip's menu. */
export type EntityMenuId = "open" | "log" | "copy-id" | "copy-name" | "copy-both" | "browse";
export type EntityMenuEntry = { id: EntityMenuId; label: string; separator?: boolean };

export function entityMenu(uri: string, name: string): EntityMenuEntry[] {
  const e = entityFromUri(uri);
  if (!e) return [];
  const out: EntityMenuEntry[] = [
    {
      id: "open",
      label: e.kind === "group" ? "Open its sync dashboard" : "Show it in its sync dashboard",
    },
  ];
  if (e.kind === "step") out.push({ id: "log", label: "Show its log" });
  out.push({ id: "copy-id", label: `Copy ${e.id}`, separator: true });
  // A source never named in the wizard is called by its id: one Copy.
  if (name !== e.id) {
    out.push(
      { id: "copy-name", label: `Copy “${name}”` },
      { id: "copy-both", label: `Copy “${entityCopyText(uri, name)}”` },
    );
  }
  if (e.kind === "group") out.push({ id: "browse", label: `Browse ${name}`, separator: true });
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
  context: EntityContext = {},
): HTMLAnchorElement {
  const a = document.createElement("a");
  a.className = "chip";
  a.href = uri;
  a.dataset.entity = uri;
  a.dataset.shownAs = shown;
  if (fallbackIcon) a.dataset.icon = fallbackIcon;
  drawChip(a, entityLook(uri, shown, view, fallbackIcon, context));
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
