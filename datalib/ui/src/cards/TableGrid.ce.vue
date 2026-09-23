<script setup lang="ts" generic="T extends Record<string, unknown>">
// A grid over `typedColumns`, for the hosts that want a table and
// nothing more: rows plus the specs their producer declared in, cells
// drawn by type out. Tree data, in-place edit, a right-click menu, and
// the clock that keeps "5 minutes ago" honest. A card that drives a
// grid itself — its own selection, column state, sort — calls
// `typedColumns` directly instead of mounting this.
//
// The grid is `@slickgrid-universal/vanilla-bundle`, built into this
// component's own element: a card is a custom element, and the bundle
// is the one layer that takes an element rather than looking a
// selector up on `document`, which cannot see into a shadow root.
import { onBeforeUnmount, onMounted, ref, watch } from "vue";
import { SlickVanillaGridBundle } from "@slickgrid-universal/vanilla-bundle";
import type {
  Column,
  GridOption,
  OnBeforeEditCellEventArgs,
  OnCellChangeEventArgs,
  OnDblClickEventArgs,
  SlickEventData,
  TreeToggleStateChange,
} from "@slickgrid-universal/common";
import type { ColumnSpec, Timeseries } from "@/api";
import { calibrationMax } from "@/config/sparkline";
import { carryLayout, KEEP_COLUMN_WIDTHS } from "@/grid/columnLayout";
import { clockFaces, movedCells, type ClockFaces } from "@/grid/clockFaces";
import { menuSlots, type MenuEntry } from "@/grid/menu";
import { stampRowKeys } from "@/grid/rowKeys";
import { treeColumnField, typedColumns } from "./typedColumns";
import type { TableGridApi } from "./tableGridApi";
import { fieldsOfType, sparkStepMs } from "./cellRenderers";

const props = withDefaults(
  defineProps<{
    columns: ColumnSpec[];
    rows: T[];
    /// The field holding each row's stable id.
    rowKey?: string;
    /// The rows form a tree, each carrying a `path: string[]`.
    tree?: boolean;
    /// Which tree rows start open; default all closed.
    openByDefault?: (row: T) => boolean;
    /// How far back a `timeseries` cell's samples reach, in seconds —
    /// read from the producer so the plot and the data agree.
    windowSecs?: number;
    /// What each action id does when its button is pressed. An id with
    /// no handler here draws no button.
    actions?: Record<string, (row: T) => void>;
    /// The right-click menu for a row: the row under the click, the
    /// rows the click aims at (the selection when the row is in it),
    /// and the column it landed on.
    menu?: (anchor: T, targets: T[], column: string) => MenuEntry[];
    selectable?: boolean;
    /// Whether rows outside the viewport are left unrendered. Off, every
    /// row is in the DOM whether or not it is scrolled into view — right
    /// for a table of tens of rows, wrong for one of thousands.
    virtualizeRows?: boolean;
    /// Per-field refinements a type cannot know — a width, a formatter
    /// — merged over the typed definition.
    columnOverrides?: Record<string, Partial<Column<T>>>;
  }>(),
  { rowKey: "key", tree: false, windowSecs: 300, selectable: false, virtualizeRows: true },
);

const emit = defineEmits<{
  ready: [api: TableGridApi<T>];
  cellDoubleClick: [row: T, field: string];
  /// An in-place edit of an `editable` cell was committed. The card
  /// decides what it means; the value never lands in the row.
  edit: [row: T, field: string, value: string];
  /// A `markdown_uuid` cell was clicked.
  openDocument: [uuid: string];
  /// A tree row was opened or closed.
  rowGroupOpened: [row: T, expanded: boolean];
}>();

/// The row's key.
const keyOf = (row: T): string => String(row[props.rowKey]);

const boxEl = ref<HTMLDivElement | null>(null);
type Grid = SlickVanillaGridBundle<T> & {
  dataView: NonNullable<SlickVanillaGridBundle<T>["dataView"]>;
  slickGrid: NonNullable<SlickVanillaGridBundle<T>["slickGrid"]>;
};
let bundle: Grid | null = null;

/// The tree's parent link, the grid's name for it, and whether each
/// row is folded. The grid mutates the rows it is given (it hangs its
/// own bookkeeping on them), so it is handed copies.
const PARENT = "treeParentKey";
const COLLAPSED = "__collapsed";
/// The order the rows came in, which is the order a tree shows them in
/// until a header is clicked: the grid sorts a tree by its first
/// column unless told which column, and this hidden one is the telling.
const ORDER = "treeOrder";
const collapsedByKey = new Map<string, boolean>();

function annotate(rows: T[]): T[] {
  if (!props.tree) return rows.map((r) => ({ ...r }));
  const keyByPath = new Map<string, string>();
  for (const r of rows) keyByPath.set((r.path as string[]).join("\n"), keyOf(r));
  return rows.map((r, i) => {
    const path = r.path as string[];
    const parent = path.length > 1 ? (keyByPath.get(path.slice(0, -1).join("\n")) ?? null) : null;
    const key = keyOf(r);
    const collapsed = collapsedByKey.get(key) ?? !(props.openByDefault?.(r) ?? false);
    return { ...r, [PARENT]: parent, [COLLAPSED]: collapsed, [ORDER]: i };
  });
}

/// What each row last painted as, by key, so a new set of rows only
/// repaints the rows that changed: the grid rebuilds a row's cells on
/// an update, and the action buttons live in those cells.
const painted = new Map<string, string>();
/// The keys of the rows last handed over, in the host's order. Not the
/// grid's own order, which a sort changes: compared against that, every
/// update after a header click would read as a new set of rows and
/// reset the sort.
let handed: string[] = [];

function syncRows(rows: T[]) {
  if (!bundle) return;
  const { dataView } = bundle;
  const keys = rows.map(keyOf);
  const sameShape = keys.length === handed.length && keys.every((k, i) => k === handed[i]);
  handed = keys;
  if (!sameShape) {
    painted.clear();
    for (const r of rows) painted.set(keyOf(r), JSON.stringify(r));
    ceilings = ceilingsOf(rows);
    bundle.dataset = annotate(rows);
    return;
  }
  // A row whose cell is being edited is left as it is: the grid drops
  // the editor when it rebuilds that row. The change stays unpainted,
  // so the next sync after the edit closes applies it.
  const busy = editingCell()?.row ?? null;
  dataView.beginUpdate();
  for (const r of rows) {
    const key = keyOf(r);
    const json = JSON.stringify(r);
    if (painted.get(key) === json) continue;
    if (busy != null && dataView.getRowById(key) === busy) continue;
    painted.set(key, json);
    // The grid's own bookkeeping on the row — its level, its parent,
    // whether it is folded — rides along.
    dataView.updateItem(key, { ...dataView.getItemById(key), ...r });
  }
  dataView.endUpdate();
  const next = ceilingsOf(rows);
  if (next !== ceilings) {
    ceilings = next;
    refreshCells(fieldsOfType(props.columns, "timeseries"));
  }
}

/// Every sparkline in a column is drawn against the column's largest
/// row, so a new largest moves every line in it: the column is
/// repainted, not the rows.
let ceilings = "";
function ceilingsOf(rows: T[]): string {
  const series = fieldsOfType(props.columns, "timeseries").map((f) =>
    calibrationMax(rows.map((r) => (r[f] as Timeseries | undefined) ?? EMPTY_SERIES)),
  );
  return JSON.stringify(series);
}
const EMPTY_SERIES: Timeseries = { value: null, unit: "", samples: [] };

/// The cell with an open editor, if any.
function editingCell(): { row: number; cell: number } | null {
  const grid = bundle?.slickGrid;
  if (!grid?.getCellEditor()) return null;
  return grid.getActiveCell() ?? null;
}

/// Redraw single cells in place, leaving the rest of each row alone: a
/// row rebuilt under the pointer loses the button it was about to
/// press. A cell being edited is skipped, because redrawing it resets
/// what the person typed.
function repaintCells(cells: { key: string; field: string }[]) {
  if (!bundle) return;
  const { slickGrid: grid, dataView } = bundle;
  const busy = editingCell();
  for (const { key, field } of cells) {
    const row = dataView.getRowById(key);
    const cell = grid.getColumnIndex(field);
    if (row == null || cell == null || cell < 0) continue;
    if (busy?.row === row && busy.cell === cell) continue;
    grid.updateCell(row, cell);
  }
}

/// Redraw the named columns in every row.
function refreshCells(fields: string[]) {
  repaintCells(props.rows.flatMap((r) => fields.map((field) => ({ key: keyOf(r), field }))));
}

function selectedRows(): T[] {
  if (!bundle) return [];
  const { slickGrid, dataView } = bundle;
  return slickGrid
    .getSelectedRows()
    .sort((a, b) => a - b)
    .map((i) => dataView.getItem(i) as T | undefined)
    .filter((r): r is T => r != null);
}

function startEditing(row: T, field: string) {
  if (!bundle) return;
  const { slickGrid, dataView } = bundle;
  const idx = dataView.getRowById(keyOf(row));
  const cell = slickGrid.getColumnIndex(field);
  if (idx == null || cell == null) return;
  slickGrid.gotoCell(idx, cell, true);
}

const api: TableGridApi<T> = { startEditing, selectedRows, refreshCells };
defineExpose({ api: () => api });

function buildColumns(): Column<T>[] {
  const typed = typedColumns<T>(props.columns, {
    rows: () => props.rows,
    tree: props.tree,
    windowSecs: props.windowSecs,
    actions: props.actions,
    onOpenDocument: (uuid) => emit("openDocument", uuid),
    overrides: props.columnOverrides,
  });
  if (!props.tree) return typed;
  return [
    ...typed,
    {
      id: ORDER,
      field: ORDER as Column<T>["field"],
      name: "",
      hidden: true,
      type: "number",
      excludeFromColumnPicker: true,
    },
  ];
}

/// The menu's entries for the click in hand: the row under it, and the
/// selection when the row is part of it.
function entriesFor(args: { row?: number; cell?: number }): MenuEntry[] {
  if (!bundle || !props.menu || args.row == null) return [];
  const anchor = bundle.dataView.getItem(args.row) as T | undefined;
  if (!anchor) return [];
  const selected = selectedRows();
  const targets = selected.some((r) => keyOf(r) === keyOf(anchor)) ? selected : [anchor];
  const column = String(bundle.slickGrid.getColumns()[args.cell ?? -1]?.id ?? "");
  return props.menu(anchor, targets, column);
}

function isDark(): boolean {
  return document.documentElement.dataset.theme === "dark";
}

function options(): GridOption {
  const treeField = treeColumnField(props.columns);
  return {
    datasetIdPropertyName: props.rowKey,
    enableHtmlRendering: false,
    enableEmptyDataWarningMessage: false,
    darkMode: isDark(),
    enableAutoResize: true,
    ...KEEP_COLUMN_WIDTHS,
    autoResize: {
      container: boxEl.value!.parentElement!,
      calculateAvailableSizeBy: "container",
      resizeDetection: "container",
      autoHeight: false,
      bottomPadding: 0,
      minHeight: 120,
    },
    rowHeight: 34,
    enableTextSelectionOnCells: true,
    enableCellNavigation: true,
    enableSelection: props.selectable,
    multiSelect: props.selectable,
    selectionOptions: { selectActiveRow: props.selectable },
    // An `editable` cell opens on double-click or Enter; the value it
    // takes is handed to the host and put back (see `onCellChange`).
    editable: true,
    autoEdit: false,
    enableSorting: true,
    multiColumnSort: false,
    enableColumnReorder: true,
    enableHeaderMenu: false,
    enableGridMenu: false,
    enableColumnPicker: false,
    // Folding a tree row goes through the grid's filters, so filtering
    // is on; nothing here is filterable, and the filter row stays hidden.
    enableFiltering: true,
    showHeaderRow: false,
    // A cell's value by its field, which for an identity is a path into
    // the object (`name.label`).
    dataItemColumnValueExtractor: (item, col) => {
      const field = String(col.field ?? "");
      if (!field.includes(".")) return item[field];
      return field
        .split(".")
        .reduce<unknown>((v, k) => (v as Record<string, unknown> | undefined)?.[k], item);
    },
    ...(props.tree
      ? {
          enableTreeData: true,
          treeDataOptions: {
            columnId: treeField,
            parentPropName: PARENT,
            collapsedPropName: COLLAPSED,
            initialSort: { columnId: ORDER, direction: "ASC" },
          },
        }
      : {}),
    enableContextMenu: !!props.menu,
    contextMenu: {
      // The menu stays put while the grid scrolls: the scroll that
      // brought a row into view can report after the click on it.
      hideMenuOnScroll: false,
      hideCopyCellValueCommand: true,
      hideCommands: ["copy", "clear-grouping", "collapse-all-groups", "expand-all-groups"],
      commandItems: menuSlots(24, entriesFor),
    },
  };
}

/// The value a cell held before an edit, put back once the host has
/// heard the new one: the value never lands in the row.
let editing: { item: T; field: string; before: unknown } | null = null;

function onBeforeEditCell(_e: SlickEventData, args: OnBeforeEditCellEventArgs) {
  const field = String(args.column?.field ?? "");
  editing = { item: args.item as T, field, before: readPath(args.item, field) };
}

function onCellChange(_e: SlickEventData, args: OnCellChangeEventArgs) {
  if (!bundle || !editing) return;
  const { item, field, before } = editing;
  editing = null;
  const next = String(readPath(item, field) ?? "").trim();
  writePath(item, field, before);
  bundle.slickGrid.updateRow(args.row);
  if (next !== String(before ?? "")) emit("edit", item, field.split(".")[0], next);
}

function readPath(item: unknown, field: string): unknown {
  return field
    .split(".")
    .reduce<unknown>((v, k) => (v as Record<string, unknown> | undefined)?.[k], item);
}

function writePath(item: unknown, field: string, value: unknown) {
  const parts = field.split(".");
  const last = parts.pop()!;
  const target = parts.reduce<unknown>(
    (v, k) => (v as Record<string, unknown> | undefined)?.[k],
    item,
  );
  if (target && typeof target === "object") (target as Record<string, unknown>)[last] = value;
}

function onDblClick(_e: SlickEventData, args: OnDblClickEventArgs) {
  if (!bundle) return;
  const row = bundle.dataView.getItem(args.row) as T | undefined;
  const column = bundle.slickGrid.getColumns()[args.cell];
  if (row && column) emit("cellDoubleClick", row, String(column.id));
}

function onTreeToggled(change: TreeToggleStateChange) {
  if (!bundle) return;
  const key = String(change.fromItemId ?? "");
  const toggled = change.toggledItems?.find((t) => String(t.itemId) === key);
  if (!key || !toggled) return;
  collapsedByKey.set(key, toggled.isCollapsed);
  const row = bundle.dataView.getItemById(key) as T | undefined;
  if (row) emit("rowGroupOpened", row, !toggled.isCollapsed);
}

/// Built once the box is on the page, the producer has declared its
/// columns and, for a tree, the first rows are in — whichever comes
/// last. A grid built with no columns stays a 2px strip, and a tree
/// built with no rows is refused outright.
/// The column specs the grid was last given, as JSON.
let declared = "";

function createGrid() {
  if (bundle || !boxEl.value || props.columns.length === 0) return;
  if (props.tree && props.rows.length === 0) return;
  declared = JSON.stringify(props.columns);
  const opts = options();
  const root = boxEl.value.getRootNode();
  if (root instanceof ShadowRoot) opts.shadowRoot = root;
  const b = new SlickVanillaGridBundle<T>(
    boxEl.value,
    buildColumns(),
    opts,
    annotate(props.rows),
  ) as Grid;
  bundle = b;
  // The grid, reachable from its element for anyone debugging in the
  // inspector.
  (boxEl.value as HTMLDivElement & { __grid?: unknown; __api?: unknown }).__grid = b;
  (boxEl.value as HTMLDivElement & { __api?: unknown }).__api = api;
  if (!props.virtualizeRows) {
    // The grid renders the rows in view plus a viewport's worth beyond,
    // whatever its buffer options say; a table asked to show every row
    // gets the whole range.
    const grid = b.slickGrid;
    const ranged = grid.getRenderedRange.bind(grid);
    grid.getRenderedRange = (top?: number, left?: number) => {
      const range = ranged(top, left);
      range.top = 0;
      range.bottom = Math.max(range.bottom, grid.getDataLength() - 1);
      return range;
    };
  }
  for (const r of props.rows) painted.set(keyOf(r), JSON.stringify(r));
  handed = props.rows.map(keyOf);
  ceilings = ceilingsOf(props.rows);
  stampRowKeys(b.slickGrid, b.dataView, (item) => keyOf(item as T));
  b.slickGrid.onBeforeEditCell.subscribe(onBeforeEditCell);
  b.slickGrid.onCellChange.subscribe(onCellChange);
  b.slickGrid.onDblClick.subscribe(onDblClick);
  b.instances?.eventPubSubService?.subscribe<TreeToggleStateChange>(
    "onTreeItemToggled",
    onTreeToggled,
  );
  emit("ready", api);
}

// Some cells go stale with no new data ("5 minutes ago", a sliding
// sparkline), so they need a clock rather than an event. It ticks every
// second and repaints only the cells that would draw differently.
let faces: ClockFaces = new Map();
let clock: ReturnType<typeof setInterval> | null = null;
function tickClock() {
  const timestamps = fieldsOfType(props.columns, "timestamp");
  const timeseries = fieldsOfType(props.columns, "timeseries");
  if (timestamps.length === 0 && timeseries.length === 0) return;
  const cols = {
    timestamps,
    timeseries,
    windowMs: props.windowSecs * 1000,
    stepMs: sparkStepMs(props.windowSecs),
  };
  const next = clockFaces(props.rows, keyOf, cols, Date.now());
  const moved = movedCells(faces, next);
  faces = next;
  repaintCells(moved);
}

let themeWatch: MutationObserver | null = null;
onMounted(() => {
  createGrid();
  clock = setInterval(tickClock, 1000);
  themeWatch = new MutationObserver(() => bundle?.setDarkMode(isDark()));
  themeWatch.observe(document.documentElement, {
    attributes: true,
    attributeFilter: ["data-theme"],
  });
});
onBeforeUnmount(() => {
  if (clock) clearInterval(clock);
  themeWatch?.disconnect();
  bundle?.dispose();
  bundle = null;
});

watch(
  () => props.rows,
  (rows) => {
    if (bundle) syncRows(rows);
    else createGrid();
  },
);
watch(
  () => props.columns,
  (columns) => {
    // A host may hand over an equal array on every poll. Only a real
    // change reaches the grid, and it keeps the widths and order a
    // person set.
    const next = JSON.stringify(columns);
    if (next === declared) return;
    declared = next;
    if (bundle)
      bundle.columnDefinitions = carryLayout(buildColumns(), bundle.slickGrid.getColumns());
    else createGrid();
  },
);
</script>

<template>
  <div class="tg-root">
    <div ref="boxEl" class="tg-grid" />
  </div>
</template>
