<script setup lang="ts">
// Unfiled, the contacts card's second tab: everyone who wrote something
// and is on no contact, most first (`GET …/people/unfiled`). Select rows
// and Add to contacts makes each a contact with every handle a source
// ties to them (`peopleToAdd`). A sender marked not real is left out
// unless asked for. docs/dev/contacts.md § "The contacts list".
import { computed, onBeforeUnmount, onMounted, ref, shallowRef } from "vue";
import type { ColumnSpec } from "@/api";
import { copyToClipboard } from "@/clipboard";
import { filterToken } from "@/grid/query";
import type { MenuEntry } from "@/grid/menu";
import { subscribeLive } from "@/live";
import { pushToast } from "@/toasts";
import { personSource, searchSource } from "./cardSources";
import {
  addedSummary,
  addPeople,
  contactsById,
  movesPeople,
  people,
  unfiledPeople,
} from "./contacts";
import {
  filterUnfiled,
  LIST_SORTS,
  peopleToAdd,
  sortList,
  unfiledSortKey,
  type ListSort,
  type UnfiledRow,
} from "./contactsTable";
import TableGrid from "./TableGrid.ce.vue";
import type { TableGridApi } from "./tableGridApi";
import type { CardCtx } from "./types";

const props = defineProps<{ ctx: CardCtx }>();
const emit = defineEmits<{ added: [] }>();

const columns = ref<ColumnSpec[]>([]);
const rows = ref<UnfiledRow[]>([]);
const loaded = ref(false);
const error = ref<string | null>(null);
const filter = ref("");
const sort = ref<ListSort>("most");
const notReal = ref(false);
const busy = ref(false);
const listEl = ref<HTMLElement | null>(null);
const grid = shallowRef<TableGridApi<UnfiledRow> | null>(null);
const shown = computed(() =>
  sortList(filterUnfiled(rows.value, filter.value), sort.value, unfiledSortKey),
);

let asked = 0;
async function load() {
  const mine = ++asked;
  try {
    const t = await unfiledPeople(notReal.value);
    if (mine !== asked) return;
    if (JSON.stringify(t.columns) !== JSON.stringify(columns.value)) columns.value = t.columns;
    rows.value = t.rows;
    error.value = null;
  } catch (e) {
    if (mine === asked) error.value = (e as Error).message;
  } finally {
    if (mine === asked) loaded.value = true;
  }
}

async function add(targets: UnfiledRow[]) {
  if (targets.length === 0) return;
  busy.value = true;
  error.value = null;
  try {
    await people.ask(targets.map((r) => r.handle));
    const added = await addPeople(peopleToAdd(targets, (h) => people.get(h)?.sourceContacts ?? []));
    pushToast(addedSummary(added), "info");
    emit("added");
    await load();
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    busy.value = false;
  }
}

function addSelected() {
  void add(grid.value?.selectedRows() ?? []);
}

function openPerson(row: UnfiledRow) {
  props.ctx.host.openCards(personSource(row.handle, { seenIn: null }));
}

function menu(row: UnfiledRow, targets: UnfiledRow[]): MenuEntry[] {
  const name = row.person.label;
  return [
    {
      name: targets.length === 1 ? "Add to contacts" : `Add ${targets.length} to contacts`,
      action: () => void add(targets),
    },
    { name: "Open", action: () => openPerson(row) },
    {
      name: `Everything from ${name}`,
      action: () => props.ctx.host.openCards(searchSource(filterToken("from", row.handle, false))),
    },
    { separator: true, name: "" },
    {
      name: `Copy “${name}”`,
      action: () =>
        void copyToClipboard(name).then((ok) =>
          ok ? pushToast(`Copied ${name}`, "info") : pushToast("The clipboard refused the copy"),
        ),
    },
  ];
}

const chips = {
  who: (handle: string) => people.lookup(handle),
  contact: (id: string) => contactsById.lookup(id),
};

const stopPeople = people.subscribe(() => grid.value?.refreshCells(["person"]));
let stopLive: (() => void) | null = null;
onMounted(() => {
  void load();
  stopLive = subscribeLive(
    {
      root: (e) => {
        if (movesPeople(e)) void load();
      },
      resync: () => void load(),
    },
    { onScreen: listEl.value ?? undefined },
  );
});
onBeforeUnmount(() => {
  stopPeople();
  stopLive?.();
});
</script>

<template>
  <div ref="listEl" class="contacts-list">
    <div class="contacts-bar">
      <input
        v-model="filter"
        class="contacts-input"
        type="search"
        placeholder="Filter by name or handle"
        aria-label="Filter unfiled people"
      />
      <select v-model="sort" class="contacts-input contacts-sort" aria-label="Sort unfiled people">
        <option v-for="s in LIST_SORTS" :key="s.sort" :value="s.sort">{{ s.label }}</option>
      </select>
      <span class="contacts-count">
        {{ shown.length === rows.length ? rows.length : `${shown.length} of ${rows.length}` }}
        {{ rows.length === 1 ? "person" : "people" }}
      </span>
      <label class="contacts-switch">
        <input v-model="notReal" type="checkbox" @change="load" />
        Show senders marked not real
      </label>
      <button type="button" class="contacts-button" :disabled="busy" @click="addSelected">
        Add selected to contacts
      </button>
    </div>
    <div v-if="error" class="contacts-error">{{ error }}</div>
    <p v-if="loaded && !error && rows.length === 0" class="contacts-empty">
      Everyone who wrote something here is in your contacts.
    </p>
    <div class="contacts-grid">
      <TableGrid
        :columns="columns"
        :rows="shown"
        rowKey="handle"
        :selectable="true"
        :chips="chips"
        :menu="(anchor, targets) => menu(anchor, targets)"
        @ready="(api) => (grid = api)"
        @cellDoubleClick="(row) => openPerson(row)"
        @openCard="(source) => props.ctx.host.openCards(source)"
      />
    </div>
  </div>
</template>
