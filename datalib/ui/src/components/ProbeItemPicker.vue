<script setup lang="ts">
// The picker behind a filter field whose values a probe can enumerate:
// an account's mailboxes, its conversations (a Claude chat, a Slack
// DM), a workspace's channels. One scrollable grid with a checkbox per
// row, and the checked set *is* the field's value — the text box
// beside it edits the same array. What the columns are follows from
// what kind of item the list holds; nothing else differs per source.
//
// A value the probe has never heard of is kept rather than unchecked
// away: it is how a hand-typed label survives touching this grid, and
// the field says separately that the account does not have it.
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import { SlickVanillaGridBundle } from "@slickgrid-universal/vanilla-bundle";
import type {
  Column,
  GridOption,
  OnSelectedRowsChangedEventArgs,
  SlickEventData,
} from "@slickgrid-universal/common";
import type { ProbeItem, ProbeItemKind } from "@/api";
import { KEEP_COLUMN_WIDTHS } from "@/grid/columnLayout";
import { stampRowKeys } from "@/grid/rowKeys";

const props = defineProps<{
  items: ProbeItem[];
  /// The field's array: every value chosen, probed or typed.
  modelValue: string[];
}>();
const emit = defineEmits<{ (e: "update:modelValue", value: string[]): void }>();

const query = ref("");
const boxEl = ref<HTMLDivElement | null>(null);
type Grid = SlickVanillaGridBundle<ProbeItem> & {
  dataView: NonNullable<SlickVanillaGridBundle<ProbeItem>["dataView"]>;
  slickGrid: NonNullable<SlickVanillaGridBundle<ProbeItem>["slickGrid"]>;
};
let bundle: Grid | null = null;

/// The item's name as a person reads it: the title where the path is
/// an opaque id, the path itself where it is its own name.
const byTitle = (item: ProbeItem) => item.title || item.path || "";
/// The date the source stamped, not one re-derived in this browser's
/// zone — see AGENTS.md's timestamp convention.
const byDate = (item: ProbeItem) => item.updated_at?.slice(0, 10) ?? "";

const text = (v: unknown) => ({ text: v == null ? "" : String(v) });

type Layout = { placeholder: string; columns: Column<ProbeItem>[] };

/// A mailbox is named by the very string the filter matches, so it
/// needs no second column for it. A keyword sits in the same list — a
/// Gmail flag reads as a label — so it shares the layout.
const LABELS: Layout = {
  placeholder: "Search these labels…",
  columns: [
    {
      id: "path",
      name: "Label",
      field: "path",
      width: 260,
      formatter: (_r, _c, v) => text(v),
    },
    { id: "role", name: "Role", field: "role", width: 110, formatter: (_r, _c, v) => text(v) },
    {
      id: "messages",
      name: "Messages",
      field: "messages",
      width: 110,
      cssClass: "tg-right",
      formatter: (_r, _c, v) => text(v),
    },
  ],
};

/// One column set per item kind; a conversation is named by a title
/// over an opaque id.
const COLUMNS: Record<ProbeItemKind, Layout> = {
  mailbox: LABELS,
  keyword: LABELS,
  conversation: {
    placeholder: "Search these conversations…",
    columns: [
      {
        id: "title",
        name: "Conversation",
        field: "path",
        width: 280,
        formatter: (_r, _c, _v, _col, item) => ({ text: byTitle(item), toolTip: item.path ?? "" }),
      },
      // A Slack DM carries a `group` tag and a head-count, a Claude
      // chat a date; whichever a source leaves empty is pruned below.
      { id: "role", name: "", field: "role", width: 80, formatter: (_r, _c, v) => text(v) },
      {
        id: "members",
        name: "People",
        field: "members",
        width: 90,
        cssClass: "tg-right",
        formatter: (_r, _c, v) => text(v),
      },
      {
        id: "updated_at",
        name: "Updated",
        field: "updated_at",
        width: 118,
        formatter: (_r, _c, _v, _col, item) => ({ text: byDate(item) }),
      },
    ],
  },
  channel: {
    placeholder: "Search these channels…",
    columns: [
      {
        id: "path",
        name: "Channel",
        field: "path",
        width: 260,
        formatter: (_r, _c, _v, _col, item) => ({ text: item ? `#${item.path}` : "" }),
      },
      { id: "role", name: "Notes", field: "role", width: 180, formatter: (_r, _c, v) => text(v) },
      {
        id: "members",
        name: "Members",
        field: "members",
        width: 110,
        cssClass: "tg-right",
        formatter: (_r, _c, v) => text(v),
      },
    ],
  },
  // A calendar is named by the name the filter matches; its id rides in
  // `title`, shown on hover, and Google's `primary` / `read-only` in role.
  calendar: {
    placeholder: "Search these calendars…",
    columns: [
      {
        id: "path",
        name: "Calendar",
        field: "path",
        width: 280,
        formatter: (_r, _c, _v, _col, item) => ({
          text: item?.path ?? "",
          toolTip: item?.title ?? "",
        }),
      },
      { id: "role", name: "Notes", field: "role", width: 120, formatter: (_r, _c, v) => text(v) },
    ],
  },
};

/// Every list a probe returns is one kind throughout (`labels` mixes
/// mailboxes with keywords, and those share a layout), so the first
/// item's kind decides. A column for a field no row fills in — Gmail's
/// message counts, a Claude chat's head-count — is dropped rather than
/// shown blank.
const layout = computed(() => {
  const { placeholder, columns } = COLUMNS[props.items[0]?.kind ?? "mailbox"] ?? LABELS;
  const filled = (field: keyof ProbeItem) =>
    props.items.some((i) => i[field] !== null && i[field] !== undefined);
  return {
    placeholder,
    columns: columns
      .filter((c) => c.id === "title" || filled(String(c.field) as keyof ProbeItem))
      .map((c) => ({ ...c, sortable: true, cellAttrs: { "col-id": String(c.id) } })),
  };
});

/// The rows on screen: the items whose visible text has the query in
/// it. The field's value is never narrowed by this; only the list is.
const shown = computed(() => {
  const q = query.value.trim().toLowerCase();
  if (!q) return props.items;
  return props.items.filter((i) =>
    [i.path, i.title, i.role].some((v) => v && String(v).toLowerCase().includes(q)),
  );
});

/// Set while this component is writing the grid's selection from
/// `modelValue`, so the resulting event doesn't echo straight back.
let applying = false;

function applySelection() {
  if (!bundle) return;
  const chosen = new Set(props.modelValue);
  const rows = bundle.dataView
    .getItems()
    .filter((i) => chosen.has(i.path))
    .map((i) => bundle!.dataView.getRowById(i.path))
    .filter((r): r is number => r != null);
  applying = true;
  bundle.slickGrid.setSelectedRows(rows);
  applying = false;
}

function onSelectedRowsChanged(_e: SlickEventData, args: OnSelectedRowsChangedEventArgs) {
  if (!bundle || applying) return;
  // Rows the query has hidden keep their choice: only what is listed
  // right now is read off the grid.
  const listed = new Set(shown.value.map((i) => i.path));
  const picked = new Set(
    args.rows
      .map((r) => (bundle!.dataView.getItem(r) as ProbeItem | undefined)?.path)
      .filter((p): p is string => !!p),
  );
  // In the order they were chosen, the grid's row order notwithstanding:
  // what was already in the value stays where it was, a new tick goes
  // on the end. Values from outside this list ride along untouched.
  const kept = props.modelValue.filter((v) => !listed.has(v) || picked.has(v));
  const added = [...picked].filter((p) => !props.modelValue.includes(p));
  emit("update:modelValue", [...kept, ...added]);
}

function options(): GridOption {
  return {
    datasetIdPropertyName: "path",
    enableHtmlRendering: false,
    enableEmptyDataWarningMessage: false,
    darkMode: document.documentElement.dataset.theme === "dark",
    enableAutoResize: true,
    ...KEEP_COLUMN_WIDTHS,
    autoResize: {
      container: boxEl.value!.parentElement!,
      calculateAvailableSizeBy: "container",
      resizeDetection: "container",
      autoHeight: false,
      bottomPadding: 0,
      minHeight: 100,
    },
    rowHeight: 28,
    enableCellNavigation: true,
    enableSelection: true,
    multiSelect: true,
    enableCheckboxSelector: true,
    checkboxSelector: { hideInFilterHeaderRow: true, width: 36 },
    selectionOptions: { selectActiveRow: false },
    enableSorting: true,
    enableColumnReorder: false,
    enableHeaderMenu: false,
    enableGridMenu: false,
    enableColumnPicker: false,
    enableContextMenu: false,
  };
}

function createGrid() {
  if (bundle || !boxEl.value) return;
  const opts = options();
  const root = boxEl.value.getRootNode();
  if (root instanceof ShadowRoot) opts.shadowRoot = root;
  bundle = new SlickVanillaGridBundle<ProbeItem>(
    boxEl.value,
    layout.value.columns,
    opts,
    shown.value,
  ) as Grid;
  bundle.slickGrid.onSelectedRowsChanged.subscribe(onSelectedRowsChanged);
  stampRowKeys(bundle.slickGrid, bundle.dataView, (item) => (item as ProbeItem).path);
  applySelection();
}

onMounted(createGrid);
onBeforeUnmount(() => {
  bundle?.dispose();
  bundle = null;
});

watch(layout, (l) => {
  if (bundle) bundle.columnDefinitions = l.columns;
});
watch(shown, (rows) => {
  if (!bundle) return;
  bundle.dataset = rows;
  applySelection();
});
watch(() => props.modelValue, applySelection, { deep: true });
</script>

<template>
  <div class="pick">
    <input v-model="query" class="pick-filter" type="search" :placeholder="layout.placeholder" />
    <div class="pick-grid">
      <div ref="boxEl" class="pick-box" />
    </div>
  </div>
</template>

<style scoped>
.pick {
  display: flex;
  flex-direction: column;
  gap: 6px;
}
.pick-filter {
  width: 100%;
  padding: 6px 8px;
  border: 1px solid var(--datalib-border);
  border-radius: 5px;
  background: var(--datalib-input-bg);
  color: var(--datalib-fg);
  font: inherit;
  box-sizing: border-box;
}
/* Tall enough to show that the account really is being read, short
   enough that the rest of the form stays on screen. The grid scrolls
   inside it. */
.pick-grid {
  height: 260px;
  width: 100%;
  position: relative;
}
.pick-box {
  position: absolute;
  inset: 0;
}
</style>
