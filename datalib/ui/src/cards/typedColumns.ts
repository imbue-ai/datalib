// The column-type vocabulary, as slickgrid column definitions. A
// table's producer declares a `ColumnSpec` per column; `typedColumns`
// turns each into a `Column` whose cell is drawn by the column's *type*
// — with the renderers in `cellRenderers.ts` — rather than by comparing
// field names. Pure: it knows nothing about what the rows are, and owns
// no grid. `TableGrid.ce.vue` mounts one over these for the simple
// hosts; a card with a grid of its own (`GridCard`) calls this and
// keeps driving its grid itself.
import type { Column, Formatter, GroupingFormatterItem } from "@slickgrid-universal/common";
import { contactFromUri, entityFromUri, handleFromUri } from "./chipLinks";
import { entityCell, type EntityView } from "./entities";
import { chipCell, contactCell, type NormalizedContact, type Who } from "./contacts";
import { CELL_MEASURE, fitChips } from "./chipFit";
import { Editors } from "@slickgrid-universal/common";
import type {
  Action,
  ColumnSpec,
  ColumnType,
  Identity,
  Quantity,
  StatusView,
  Chip,
  Timeseries,
} from "@/api";
import {
  WIDTH,
  renderBadges,
  renderChips,
  quantityText,
  renderIdentity,
  renderQuantity,
  renderStatus,
  renderTimeseries,
  renderTimestamp,
} from "./cellRenderers";
import { compareStamps, formatStamp } from "@/config/timeFormat";
import { formatBytes } from "@/config/bytes";

export type SlickColumnOptions<T> = {
  /// The rows form a tree; the tree column (`treeColumnField`) carries
  /// the chevron and shows each row's own id beside its label.
  tree?: boolean;
  /// What each action id does when its button is pressed. An id with
  /// no handler draws no button.
  actions?: Record<string, (row: T) => void>;
  /// A `markdown_uuid` cell was clicked.
  onOpenDocument?: (uuid: string, row: T) => void;
  /// Every column can be dragged into the grouping bar. Off, no column
  /// carries a `grouping`, and the bar accepts none.
  groupable?: boolean;
  /// Per-field refinements a type cannot know — a width, a hover, a
  /// formatter — merged over the typed definition.
  overrides?: Record<string, Partial<Column<T>>>;
  /// An identity whose id names a person (a handle as a URI) is drawn
  /// as a chip, the way a document draws one, from what the grid has
  /// resolved so far (docs/dev/chips.md § "In a grid"). Absent,
  /// every identity is icon and label.
  chips?: {
    who: (handle: string) => Who | undefined;
    canLink: () => boolean;
    /// A group or step identity (`Identity.entity`): what is known of it.
    entity?: (uri: string) => EntityView | undefined;
    /// A group or step chip keeps the cell's own label, where the grid
    /// already says the rest: a step nested under its group.
    keepLabels?: boolean;
    /// A group or step chip draws no status dot, where a Status column
    /// beside it says the same.
    quiet?: boolean;
    /// One of your contacts, whose id is a `datalib:contact/` URI.
    contact?: (contactId: string) => NormalizedContact | undefined;
  };
};

/// A group row's title: the column, the value and how many rows share
/// it. An element, not markup: the value is the rows' own text. `chip`
/// draws the value as a chip instead, where it names one.
export function groupTitle(
  name: string,
  show: (value: unknown) => string = String,
  chip: (g: GroupingFormatterItem) => HTMLElement | null = () => null,
) {
  return (g: GroupingFormatterItem) => {
    const el = document.createElement("span");
    el.className = "tg-group";
    const drawn = chip(g);
    if (drawn) {
      el.append(`${name}: `, drawn);
    } else {
      const value = g.value == null || g.value === "" ? "—" : show(g.value);
      el.textContent = `${name}: ${value}`;
    }
    const count = document.createElement("span");
    count.className = "tg-group-count";
    count.textContent = ` (${g.count})`;
    el.appendChild(count);
    return el as unknown as string;
  };
}

/// Text order with the empties last, whichever way the column sorts.
function compareText(a: unknown, b: unknown, dir: number): number {
  const sa = a == null ? "" : String(a);
  const sb = b == null ? "" : String(b);
  if (sa === sb) return 0;
  if (sa === "") return 1;
  if (sb === "") return -1;
  return sa.localeCompare(sb) * dir;
}

function compareNumber(a: unknown, b: unknown, dir: number): number {
  const na = typeof a === "number" ? a : null;
  const nb = typeof b === "number" ? b : null;
  if (na === nb) return 0;
  if (na === null) return 1;
  if (nb === null) return -1;
  return (na - nb) * dir;
}

/// The tree column's cell: the indent, the chevron the grid's tree
/// service listens for (by class), then the column's own drawing. The
/// grid's own tree formatter serializes that drawing to markup and
/// re-parses it, which a grid that renders text cannot; this builds it
/// as DOM the whole way.
function treeCell<T extends Record<string, unknown>>(inner: Formatter<T>): Formatter<T> {
  return (row, cell, value, col, item, grid) => {
    const level = Number(item.__treeLevel ?? 0);
    const wrap = document.createElement("span");
    wrap.className = `tg-tree slick-tree-level-${level}`;
    const indent = document.createElement("span");
    indent.style.display = "inline-block";
    indent.style.width = `${18 * level}px`;
    const toggle = document.createElement("span");
    const state = item.__hasChildren ? (item.__collapsed ? "collapsed" : "expanded") : "";
    toggle.className = `slick-group-toggle slick-tree-toggle ${state}`.trim();
    toggle.setAttribute("aria-expanded", String(state === "expanded"));
    const title = document.createElement("span");
    title.className = "slick-tree-title";
    title.setAttribute("level", String(level));
    const drawn = inner(row, cell, value, col, item, grid);
    if (drawn instanceof HTMLElement || drawn instanceof DocumentFragment) title.appendChild(drawn);
    else
      title.textContent =
        typeof drawn === "string" ? drawn : ((drawn as { text?: string }).text ?? "");
    wrap.append(indent, toggle, title);
    return wrap;
  };
}

/// 24×24 Material-ish glyphs for the action ids the viewer knows a
/// picture for, drawn in `currentColor`. An action carrying `on` draws
/// a switch; any other id draws its label.
export const ACTION_ICONS: Record<string, string> = {
  // A table: what Browse opens is this row's data as rows and columns.
  browse: "M3 5h18v4H3V5zm0 6h8v8H3v-8zm10 0h8v8h-8v-8z",
  sync: "M8 5v14l11-7z",
  stop: "M6 6h12v12H6z",
  // A line climbing across axes: the sync's numbers over the run.
  dashboard: "M3.5 18.49l6-6.01 4 4L22 6.92l-1.41-1.41-7.09 7.97-4-4L2 16.99z",
};

/// The buttons of an `actions` cell. One element per row, kept across
/// repaints and updated in place: a repaint runs on every rows refetch —
/// a few times a second during a sync — and a button rebuilt under a
/// press swallows the click (its mousedown landed on the old one). The
/// grid moves the kept element into the fresh cell.
function actionsFormatter<T extends Record<string, unknown>>(
  handlers: Record<string, (row: T) => void>,
): Formatter<T> {
  type Kept = { wrap: HTMLSpanElement; buttons: Map<string, HTMLButtonElement>; row: T };
  const kept = new Map<unknown, Kept>();
  return (_r, _c, value, _col, row, grid) => {
    const idProp = grid.getOptions().datasetIdPropertyName ?? "id";
    const key = row[idProp];
    let k = kept.get(key);
    if (!k) {
      k = { wrap: document.createElement("span"), buttons: new Map(), row };
      k.wrap.className = "tg-actions";
      kept.set(key, k);
    }
    // The row current at paint time: a click reads it, not the row the
    // button was first built for.
    k.row = row;
    const held = k;
    const actions = (value as Action[] | null | undefined) ?? [];
    const wanted = new Set<string>();
    for (const a of actions) {
      if (!handlers[a.id]) continue;
      wanted.add(a.id);
      let b = held.buttons.get(a.id);
      if (!b) {
        b = document.createElement("button");
        b.type = "button";
        const glyph = ACTION_ICONS[a.id];
        if (a.on != null) {
          b.className = "tg-switch";
          b.setAttribute("role", "switch");
          const knob = document.createElement("span");
          knob.className = "tg-switch-knob";
          b.appendChild(knob);
        } else if (glyph) {
          b.className = "tg-icon-btn";
          const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
          svg.setAttribute("viewBox", "0 0 24 24");
          svg.setAttribute("width", "15");
          svg.setAttribute("height", "15");
          svg.setAttribute("aria-hidden", "true");
          const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
          path.setAttribute("fill", "currentColor");
          path.setAttribute("d", glyph);
          svg.appendChild(path);
          b.appendChild(svg);
        } else {
          b.className = "tg-btn";
        }
        const id = a.id;
        b.addEventListener("click", (e) => {
          e.stopPropagation();
          handlers[id]?.(held.row);
        });
        held.buttons.set(a.id, b);
      }
      if (a.on != null) b.setAttribute("aria-checked", String(a.on));
      else if (!ACTION_ICONS[a.id]) b.textContent = a.label;
      b.title = a.enabled ? (a.hint ?? a.label) : (a.disabled_reason ?? a.label);
      b.setAttribute("aria-label", a.label);
      b.disabled = !a.enabled;
      b.classList.toggle("danger", !!a.danger);
      held.wrap.appendChild(b);
    }
    for (const [id, b] of held.buttons) {
      if (!wanted.has(id)) {
        b.remove();
        held.buttons.delete(id);
      }
    }
    return held.wrap;
  };
}

function text(value: unknown): string {
  return value == null ? "" : String(value);
}

const plain: Formatter = (_r, _c, value) => ({ text: text(value), toolTip: text(value) });

/// An identity as the chip it names — a group or step, else a person by
/// handle — or null for one that names neither, or when the grid draws
/// no chips.
function identityChip<T>(
  v: Identity,
  chips: SlickColumnOptions<T>["chips"],
): HTMLAnchorElement | null {
  if (chips?.entity && v.entity && entityFromUri(v.entity)) {
    return entityCell(v.entity, v.label, chips.entity(v.entity), v.icon ?? null, {
      keepShown: chips.keepLabels === true,
      quiet: chips.quiet === true,
    });
  }
  const contactId = chips?.contact ? contactFromUri(v.id) : null;
  if (contactId && chips?.contact) {
    return contactCell(contactId, v.label, chips.contact(contactId));
  }
  const handle = chips ? handleFromUri(v.id) : null;
  return handle && chips ? chipCell(handle, v.label, chips.who(handle), chips.canLink()) : null;
}

/// Show `text` on a drawn chip in place of its full label, which its
/// tooltip and menu keep.
function setChipText(chip: HTMLElement, text: string) {
  const label = chip.querySelector(".chip-text");
  if (label) label.textContent = text;
}

/// A list of identities in one cell: each as its chip, or as a plain
/// pill for one that names nothing the grid resolves; the labels cut in
/// their middle to share `widthPx`, and what does not fit folded into a
/// "+N" pill whose tooltip names them. One plain identity alone reads as
/// an identity cell does.
function renderIdentities<T>(
  list: Identity[] | null | undefined,
  widthPx: number,
  chips: SlickColumnOptions<T>["chips"],
): HTMLElement {
  const all = list ?? [];
  const drawn = all.map((v) => identityChip(v, chips));
  if (all.length === 1 && !drawn[0]) return renderIdentity(all[0], false);
  const wrap = document.createElement("span");
  wrap.className = "tg-identities";
  const labels = all.map((v, i) => drawn[i]?.dataset.label ?? v.label);
  const fit = fitChips(labels, widthPx, CELL_MEASURE);
  fit.shown.forEach((shown, i) => {
    const chip = drawn[i];
    if (chip) {
      setChipText(chip, shown);
      wrap.appendChild(chip);
      return;
    }
    const pill = document.createElement("span");
    pill.className = "handle-chip handle-unresolved tg-plain-chip";
    pill.textContent = shown;
    pill.title = all[i].detail ?? all[i].label;
    wrap.appendChild(pill);
  });
  if (fit.folded) {
    const more = document.createElement("span");
    more.className = "handle-chip handle-unresolved tg-more-chip";
    more.textContent = `+${fit.folded}`;
    more.title = labels.slice(fit.shown.length).join("\n");
    wrap.appendChild(more);
  }
  return wrap;
}

function identitiesText(value: unknown): string {
  return ((value as Identity[] | null) ?? []).map((v) => v.label).join(", ");
}

/// What a cell copies as: what it shows, except that a stamp copies as
/// the stamp and a number undecorated, so a paste loses nothing.
export function copyText(type: ColumnType, value: unknown): string {
  if (value == null) return "";
  switch (type) {
    case "identity":
      return (value as Identity).label ?? "";
    case "identities":
      return identitiesText(value);
    case "markdown_uuid":
      return typeof value === "string" ? value : ((value as Identity).label ?? "");
    case "status": {
      const s = value as StatusView;
      return s.at ? `${s.label} ${s.at}` : s.label;
    }
    case "chips":
      return (value as Chip[]).map((c) => c.text).join(", ");
    case "timeseries": {
      const t = value as Timeseries;
      return t.value == null ? "" : `${t.value} ${t.unit}`;
    }
    case "quantity":
      return quantityText(value as Quantity);
    case "actions":
      return "";
    case "text":
    case "count":
    case "number":
    case "bytes":
    case "timestamp":
    case "datetime":
      return String(value);
  }
}

/// The column a tree hangs its chevrons off: the first one that is not
/// a row of buttons, so an `actions` column never becomes the tree
/// wherever it sits.
export function treeColumnField(specs: ColumnSpec[]): string {
  return (specs.find((s) => s.type !== "actions") ?? specs[0])?.field ?? "";
}

export function typedColumns<T extends Record<string, unknown>>(
  specs: ColumnSpec[],
  opts: SlickColumnOptions<T>,
): Column<T>[] {
  const Actions = actionsFormatter<T>(opts.actions ?? {});
  const treeField = treeColumnField(specs);

  return specs.map((spec) => {
    const f = spec.field;
    const isTreeColumn = !!opts.tree && f === treeField;
    const base: Column<T> = {
      id: f,
      // A dotted path type the spec's plain field name cannot satisfy.
      field: f as Column<T>["field"],
      name: spec.header,
      toolTip: spec.description,
      width: WIDTH[spec.type],
      hidden: !spec.default_visible,
      sortable: true,
      resizable: true,
      reorderable: true,
      // The tests, and anyone reading the DOM, find a cell by the
      // column it is in.
      cellAttrs: { "col-id": f },
      headerCellAttrs: { "col-id": f },
      formatter: plain,
      sortComparer: (a, b, dir) => compareText(a, b, dir ?? 1),
      ...(spec.editable && spec.type !== "identity" && spec.type !== "identities"
        ? { editor: { model: Editors.text } }
        : {}),
      ...(opts.groupable
        ? { grouping: { getter: f, formatter: groupTitle(spec.header), collapsed: false } }
        : {}),
    };
    const typed: Partial<Column<T>> = (() => {
      switch (spec.type) {
        case "identity": {
          const label = (v: unknown) => (v as Identity | null)?.label ?? "";
          const badges = spec.badges;
          const inner: Formatter<T> = (_r, _c, _v, _col, row) => {
            const v = row?.[f] as Identity | null;
            const counts = badges ? ((row?.[badges] as Chip[] | null) ?? []) : [];
            const chip = v ? identityChip(v, opts.chips) : null;
            // A chip keeps the counts the plain label would carry beside it.
            if (chip && badges && counts.length) {
              const wrap = document.createElement("span");
              wrap.className = "tg-identity";
              wrap.append(chip, renderBadges(badges, counts));
              return wrap;
            }
            if (chip) return chip;
            return renderIdentity(
              v,
              !!row?.__hasChildren,
              badges ? { field: badges, chips: counts } : null,
            );
          };
          return {
            // The cell's value is the label: what sorting, filtering and
            // an in-place edit see. The object is read off the row.
            field: `${f}.label` as Column<T>["field"],
            sortComparer: (a, b, dir) => compareText(a, b, dir ?? 1),
            ...(isTreeColumn
              ? {
                  // The column the tree hangs off: the chevron and the
                  // indent, then this.
                  width: 340,
                  formatter: treeCell(inner),
                }
              : { formatter: inner }),
            ...(spec.editable ? { editor: { model: Editors.text } } : {}),
            ...(opts.groupable
              ? {
                  grouping: {
                    getter: (row: T) => label(row[f]),
                    // The value as its chip, read off the group's first row,
                    // where the rows are loaded and it names one. The grid's
                    // group carries its rows; its declared type leaves them out.
                    formatter: groupTitle(spec.header, String, (g) => {
                      const rows = (g as GroupingFormatterItem & { rows?: T[] }).rows;
                      const v = rows?.[0]?.[f] as Identity | null | undefined;
                      return v ? identityChip(v, opts.chips) : null;
                    }),
                    collapsed: false,
                  },
                }
              : {}),
          };
        }
        case "identities":
          return {
            formatter: (_r, _c, value, col) =>
              renderIdentities(
                value as Identity[] | null,
                (col.width ?? WIDTH.identities) - 12,
                opts.chips,
              ),
            sortComparer: (a, b, dir) =>
              compareText(identitiesText(a), identitiesText(b), dir ?? 1),
            ...(opts.groupable
              ? {
                  grouping: {
                    getter: (row: T) => identitiesText(row[f]),
                    formatter: groupTitle(spec.header),
                    collapsed: false,
                  },
                }
              : {}),
          };
        case "status":
          return {
            formatter: (_r, _c, value) => renderStatus(value as StatusView | null),
            // By when, which is what the cell reads as.
            sortComparer: (a, b, dir) =>
              compareStamps(
                (a as StatusView | null)?.at ?? null,
                (b as StatusView | null)?.at ?? null,
              ) * (dir ?? 1),
          };
        case "chips":
          return {
            formatter: (_r, _c, value) => renderChips(value as Chip[] | null),
            sortComparer: (a, b, dir) =>
              compareText(
                ((a as Chip[] | null) ?? []).map((c) => c.text).join("  "),
                ((b as Chip[] | null) ?? []).map((c) => c.text).join("  "),
                dir ?? 1,
              ),
          };
        case "timestamp":
          return {
            formatter: (_r, _c, value) => renderTimestamp(value as string | null),
            sortComparer: (a, b, dir) => compareStamps(a as string, b as string) * (dir ?? 1),
          };
        case "datetime":
          return {
            formatter: (_r, _c, value) => ({
              text: typeof value === "string" && value ? formatStamp(value) : "",
              toolTip: text(value),
            }),
            sortComparer: (a, b, dir) => compareStamps(a as string, b as string) * (dir ?? 1),
          };
        case "timeseries":
          return {
            formatter: (_r, _c, value) => renderTimeseries(value as Timeseries | null),
            sortComparer: (a, b, dir) =>
              compareNumber(
                (a as Timeseries | null)?.value,
                (b as Timeseries | null)?.value,
                dir ?? 1,
              ),
          };
        case "quantity":
          return {
            cssClass: "tg-right",
            formatter: (_r, _c, value) => renderQuantity(value as Quantity | null),
            // A figure sorts by size; a word after every figure.
            sortComparer: (a, b, dir) =>
              compareNumber(
                (a as Quantity | null)?.value ?? undefined,
                (b as Quantity | null)?.value ?? undefined,
                dir ?? 1,
              ),
          };
        case "bytes":
          return {
            cssClass: "tg-right",
            type: "number",
            formatter: (_r, _c, value) => ({
              text: typeof value === "number" ? formatBytes(value) : "",
              toolTip: typeof value === "number" ? `${value.toLocaleString()} bytes` : "",
            }),
            sortComparer: (a, b, dir) => compareNumber(a, b, dir ?? 1),
          };
        case "count":
          return {
            cssClass: "tg-right",
            type: "number",
            formatter: (_r, _c, value) => ({
              text: typeof value === "number" ? value.toLocaleString() : "",
            }),
            sortComparer: (a, b, dir) => compareNumber(a, b, dir ?? 1),
          };
        case "number":
          return {
            cssClass: "tg-right",
            type: "number",
            formatter: (_r, _c, value) => ({
              text:
                typeof value === "number"
                  ? value.toLocaleString(undefined, { maximumFractionDigits: 3 })
                  : "",
            }),
            sortComparer: (a, b, dir) => compareNumber(a, b, dir ?? 1),
          };
        case "markdown_uuid":
          return {
            formatter: (_r, _c, value, _col, row) => {
              const v = value as Identity | string | null;
              const id = typeof v === "string" ? v : v?.id;
              const a = document.createElement("a");
              a.className = "tg-link";
              a.textContent = typeof v === "string" ? v : (v?.label ?? "");
              a.href = "#";
              a.addEventListener("click", (e) => {
                e.preventDefault();
                e.stopPropagation();
                if (id) opts.onOpenDocument?.(id, row);
              });
              return a;
            },
          };
        case "actions":
          return { sortable: false, resizable: false, filterable: false, formatter: Actions };
        case "text":
          return {};
      }
    })();
    const merged = { ...base, ...typed, ...opts.overrides?.[f] };
    // A tree hangs off its column whatever the type; an identity
    // column already drew its chevron above.
    if (isTreeColumn && spec.type !== "identity") {
      const inner =
        (opts.overrides?.[f]?.params as { innerFormatter?: Formatter<T> } | undefined)
          ?.innerFormatter ??
        merged.formatter ??
        plain;
      merged.formatter = treeCell(inner);
      merged.width = Math.max(merged.width ?? 0, 340);
    }
    return merged;
  });
}
