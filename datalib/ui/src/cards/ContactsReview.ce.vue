<script setup lang="ts">
// The contacts app's review: what agents proposed (links, new contacts,
// labels) beside what you already decided, read live from
// `GET /applet/datalib_contacts/review`. A row is a contact or a handle
// with labels of its own. `real` (not spam), and any label picked in the
// label filter, is a membership switch (out · not known · in) on every
// row: an outlined answer is an agent's, and a press is yours. Confirm
// agrees with everything the agent proposed on the row; Reject unlinks
// what it linked and flips its labels; the bar's buttons act on every
// selected row. Close calls come first. `contactsReview.ts` holds the
// rules.
import { computed, onBeforeUnmount, onMounted, ref, shallowRef, watch } from "vue";
import { filterToken } from "@/grid/query";
import type { MenuEntry } from "@/grid/menu";
import { subscribeLive } from "@/live";
import { pushToast } from "@/toasts";
import type { ColumnSpec } from "@/api";
import { personSource, searchSource } from "./cardSources";
import {
  CONTACTS_APPLET,
  canLinkHandles,
  confirmReview,
  contactsById,
  movesPeople,
  people,
  rejectReview,
  setLabel,
} from "./contacts";
import {
  REAL,
  clusterLabels,
  filterReview,
  membershipLabel,
  orderReview,
  reviewHandle,
  type ReviewRow,
} from "./contactsReview";
import TableGrid from "./TableGrid.ce.vue";
import type { TableGridApi } from "./tableGridApi";
import type { CardCtx } from "./types";

const props = defineProps<{ ctx: CardCtx }>();
const emit = defineEmits<{ waiting: [count: number] }>();

const columns = ref<ColumnSpec[]>([]);
const rows = ref<ReviewRow[]>([]);
const error = ref<string | null>(null);
const waiting = ref(true);
const real = ref<"" | "in" | "out" | "unknown">("");
const label = ref("");
const text = ref("");
const newLabel = ref("");
const busy = ref(false);
const closeFirst = ref(true);
const shown = computed(() => {
  const kept = filterReview(rows.value, {
    waiting: waiting.value,
    real: real.value,
    label: label.value,
    text: text.value,
  });
  return closeFirst.value ? orderReview(kept) : kept;
});
const waitingCount = computed(() => rows.value.filter((r) => r.awaiting).length);
const clusters = computed(() => clusterLabels(rows.value));
const cardEl = ref<HTMLElement | null>(null);
const grid = shallowRef<TableGridApi<ReviewRow> | null>(null);

let asked = 0;
async function load() {
  const mine = ++asked;
  try {
    const toggle = label.value ? `?label=${encodeURIComponent(label.value)}` : "";
    const r = await fetch(`${CONTACTS_APPLET}/review${toggle}`);
    const body = await r.text();
    if (!r.ok) throw new Error(`review → ${r.status}: ${body}`);
    const t = JSON.parse(body) as { columns: ColumnSpec[]; rows: ReviewRow[] };
    if (mine !== asked) return;
    if (JSON.stringify(t.columns) !== JSON.stringify(columns.value)) columns.value = t.columns;
    rows.value = t.rows;
    emit("waiting", rows.value.filter((r) => r.awaiting).length);
    error.value = null;
  } catch (e) {
    if (mine === asked) error.value = (e as Error).message;
  }
}

/** The rows a bar button acts on: the selection, else none. */
function selected(): ReviewRow[] {
  return grid.value?.selectedRows() ?? [];
}

async function act(what: string, run: () => Promise<number>) {
  busy.value = true;
  try {
    const n = await run();
    pushToast(`${what}: ${n} ${n === 1 ? "change" : "changes"}`, "info");
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    busy.value = false;
  }
  await load();
}

const keys = (targets: ReviewRow[]) => targets.map((r) => r.key);
const confirm = (targets: ReviewRow[]) => act("Confirmed", () => confirmReview(keys(targets)));
const reject = (targets: ReviewRow[]) => act("Turned down", () => rejectReview(keys(targets)));
const mark = (targets: ReviewRow[], l: string, applies: boolean | null) =>
  act(applies === false ? `Not ${l}` : applies === null ? `Forgot ${l}` : `Marked ${l}`, () =>
    setLabel(keys(targets), l, applies),
  );

/// A membership toggle pressed on one row: your answer for that label,
/// at once and without a toast, since a review is many of them.
async function onMembership(row: ReviewRow, field: string, applies: boolean | null) {
  const l = membershipLabel(field);
  if (!l) return;
  try {
    await setLabel([row.key], l, applies);
  } catch (e) {
    error.value = (e as Error).message;
  }
  await load();
}

watch(label, () => void load());

function addLabel() {
  const l = newLabel.value.trim();
  const targets = selected();
  if (!l || targets.length === 0) return;
  void mark(targets, l, true).then(() => (newLabel.value = ""));
}

function openPerson(row: ReviewRow) {
  const handle = reviewHandle(row);
  if (handle) props.ctx.host.openCards(personSource(handle, { seenIn: null }));
}

function menu(row: ReviewRow, targets: ReviewRow[]): MenuEntry[] {
  const n = targets.length;
  const these = n === 1 ? "" : ` (${n} rows)`;
  const handle = reviewHandle(row);
  const entries: MenuEntry[] = [
    { name: `Confirm${these}`, action: () => void confirm(targets) },
    { name: `Reject${these}`, action: () => void reject(targets) },
    { separator: true, name: "" },
    { name: `Real${these}`, action: () => void mark(targets, REAL, true) },
    { name: `Not real${these}`, action: () => void mark(targets, REAL, false) },
  ];
  const held = row.label_values.filter((l) => l !== REAL);
  if (held.length > 0) {
    entries.push({ separator: true, name: "" });
    for (const l of held) {
      entries.push({ name: `Not ${l}${these}`, action: () => void mark(targets, l, false) });
    }
  }
  entries.push(
    { separator: true, name: "" },
    {
      name: "Open person card",
      disabled: handle ? null : "This contact has no handle yet.",
      action: () => openPerson(row),
    },
    {
      name: `Everything with ${row.who.label}`,
      disabled: handle ? null : "This contact has no handle yet.",
      action: () =>
        handle && props.ctx.host.openCards(searchSource(filterToken("with", handle, false))),
    },
  );
  return entries;
}

const actions = {
  confirm: (row: ReviewRow) => void confirm([row]),
  reject: (row: ReviewRow) => void reject([row]),
};

const chips = {
  who: (handle: string) => people.lookup(handle),
  canLink: () => canLinkHandles(),
  contact: (id: string) => contactsById.lookup(id),
};

const stopPeople = people.subscribe(() => grid.value?.refreshCells(["who", "links"]));
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
    { onScreen: cardEl.value ?? undefined },
  );
});
onBeforeUnmount(() => {
  stopPeople();
  stopLive?.();
});
</script>

<template>
  <div ref="cardEl" class="review-card">
    <div class="contacts-bar">
      <input
        v-model="text"
        class="contacts-input review-text"
        type="search"
        placeholder="Filter by name, handle, label or reason"
        aria-label="Filter the review"
      />
      <label class="review-check">
        <input v-model="waiting" type="checkbox" />
        Waiting for you ({{ waitingCount }})
      </label>
      <label class="review-check" title="What waits for you, the agent's least sure first">
        <input v-model="closeFirst" type="checkbox" />
        Close calls first
      </label>
      <select v-model="real" class="contacts-input" aria-label="Real">
        <option value="">Real or not</option>
        <option value="in">Real</option>
        <option value="out">Not real</option>
        <option value="unknown">Not known</option>
      </select>
      <select v-model="label" class="contacts-input" aria-label="Label">
        <option value="">Any label</option>
        <option v-for="c in clusters" :key="c.label" :value="c.label">
          {{ c.label }} ({{ c.count }})
        </option>
      </select>
      <span class="contacts-count">{{ shown.length }} of {{ rows.length }}</span>
    </div>
    <div class="contacts-bar review-actions">
      <span class="contacts-count">Selected rows:</span>
      <button class="contacts-button" :disabled="busy" @click="confirm(selected())">Confirm</button>
      <button class="contacts-button" :disabled="busy" @click="reject(selected())">Reject</button>
      <button class="contacts-button" :disabled="busy" @click="mark(selected(), REAL, true)">
        Real
      </button>
      <button class="contacts-button" :disabled="busy" @click="mark(selected(), REAL, false)">
        Not real
      </button>
      <form class="contacts-new" @submit.prevent="addLabel">
        <input
          v-model="newLabel"
          class="contacts-input"
          list="review-labels"
          placeholder="Add a label"
          aria-label="Label to add to the selected rows"
        />
        <datalist id="review-labels">
          <option v-for="c in clusters" :key="c.label" :value="c.label" />
        </datalist>
        <button type="submit" class="contacts-button" :disabled="busy || !newLabel.trim()">
          Add
        </button>
      </form>
    </div>
    <div v-if="error" class="contacts-error">{{ error }}</div>
    <div class="contacts-grid">
      <TableGrid
        :columns="columns"
        :rows="shown"
        rowKey="key"
        :selectable="true"
        :chips="chips"
        :actions="actions"
        :membership="onMembership"
        :menu="(anchor, targets) => menu(anchor, targets)"
        @ready="(api) => (grid = api)"
        @cellDoubleClick="(row) => openPerson(row)"
        @openCard="(source) => props.ctx.host.openCards(source)"
      />
    </div>
  </div>
</template>

<style>
.review-card {
  display: flex;
  flex-direction: column;
  flex: 1 1 auto;
  min-height: 0;
}
.review-card > .contacts-bar > .review-text {
  flex: 1 1 14rem;
}
.review-card select.contacts-input {
  flex: 0 0 auto;
}
.review-check {
  display: flex;
  align-items: center;
  gap: 0.3rem;
  white-space: nowrap;
}
.review-actions {
  gap: 0.4rem;
}
</style>
