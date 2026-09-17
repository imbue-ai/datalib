// The column-type vocabulary as slickgrid-vue column definitions: the
// same `ColumnSpec` → column mapping `typedColumns.ts` makes for AG
// Grid, drawn with the same cell renderers, for the cards that have
// moved to slickgrid. Pure, like its sibling: it knows nothing about
// what the rows are and owns no grid. The two files exist side by side
// only while both grids are in the tree; this one is what stays.
import type { Column, Formatter, GroupingFormatterItem } from "slickgrid-vue";
import type { ColumnSpec, Identity, StatusView, Chip, Timeseries } from "@/api";
import {
  WIDTH,
  renderChips,
  renderIdentity,
  renderStatus,
  renderTimeseries,
  renderTimestamp,
} from "./typedColumns";
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
  /// A `markdown_uuid` cell was clicked.
  onOpenDocument?: (uuid: string) => void;
  /// Every column can be dragged into the grouping bar. Off, no column
  /// carries a `grouping`, and the bar accepts none.
  groupable?: boolean;
  /// Per-field refinements a type cannot know — a width, a hover, a
  /// formatter — merged over the typed definition.
  overrides?: Record<string, Partial<Column<T>>>;
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

function text(value: unknown): string {
  return value == null ? "" : String(value);
}

const plain: Formatter = (_r, _c, value) => ({ text: text(value), toolTip: text(value) });

export function typedSlickColumns<T extends Record<string, unknown>>(
  specs: ColumnSpec[],
  opts: SlickColumnOptions<T>,
): Column<T>[] {
  const windowSecs = opts.windowSecs ?? 300;
  const ceilingOf = (field: string) =>
    calibrationMax(
      opts.rows().map((r) => (r[field] as Timeseries | undefined) ?? { value: null, samples: [] }),
    );

  return specs.map((spec) => {
    const f = spec.field;
    const base: Column<T> = {
      id: f,
      // A dotted path type the spec's plain field name cannot satisfy.
      field: f as Column<T>["field"],
      name: spec.header,
      toolTip: spec.description,
      width: WIDTH[spec.type],
      minWidth: Math.min(WIDTH[spec.type], 70),
      hidden: !spec.default_visible,
      sortable: true,
      resizable: true,
      reorderable: true,
      // The tests, and anyone reading the DOM, find a cell by the
      // column it is in, the way AG Grid's markup let them.
      cellAttrs: { "col-id": f },
      headerCellAttrs: { "col-id": f },
      formatter: plain,
      sortComparer: (a, b, dir) => compareText(a, b, dir ?? 1),
      ...(opts.groupable
        ? { grouping: { getter: f, formatter: groupTitle(spec.header), collapsed: false } }
        : {}),
    };
    const typed: Partial<Column<T>> = (() => {
      switch (spec.type) {
        case "identity": {
          const label = (v: unknown) => (v as Identity | null)?.label ?? "";
          return {
            formatter: (_r, _c, value) => renderIdentity(value as Identity | null, false, false),
            sortComparer: (a, b, dir) => compareText(label(a), label(b), dir ?? 1),
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
              compareText((a as StatusView | null)?.label, (b as StatusView | null)?.label, dir ?? 1),
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
              compareNumber((a as Timeseries | null)?.value, (b as Timeseries | null)?.value, dir ?? 1),
          };
        case "bytes":
          return {
            cssClass: "tg-right",
            formatter: (_r, _c, value) => ({
              text: typeof value === "number" ? formatBytes(value) : "",
              toolTip: typeof value === "number" ? `${value.toLocaleString()} bytes` : "",
            }),
            sortComparer: (a, b, dir) => compareNumber(a, b, dir ?? 1),
          };
        case "count":
          return {
            cssClass: "tg-right",
            formatter: (_r, _c, value) => ({
              text: typeof value === "number" ? value.toLocaleString() : "",
            }),
            sortComparer: (a, b, dir) => compareNumber(a, b, dir ?? 1),
          };
        case "number":
          return {
            cssClass: "tg-right",
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
          // Buttons that repaint in place are the AG Grid renderer's
          // job; no card on this grid has an actions column yet.
          return { sortable: false, resizable: false, formatter: () => "" };
        case "text":
          return {};
      }
    })();
    return { ...base, ...typed, ...opts.overrides?.[f] };
  });
}
