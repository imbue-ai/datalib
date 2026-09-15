<script setup lang="ts" generic="T extends Record<string, unknown>">
// The one typed table viewer. Takes rows plus the `ColumnSpec`s whoever
// served them declared, and draws each cell by its column's type — a
// byte count as a size, a timestamp as "7 days ago", an identity as its
// icon and label, a status as its glyph — rather than by comparing
// field names. Nothing here knows what the rows *are*; the card that
// mounts it supplies the action handlers and any columns of its own.
// AG Grid carries sort, filter, tree data, virtualisation and resize.
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { AgGridVue } from "ag-grid-vue3";
import {
  AllCommunityModule,
  ModuleRegistry,
  colorSchemeVariable,
  themeQuartz,
  type CellDoubleClickedEvent,
  type ColDef,
  type GetContextMenuItemsParams,
  type GridApi,
  type GridOptions,
  type RowClickedEvent,
  type GridReadyEvent,
  type ICellRendererComp,
  type ICellRendererParams,
  type IsGroupOpenByDefaultParams,
  type MenuItemDef,
  type DefaultMenuItem,
  type RowGroupOpenedEvent,
  type RowSelectionOptions,
  type ValueGetterParams,
} from "ag-grid-community";
import { ContextMenuModule, TreeDataModule } from "ag-grid-enterprise";
import type { Action, Chip, ColumnSpec, ColumnType, Identity, StatusView, Timeseries } from "@/api";
import { iconUrl } from "@/config/icons";
import { STATUS_GLYPHS, STEP_GLYPHS, glyphSvg } from "@/config/glyphs";
import { calibrationMax, sparkline, type Sample } from "@/config/sparkline";
import { compareStamps, formatRelative, formatStamp } from "@/config/timeFormat";
import { formatBytes } from "@/config/bytes";

ModuleRegistry.registerModules([AllCommunityModule, TreeDataModule, ContextMenuModule]);
const gridTheme = themeQuartz.withPart(colorSchemeVariable);

const props = withDefaults(
  defineProps<{
    columns: ColumnSpec[];
    rows: T[];
    /// The field holding each row's stable id.
    rowKey?: string;
    /// The rows form a tree, each carrying a `path: string[]`.
    tree?: boolean;
    /// Which tree rows start open; default all closed.
    isGroupOpenByDefault?: (p: IsGroupOpenByDefaultParams<T>) => boolean;
    /// How far back a `timeseries` cell's samples reach, in seconds —
    /// read from the producer so the plot and the data agree.
    windowSecs?: number;
    /// What each action id does when its button is pressed. An id with
    /// no handler here draws no button.
    actions?: Record<string, (row: T) => void>;
    /// Columns the card adds beside the declared ones, AG Grid's way.
    extraColumns?: ColDef<T>[];
    /// Per-field refinements of a declared column, AG Grid's way —
    /// a width, a tooltip, a formatter the type cannot know. Merged
    /// over the typed definition, so a card can also override it.
    columnOverrides?: Record<string, ColDef<T>>;
    contextMenu?: (params: GetContextMenuItemsParams<T>) => (MenuItemDef<T> | DefaultMenuItem)[];
    selectable?: boolean;
    /// Everything else AG Grid takes, for the card that needs it. The
    /// viewer's own settings — columns, rows, theme — win.
    gridOptions?: GridOptions<T>;
    defaultColDef?: ColDef<T>;
  }>(),
  { rowKey: "key", tree: false, windowSecs: 300, selectable: false },
);

const emit = defineEmits<{
  ready: [api: GridApi<T>];
  cellDoubleClick: [row: T, field: string];
  /// An in-place edit of an `editable` cell was committed. The card
  /// decides what it means; the value never lands in the row.
  edit: [row: T, field: string, value: string];
  /// A `markdown_uuid` cell was clicked.
  openDocument: [uuid: string];
  rowClick: [e: RowClickedEvent<T>];
  rowGroupOpened: [e: RowGroupOpenedEvent<T>];
}>();

let gridApi: GridApi<T> | null = null;
function onGridReady(e: GridReadyEvent<T>) {
  gridApi = e.api;
  emit("ready", e.api);
}

/// Repaint the cells whose content lives outside the row's identity
/// — every typed renderer, since AG Grid reuses a cell whose row id
/// is unchanged.
function refreshCells(fields?: string[]) {
  gridApi?.refreshCells({ columns: fields, force: true });
}
defineExpose({ refreshCells, api: () => gridApi });

// ── Icon tokens ──────────────────────────────────────────────────
// A producer names an icon as a token; this is where the token becomes
// a picture. Brand marks are bundled assets; `step:<phase>` and
// `applet` are the pipeline-role glyphs.

function iconFor(token: string | null | undefined, label: string): Element | null {
  if (!token) return null;
  const url = iconUrl(token);
  if (url) {
    const img = document.createElement("img");
    img.src = url;
    img.alt = label;
    img.className = "tg-brand";
    return img;
  }
  const glyph =
    token === "applet"
      ? STEP_GLYPHS.applet
      : token.startsWith("step:")
        ? STEP_GLYPHS[token.slice(5) as keyof typeof STEP_GLYPHS]
        : undefined;
  return glyph ? glyphSvg(glyph, label, 14) : null;
}

// ── Formatting ───────────────────────────────────────────────────

function formatUnit(n: number, unit: string): string {
  return unit === "bytes" ? formatBytes(n) : n.toLocaleString();
}

function none(): HTMLElement {
  const span = document.createElement("span");
  span.className = "tg-none";
  span.textContent = "—";
  return span;
}

// ── Timeseries calibration ───────────────────────────────────────
// Every sparkline in a column is drawn against the same ceiling — the
// largest value any row in that column reaches — so heights compare.

const SPARK = { width: 120, height: 18 };
const windowMs = computed(() => props.windowSecs * 1000);
const windowPhrase = computed(() => {
  const secs = props.windowSecs;
  return secs % 60 === 0
    ? `the last ${secs / 60} minute${secs === 60 ? "" : "s"}`
    : `the last ${secs} seconds`;
});
const ceilings = computed<Record<string, number>>(() => {
  const out: Record<string, number> = {};
  for (const c of props.columns) {
    if (c.type !== "timeseries") continue;
    out[c.field] = calibrationMax(
      props.rows.map((r) => (r[c.field] as Timeseries | undefined) ?? { value: null, samples: [] }),
    );
  }
  return out;
});

function sparkSvg(samples: Sample[], max: number): SVGSVGElement | null {
  const spark = sparkline(samples, {
    nowMs: Date.now(),
    windowMs: windowMs.value,
    max,
    width: SPARK.width,
    height: SPARK.height,
    inset: 0.5,
  });
  if (!spark) return null;
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", `0 0 ${SPARK.width} ${SPARK.height}`);
  svg.setAttribute("preserveAspectRatio", "none");
  svg.setAttribute("aria-hidden", "true");
  svg.classList.add("tg-spark");
  const area = document.createElementNS("http://www.w3.org/2000/svg", "polygon");
  area.setAttribute("points", spark.area);
  area.classList.add("tg-spark-area");
  svg.appendChild(area);
  const line = document.createElementNS("http://www.w3.org/2000/svg", "polyline");
  line.setAttribute("points", spark.line);
  line.classList.add("tg-spark-line");
  svg.appendChild(line);
  return svg;
}

// ── Cell renderers, one per type ─────────────────────────────────

function renderIdentity(v: Identity | null | undefined, isTreeColumn: boolean, isParent: boolean): HTMLElement {
  const wrap = document.createElement("span");
  wrap.className = "tg-identity";
  if (!v) return wrap;
  const icon = iconFor(v.icon, v.detail ?? v.label);
  // A brand mark leads; a role glyph follows the name, muted, because
  // the name is what the eye should land on and the glyph answers the
  // follow-up question.
  const brand = icon?.tagName === "IMG";
  if (icon && brand) wrap.appendChild(icon);
  const text = document.createElement("span");
  text.textContent = v.label;
  if (isParent) text.className = "tg-parent";
  text.title = v.id;
  wrap.appendChild(text);
  if (icon && !brand) {
    const mark = document.createElement("span");
    mark.className = "tg-mark";
    mark.title = v.detail ?? "";
    mark.appendChild(icon);
    wrap.appendChild(mark);
  }
  // The row's own id, where the column is the row's identity and the
  // label hides it.
  if (isTreeColumn && v.id !== v.label) {
    const id = document.createElement("span");
    id.className = "tg-id";
    id.textContent = v.id;
    id.title = `Id — stored in ${v.id}/ under the data root`;
    wrap.appendChild(id);
  }
  return wrap;
}

function renderStatus(s: StatusView | null | undefined): HTMLElement {
  const wrap = document.createElement("span");
  if (!s) return wrap;
  const { key, label } = s;
  wrap.className = `tg-status tg-status-${key.replace(/[\s_]+/g, "-")}`;
  // The word, and then why it is that word. The column is glyphs, so
  // this is the only place either appears.
  wrap.title = s.detail ? `${label} — ${s.detail}` : label;
  const spinnerOrGlyph = () => {
    if (key === "running") {
      // A still frame can't say "still going", so running is the one
      // state drawn rather than glyphed.
      const spin = document.createElement("span");
      spin.className = "tg-spinner";
      spin.setAttribute("role", "img");
      spin.setAttribute("aria-label", label);
      return spin;
    }
    const glyph = STATUS_GLYPHS[key];
    if (glyph) return glyphSvg(glyph, label);
    // A status this sheet hasn't met. Say the word rather than draw
    // nothing — an unknown state is exactly when a reader most needs
    // to know what it was.
    const word = document.createElement("span");
    word.textContent = label;
    word.setAttribute("role", "img");
    word.setAttribute("aria-label", label);
    return word;
  };
  wrap.appendChild(spinnerOrGlyph());
  if (s.segments) {
    // One segment per part, each in its own status colour, the running
    // one pulsing. No arithmetic across parts; the bar *is* the parts.
    const bar = document.createElement("span");
    bar.className = "tg-segs";
    for (const seg of s.segments) {
      const cell = document.createElement("span");
      cell.className = `tg-seg tg-seg-${seg.key.replace(/[\s_]+/g, "-")}`;
      cell.title = `${seg.id}: ${seg.label}`;
      bar.appendChild(cell);
    }
    wrap.appendChild(bar);
  } else if (key === "running" && s.fraction != null) {
    // A bar only when the thing said how much is ahead of it: a bar at
    // an invented fraction claims more than we know.
    const bar = document.createElement("span");
    bar.className = "tg-progress";
    const fill = document.createElement("span");
    fill.style.width = `${s.fraction * 100}%`;
    bar.appendChild(fill);
    wrap.appendChild(bar);
  }
  return wrap;
}

function renderChips(chips: Chip[] | null | undefined): HTMLElement {
  const wrap = document.createElement("span");
  wrap.className = "tg-chips";
  if (!chips?.length) return wrap;
  // The chips are cut at the column's edge; the whole row of them is
  // one hover away.
  wrap.title = chips.map((c) => c.text).join("  ");
  for (const chip of chips) {
    const el = document.createElement("span");
    el.className = `tg-chip tg-chip-${chip.kind}`;
    el.textContent = chip.text;
    el.title = chip.title;
    wrap.appendChild(el);
  }
  return wrap;
}

function renderTimestamp(iso: string | null | undefined): HTMLElement {
  if (!iso) return none();
  const span = document.createElement("span");
  span.textContent = formatRelative(iso, Date.now());
  // The exact stamp, for when "7 days ago" isn't the answer you needed.
  span.title = formatStamp(iso);
  return span;
}

function renderTimeseries(v: Timeseries | null | undefined, field: string): HTMLElement {
  const wrap = document.createElement("span");
  wrap.className = "tg-series";
  if (!v || v.value === null) {
    // Null is "nothing measured yet", which is not a flat line at zero
    // — it's the absence of a plot.
    wrap.appendChild(none());
    if (v?.detail) wrap.title = v.detail;
    return wrap;
  }
  wrap.title = `${v.detail ?? formatUnit(v.value, v.unit)} · the line is ${windowPhrase.value}, drawn against the largest row`;
  const track = document.createElement("span");
  track.className = "tg-plot";
  // No samples yet means the producer hasn't measured twice. The value
  // still shows; there is just nothing behind it to draw.
  const svg = sparkSvg(v.samples, ceilings.value[field] ?? 0);
  if (svg) track.appendChild(svg);
  const label = document.createElement("span");
  label.className = "tg-plot-label";
  label.textContent = formatUnit(v.value, v.unit);
  track.appendChild(label);
  wrap.appendChild(track);
  return wrap;
}

/// 24×24 Material-ish glyphs for the action ids the viewer knows a
/// picture for, drawn in `currentColor`. Any other id draws its label.
const ACTION_ICONS: Record<string, string> = {
  sync: "M8 5v14l11-7z",
  stop: "M6 6h12v12H6z",
};

/// A class rather than a function so that `refresh` can update the
/// buttons in place: a repaint runs on every job event — a few times a
/// second during a sync — and a function renderer is torn down and
/// rebuilt on each, so a click whose mousedown landed on the old button
/// and mouseup on its replacement fired nothing.
class ActionsRenderer implements ICellRendererComp<T> {
  private wrap!: HTMLSpanElement;
  private row!: T;
  private field!: string;
  private buttons = new Map<string, HTMLButtonElement>();

  init(p: ICellRendererParams<T>): void {
    this.row = p.data!;
    this.field = p.colDef?.field ?? "";
    this.wrap = document.createElement("span");
    this.wrap.className = "tg-actions";
    this.apply();
  }
  getGui(): HTMLElement {
    return this.wrap;
  }
  refresh(p: ICellRendererParams<T>): boolean {
    this.row = p.data!;
    this.apply();
    return true;
  }
  private apply(): void {
    const actions = (this.row[this.field] as Action[] | undefined) ?? [];
    const wanted = new Set<string>();
    for (const a of actions) {
      const handler = props.actions?.[a.id];
      if (!handler) continue;
      wanted.add(a.id);
      let b = this.buttons.get(a.id);
      if (!b) {
        b = document.createElement("button");
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
        b.addEventListener("click", (e) => {
          e.stopPropagation();
          // The row current at click time, not the one the button was
          // built for.
          props.actions?.[a.id]?.(this.row);
        });
        this.buttons.set(a.id, b);
      }
      if (!ACTION_ICONS[a.id]) b.textContent = a.label;
      b.title = a.enabled ? a.label : (a.disabled_reason ?? a.label);
      b.setAttribute("aria-label", a.label);
      b.disabled = !a.enabled;
      b.classList.toggle("danger", !!a.danger);
      this.wrap.appendChild(b);
    }
    for (const [id, b] of this.buttons) {
      if (!wanted.has(id)) {
        b.remove();
        this.buttons.delete(id);
      }
    }
  }
}

// ── Column definitions, from the specs ───────────────────────────

const WIDTH: Record<ColumnType, number> = {
  text: 150,
  count: 90,
  number: 90,
  bytes: 110,
  timestamp: 150,
  datetime: 165,
  timeseries: 140,
  identity: 120,
  status: 96,
  chips: 260,
  actions: 64,
  markdown_uuid: 200,
};

function colDef(spec: ColumnSpec, index: number): ColDef<T> {
  const f = spec.field;
  const isTreeColumn = props.tree && index === 0;
  const base: ColDef<T> = {
    field: f as ColDef<T>["field"],
    colId: f,
    headerName: spec.header,
    headerTooltip: spec.description,
    width: WIDTH[spec.type],
    minWidth: Math.min(WIDTH[spec.type], 70),
    hide: !spec.default_visible,
  };
  if (spec.editable) {
    // The value never lands in the row: the card hears the edit and
    // whatever it writes comes back with the next rows.
    base.editable = true;
    base.cellEditor = "agTextCellEditor";
    base.valueSetter = (p) => {
      const next = String(p.newValue ?? "").trim();
      if (p.data && next !== String(p.oldValue ?? "")) emit("edit", p.data, f, next);
      return false;
    };
  }
  const typed: ColDef<T> = (() => {
    switch (spec.type) {
      case "identity": {
        const inner = (p: ICellRendererParams<T>) =>
          renderIdentity(
            p.data?.[f] as Identity | null,
            isTreeColumn,
            !!p.node?.allChildrenCount,
          );
        return {
          // Sort, filter and edit on the label, not the object.
          valueGetter: (p: ValueGetterParams<T>) => (p.data?.[f] as Identity | null)?.label ?? "",
          valueSetter: base.valueSetter,
          ...(isTreeColumn
            ? {
                // The column the tree hangs off: AG Grid's group
                // renderer draws the chevron and the indent, and hands
                // the cell's content to the renderer.
                flex: 1,
                minWidth: 200,
                showRowGroup: true,
                cellRenderer: "agGroupCellRenderer",
                cellRendererParams: { suppressCount: true, innerRenderer: inner },
              }
            : { cellRenderer: inner }),
        };
      }
      case "status":
        return {
          valueGetter: (p: ValueGetterParams<T>) => (p.data?.[f] as StatusView | null)?.label ?? "",
          cellRenderer: (p: ICellRendererParams<T>) => renderStatus(p.data?.[f] as StatusView | null),
        };
      case "chips":
        return {
          valueGetter: (p: ValueGetterParams<T>) =>
            ((p.data?.[f] as Chip[] | null) ?? []).map((c) => c.text).join("  "),
          cellRenderer: (p: ICellRendererParams<T>) => renderChips(p.data?.[f] as Chip[] | null),
        };
      case "timestamp":
        return {
          // Sort on the instant: an ISO string carrying its own offset
          // does not compare correctly as text.
          comparator: compareStamps,
          cellRenderer: (p: ICellRendererParams<T>) => renderTimestamp(p.data?.[f] as string | null),
        };
      case "timeseries":
        return {
          valueGetter: (p: ValueGetterParams<T>) => (p.data?.[f] as Timeseries | null)?.value ?? null,
          cellRenderer: (p: ICellRendererParams<T>) => renderTimeseries(p.data?.[f] as Timeseries | null, f),
        };
      case "bytes":
        return {
          cellStyle: { "text-align": "right" },
          valueFormatter: (p) => (typeof p.value === "number" ? formatBytes(p.value) : ""),
          tooltipValueGetter: (p) => (typeof p.value === "number" ? `${p.value.toLocaleString()} bytes` : ""),
        };
      case "count":
        return {
          cellStyle: { "text-align": "right" },
          valueFormatter: (p) => (typeof p.value === "number" ? p.value.toLocaleString() : ""),
        };
      case "number":
        return {
          cellStyle: { "text-align": "right" },
          valueFormatter: (p) =>
            typeof p.value === "number"
              ? p.value.toLocaleString(undefined, { maximumFractionDigits: 3 })
              : "",
        };
      case "datetime":
        return {
          comparator: compareStamps,
          valueFormatter: (p) => (typeof p.value === "string" && p.value ? formatStamp(p.value) : ""),
        };
      case "actions":
        return {
          sortable: false,
          filter: false,
          resizable: false,
          valueGetter: (p: ValueGetterParams<T>) => p.data?.[props.rowKey],
          cellRenderer: ActionsRenderer,
        };
      case "markdown_uuid":
        return {
          valueGetter: (p: ValueGetterParams<T>) => {
            const v = p.data?.[f] as Identity | string | null;
            return typeof v === "string" ? v : (v?.label ?? "");
          },
          cellRenderer: (p: ICellRendererParams<T>) => {
            const v = p.data?.[f] as Identity | string | null;
            const id = typeof v === "string" ? v : v?.id;
            const a = document.createElement("a");
            a.className = "tg-link";
            a.textContent = typeof v === "string" ? v : (v?.label ?? "");
            a.href = "#";
            a.addEventListener("click", (e) => {
              e.preventDefault();
              e.stopPropagation();
              if (id) emit("openDocument", id);
            });
            return a;
          },
        };
      case "text":
        return {};
    }
  })();
  return { ...base, ...typed, ...props.columnOverrides?.[f] };
}

const columnDefs = computed<ColDef<T>[]>(() => [
  ...props.columns.map(colDef),
  ...(props.extraColumns ?? []),
]);

const rowSelection = computed<RowSelectionOptions<T> | undefined>(() =>
  props.selectable
    ? { mode: "multiRow", checkboxes: false, headerCheckbox: false, enableClickSelection: true }
    : undefined,
);

function onCellDoubleClicked(e: CellDoubleClickedEvent<T>) {
  if (e.data) emit("cellDoubleClick", e.data, e.column.getColId());
}

// A `timestamp` cell reads "5 minutes ago", which goes stale on its
// own, so it needs a clock rather than an event. It ticks every second
// but repaints only when a cell would actually read differently.
const timestampFields = computed(() => props.columns.filter((c) => c.type === "timestamp").map((c) => c.field));
let lastRelativePaint = "";
let relativePoll: ReturnType<typeof setInterval> | null = null;
function tickRelative() {
  const fields = timestampFields.value;
  if (fields.length === 0) return;
  const now = Date.now();
  const next = props.rows
    .map((r) => fields.map((f) => formatRelative((r[f] as string | null) ?? null, now)).join(""))
    .join(" ");
  if (next !== lastRelativePaint) {
    lastRelativePaint = next;
    refreshCells(fields);
  }
}
onMounted(() => {
  relativePoll = setInterval(tickRelative, 1000);
});
onBeforeUnmount(() => {
  if (relativePoll) clearInterval(relativePoll);
  gridApi = null;
});

// Typed cells are `cellRenderer`s over data outside the row's
// identity, so a new set of rows only reaches the screen if the cells
// are told to repaint.
watch(
  () => props.rows,
  () => refreshCells(),
);

const rowKeyOf = (p: { data: T }) => String(p.data[props.rowKey]);
const pathOf = (r: T) => r.path as string[];
</script>

<template>
  <div class="tg-root">
    <AgGridVue
      class="tg-grid"
      :gridOptions="gridOptions"
      :defaultColDef="defaultColDef"
      :theme="gridTheme"
      :columnDefs="columnDefs"
      :rowData="rows"
      :getRowId="rowKeyOf"
      :treeData="tree"
      treeDataDisplayType="custom"
      :getDataPath="tree ? pathOf : undefined"
      :groupDefaultExpanded="0"
      :isGroupOpenByDefault="isGroupOpenByDefault"
      :tooltipShowDelay="200"
      :rowSelection="rowSelection"
      :preventDefaultOnContextMenu="!!contextMenu"
      :getContextMenuItems="contextMenu"
      @grid-ready="onGridReady"
      @cell-double-clicked="onCellDoubleClicked"
      @row-clicked="(e: RowClickedEvent<T>) => emit('rowClick', e)"
      @row-group-opened="(e: RowGroupOpenedEvent<T>) => emit('rowGroupOpened', e)"
    />
  </div>
</template>
