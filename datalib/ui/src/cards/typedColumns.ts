// The column-type vocabulary, as slickgrid column definitions. A
// table's producer declares a `ColumnSpec` per column; `typedColumns`
// turns each into a `Column` whose cell is drawn by the column's *type*
// — with the renderers in `cellRenderers.ts` — rather than by comparing
// field names. Pure: it knows nothing about what the rows are, and owns
// no grid. `TableGrid.ce.vue` mounts one over these for the simple
// hosts; a card with a grid of its own (`GridCard`) calls this and
// keeps driving its grid itself.
import type {
  Column,
  Formatter,
  GridOption,
  GroupingFormatterItem,
} from "@slickgrid-universal/common";
import { Editors, Filters } from "@slickgrid-universal/common";
import type { Action, ColumnSpec, Identity, StatusView, Chip, Timeseries } from "@/api";
import {
  WIDTH,
  renderChips,
  renderIdentity,
  renderStatus,
  renderTimeseries,
  renderTimestamp,
} from "./cellRenderers";
import { calibrationMax } from "@/config/sparkline";
import { compareStamps, formatStamp } from "@/config/timeFormat";
import { formatBytes } from "@/config/bytes";

export type SlickColumnOptions<T> = {
  /// The rows the columns will draw, read when a cell needs the whole
  /// column — a `timeseries` sparkline is calibrated against the
  /// largest value any row reaches.
  rows: () => T[];
  /// How far back a `timeseries` cell's samples reach, in seconds.
  windowSecs?: number;
  /// The rows form a tree; the tree column (`treeColumnField`) carries
  /// the chevron and shows each row's own id beside its label.
  tree?: boolean;
  /// What each action id does when its button is pressed. An id with
  /// no handler draws no button.
  actions?: Record<string, (row: T) => void>;
  /// A `markdown_uuid` cell was clicked.
  onOpenDocument?: (uuid: string) => void;
  /// Every column can be dragged into the grouping bar. Off, no column
  /// carries a `grouping`, and the bar accepts none.
  groupable?: boolean;
  /// Every column gets a box in the grid's filter row, with the grid's
  /// operator shorthand (`>5`, `a*`, `<>x`). The grid must have
  /// `enableFiltering` on, or it refuses the columns.
  filterable?: boolean;
  /// Per-field refinements a type cannot know — a width, a hover, a
  /// formatter — merged over the typed definition.
  overrides?: Record<string, Partial<Column<T>>>;
};

/// Grid options a grid drawing `filterable` columns must carry. The
/// compound number filter's operator dropdown pads each operator to
/// three characters with `&nbsp;` entities and then, with
/// `enableHtmlRendering` off, sets them as text — so its blank first
/// option read `&nbsp;&nbsp;&nbsp;`. Naming every operator already
/// padded, with no-break spaces, leaves it nothing to add.
const NBSP = "\u00a0";
const pad = (op: string) => ({ operatorAlt: op.padEnd(3, NBSP) });
export const FILTER_GRID_OPTIONS: Pick<GridOption, "compoundOperatorAltTexts"> = {
  compoundOperatorAltTexts: {
    numeric: {
      "": pad(NBSP),
      "=": pad("="),
      "<": pad("<"),
      "<=": pad("<="),
      ">": pad(">"),
      ">=": pad(">="),
      "<>": pad("<>"),
    },
  },
};

/// A group row's title: the column, the value and how many rows share
/// it. An element, not markup: the value is the rows' own text.
export function groupTitle(name: string, show: (value: unknown) => string = String) {
  return (g: GroupingFormatterItem) => {
    const el = document.createElement("span");
    el.className = "tg-group";
    const value = g.value == null || g.value === "" ? "—" : show(g.value);
    el.textContent = `${name}: ${value}`;
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
/// picture for, drawn in `currentColor`. Any other id draws its label.
const ACTION_ICONS: Record<string, string> = {
  // A table: what Browse opens is this row's data as rows and columns.
  browse: "M3 5h18v4H3V5zm0 6h8v8H3v-8zm10 0h8v8h-8v-8z",
  sync: "M8 5v14l11-7z",
  stop: "M6 6h12v12H6z",
};

/// The buttons of an `actions` cell. One element per row, kept across
/// repaints and updated in place: a repaint runs on every job event —
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
        if (glyph) {
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
      if (!ACTION_ICONS[a.id]) b.textContent = a.label;
      b.title = a.enabled ? a.label : (a.disabled_reason ?? a.label);
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
  const windowSecs = opts.windowSecs ?? 300;
  const ceilingOf = (field: string) =>
    calibrationMax(
      opts.rows().map((r) => (r[field] as Timeseries | undefined) ?? { value: null, samples: [] }),
    );

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
      // A column keeps its width when the grid fits itself to its box:
      // the fit shrinks a column down to `minWidth`, and these are as
      // narrow as they read. The tree column is the one that gives.
      width: WIDTH[spec.type],
      minWidth: WIDTH[spec.type],
      hidden: !spec.default_visible,
      sortable: true,
      resizable: true,
      reorderable: true,
      // The tests, and anyone reading the DOM, find a cell by the
      // column it is in.
      cellAttrs: { "col-id": f },
      headerCellAttrs: { "col-id": f },
      ...(opts.filterable ? { filterable: true, filter: { model: Filters.input } } : {}),
      formatter: plain,
      sortComparer: (a, b, dir) => compareText(a, b, dir ?? 1),
      ...(spec.editable && spec.type !== "identity" ? { editor: { model: Editors.text } } : {}),
      ...(opts.groupable
        ? { grouping: { getter: f, formatter: groupTitle(spec.header), collapsed: false } }
        : {}),
    };
    const typed: Partial<Column<T>> = (() => {
      switch (spec.type) {
        case "identity": {
          const label = (v: unknown) => (v as Identity | null)?.label ?? "";
          const inner: Formatter<T> = (_r, _c, _v, _col, row) =>
            renderIdentity(row?.[f] as Identity | null, isTreeColumn, !!row?.__hasChildren);
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
                  minWidth: 300,
                  formatter: treeCell(inner),
                }
              : { formatter: inner }),
            ...(spec.editable ? { editor: { model: Editors.text } } : {}),
            ...(opts.groupable
              ? {
                  grouping: {
                    getter: (row: T) => label(row[f]),
                    formatter: groupTitle(spec.header),
                    collapsed: false,
                  },
                }
              : {}),
          };
        }
        case "status":
          return {
            formatter: (_r, _c, value) => renderStatus(value as StatusView | null),
            sortComparer: (a, b, dir) =>
              compareText(
                (a as StatusView | null)?.label,
                (b as StatusView | null)?.label,
                dir ?? 1,
              ),
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
            formatter: (_r, _c, value) =>
              renderTimeseries(value as Timeseries | null, ceilingOf(f), windowSecs),
            sortComparer: (a, b, dir) =>
              compareNumber(
                (a as Timeseries | null)?.value,
                (b as Timeseries | null)?.value,
                dir ?? 1,
              ),
          };
        case "bytes":
          return {
            cssClass: "tg-right",
            type: "number",
            ...(opts.filterable ? { filter: { model: Filters.compoundInputNumber } } : {}),
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
            ...(opts.filterable ? { filter: { model: Filters.compoundInputNumber } } : {}),
            formatter: (_r, _c, value) => ({
              text: typeof value === "number" ? value.toLocaleString() : "",
            }),
            sortComparer: (a, b, dir) => compareNumber(a, b, dir ?? 1),
          };
        case "number":
          return {
            cssClass: "tg-right",
            type: "number",
            ...(opts.filterable ? { filter: { model: Filters.compoundInputNumber } } : {}),
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
            formatter: (_r, _c, value) => {
              const v = value as Identity | string | null;
              const id = typeof v === "string" ? v : v?.id;
              const a = document.createElement("a");
              a.className = "tg-link";
              a.textContent = typeof v === "string" ? v : (v?.label ?? "");
              a.href = "#";
              a.addEventListener("click", (e) => {
                e.preventDefault();
                e.stopPropagation();
                if (id) opts.onOpenDocument?.(id);
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
      merged.minWidth = Math.min(merged.minWidth ?? 300, 300);
    }
    return merged;
  });
}
