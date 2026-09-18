<script setup lang="ts">
// `tableView({ url })`: any endpoint that answers with `columns` and
// `rows` — `/api/manage/rows`, say — drawn by the typed viewer, and
// refetched whenever the data root says something changed. No actions:
// a table of somebody else's rows can be read, not driven.
import { onBeforeUnmount, onMounted, ref } from "vue";
import TableGrid from "./TableGrid.ce.vue";
import { fetchTable, type ColumnSpec } from "@/api";
import { subscribeLive } from "@/live";
import type { CardCtx } from "./types";

const props = defineProps<{ url: string; title?: string; ctx: CardCtx }>();

const columns = ref<ColumnSpec[]>([]);
const rows = ref<Record<string, unknown>[]>([]);
const tree = ref(false);
const rowKey = ref("key");
const error = ref<string | null>(null);

props.ctx.setTitle(props.title ?? props.url.replace(/^\/api\//, ""));

async function load() {
  try {
    const t = await fetchTable(props.url);
    columns.value = t.columns ?? [];
    rows.value = t.rows ?? [];
    tree.value = !!t.tree;
    rowKey.value = t.row_key ?? "key";
    error.value = t.error ?? (t.errors?.length ? t.errors.join(" · ") : null);
  } catch (e) {
    error.value = (e as Error).message;
  }
}

let unsubscribe: (() => void) | null = null;
onMounted(() => {
  void load();
  unsubscribe = subscribeLive({ root: () => void load(), resync: () => void load() });
});
onBeforeUnmount(() => unsubscribe?.());
</script>

<template>
  <div class="table-card">
    <div v-if="error" class="table-card-error">{{ error }}</div>
    <div class="table-card-grid">
      <TableGrid
        :columns="columns"
        :rows="rows"
        :rowKey="rowKey"
        :tree="tree"
        :selectable="true"
      />
    </div>
  </div>
</template>

<style>
.table-card { position: absolute; inset: 0; display: flex; flex-direction: column; }
.table-card-error { padding: 8px 12px; color: var(--datalib-log-error); font-size: 13px; }
.table-card-grid { position: relative; flex: 1 1 auto; min-height: 0; }
</style>
