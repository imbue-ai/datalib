<script setup lang="ts">
// `contactsView()`: every contact in the contacts app, read straight from
// it (`GET /applet/datalib_contacts/contacts`), so an edit made anywhere
// shows at once. The person is a chip, as in any grid: a click links it,
// a double-click on a row opens the person card, a right-click offers
// the card, everything with them, copying the name, merging the selected
// rows into the one clicked, and deleting. A new contact is made from the
// box at the top.
import { computed, onBeforeUnmount, onMounted, ref, shallowRef } from "vue";
import { copyToClipboard } from "@/clipboard";
import { confirmAction } from "@/desktop";
import { filterToken } from "@/grid/query";
import type { MenuEntry } from "@/grid/menu";
import { subscribeLive } from "@/live";
import { CONTACT } from "@/search/queryText";
import { pushToast } from "@/toasts";
import type { ColumnSpec } from "@/api";
import { personSource, searchSource } from "./cardSources";
import { isBrowserClick } from "./chatLink";
import {
  CONTACTS_APPLET,
  canLinkHandles,
  contactsById,
  createContact,
  deleteContact,
  mergeContacts,
  movesPeople,
  people,
  type NormalizedContact,
} from "./contacts";
import { filterContacts, mergeInto, rowHandle, type ContactRow } from "./contactsTable";
import HandlePopover from "./HandlePopover.ce.vue";
import TableGrid from "./TableGrid.ce.vue";
import type { TableGridApi } from "./tableGridApi";
import type { CardCtx } from "./types";

const props = defineProps<{ ctx: CardCtx }>();
props.ctx.setTitle("Your contacts");
props.ctx.setHelp(`
<p>Every contact you have made in the contacts app, read from it as it
is now: a link or a new contact shows here at once. A contact is drawn
by its first working address or number, pale blue because it is yours.
A contact with no handle yet is drawn as itself. Double-click a row to
open the person card: your contact and what every source says about
them. Right-click for everything with them, or to delete the contact.
To merge duplicates, select them all and right-click the one to keep.</p>
`);

const columns = ref<ColumnSpec[]>([]);
const rows = ref<ContactRow[]>([]);
const error = ref<string | null>(null);
const filter = ref("");
const newName = ref("");
const creating = ref(false);
const shown = computed(() => filterContacts(rows.value, filter.value));
const cardEl = ref<HTMLElement | null>(null);
const grid = shallowRef<TableGridApi<ContactRow> | null>(null);

let asked = 0;
async function load() {
  const mine = ++asked;
  try {
    const r = await fetch(`${CONTACTS_APPLET}/contacts`);
    const text = await r.text();
    if (!r.ok) throw new Error(`contacts → ${r.status}: ${text}`);
    const t = JSON.parse(text) as { columns: ColumnSpec[]; rows: ContactRow[] };
    if (mine !== asked) return;
    if (JSON.stringify(t.columns) !== JSON.stringify(columns.value)) columns.value = t.columns;
    rows.value = t.rows;
    error.value = null;
  } catch (e) {
    if (mine === asked) error.value = (e as Error).message;
  }
}

async function create() {
  const name = newName.value.trim();
  if (!name) return;
  creating.value = true;
  try {
    await createContact(name, []);
    newName.value = "";
    await load();
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    creating.value = false;
  }
}

function openPerson(row: ContactRow) {
  const handle = rowHandle(row);
  if (handle) props.ctx.host.openCards(personSource(handle, { seenIn: null }));
  else pushToast(`${row.contact_ref.label} has no handle yet; link one to open their card`, "info");
}

function everythingWith(row: ContactRow) {
  props.ctx.host.openCards(searchSource(filterToken("with", `${CONTACT}${row.key}`, false)));
}

async function merge(into: ContactRow, from: ContactRow[]) {
  try {
    for (const r of from) await mergeContacts(r.key, into.key);
    pushToast(
      `Merged ${from.length === 1 ? "1 contact" : `${from.length} contacts`} into ${into.contact_ref.label}`,
      "info",
    );
  } catch (e) {
    error.value = (e as Error).message;
  }
  await load();
}

async function remove(rows: ContactRow[]) {
  const what = rows.length === 1 ? `“${rows[0].contact_ref.label}”` : `${rows.length} contacts`;
  if (!(await confirmAction(`Delete ${what}? Their handles will be linked to nobody.`))) return;
  try {
    for (const r of rows) await deleteContact(r.key);
  } catch (e) {
    error.value = (e as Error).message;
  }
  await load();
}

function menu(row: ContactRow, targets: ContactRow[]): MenuEntry[] {
  const name = row.contact_ref.label;
  const others = mergeInto(row, targets);
  return [
    {
      name: "Open person card",
      disabled: rowHandle(row) ? null : "This contact has no handle yet.",
      action: () => openPerson(row),
    },
    { name: `Everything with ${name}`, action: () => everythingWith(row) },
    { separator: true, name: "" },
    {
      name: `Copy “${name}”`,
      action: () =>
        void copyToClipboard(name).then((ok) =>
          ok ? pushToast(`Copied ${name}`, "info") : pushToast("The clipboard refused the copy"),
        ),
    },
    { separator: true, name: "" },
    {
      name:
        others.length > 0
          ? `Merge ${others.length === 1 ? "the other selected contact" : `${others.length} selected contacts`} into this one`
          : "Merge into this one…",
      disabled:
        others.length > 0
          ? null
          : "Select the duplicates too (⌘- or Shift-click), then right-click the one to keep.",
      action: () => void merge(row, others),
    },
    {
      name: targets.length === 1 ? "Delete contact…" : `Delete ${targets.length} contacts…`,
      danger: true,
      action: () => void remove(targets),
    },
  ];
}

// ── A chip's click: link it, never follow its mailto: ───────────────

type Popover = {
  handle: string;
  shownAs: string;
  resolved: NormalizedContact | null;
  x: number;
  y: number;
};
const popover = ref<Popover | null>(null);

function onChipClick(ev: MouseEvent) {
  const t = ev.target;
  const chip = t instanceof Element ? t.closest<HTMLElement>("a.chip[data-handle]") : null;
  if (!chip || isBrowserClick(ev)) return;
  ev.preventDefault();
  if (ev.detail > 1 || !canLinkHandles()) return;
  const handle = chip.dataset.handle ?? "";
  popover.value = {
    handle,
    shownAs: chip.dataset.shownAs ?? "",
    resolved: people.get(handle)?.mine ?? null,
    x: ev.clientX,
    y: ev.clientY,
  };
}

const chips = {
  who: (handle: string) => people.lookup(handle),
  canLink: () => canLinkHandles(),
  contact: (id: string) => contactsById.lookup(id),
};

const stopPeople = people.subscribe(() => grid.value?.refreshCells(["contact_ref"]));
const stopContacts = contactsById.subscribe(() => grid.value?.refreshCells(["contact_ref"]));
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
  stopContacts();
  stopLive?.();
});
</script>

<template>
  <div ref="cardEl" class="contacts-card" @click.capture="onChipClick">
    <div class="contacts-bar">
      <input
        v-model="filter"
        class="contacts-input"
        type="search"
        placeholder="Filter by name, address, number or note"
        aria-label="Filter contacts"
      />
      <span class="contacts-count">
        {{ shown.length === rows.length ? rows.length : `${shown.length} of ${rows.length}` }}
        {{ rows.length === 1 ? "contact" : "contacts" }}
      </span>
      <form class="contacts-new" @submit.prevent="create">
        <input
          v-model="newName"
          class="contacts-input"
          placeholder="New contact's name"
          aria-label="New contact's name"
        />
        <button type="submit" class="contacts-button" :disabled="creating || !newName.trim()">
          Create
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
        :menu="(anchor, targets) => menu(anchor, targets)"
        @ready="(api) => (grid = api)"
        @cellDoubleClick="(row) => openPerson(row)"
      />
    </div>
    <HandlePopover
      v-if="popover"
      :key="popover.handle"
      v-bind="popover"
      @close="popover = null"
      @changed="load"
    />
  </div>
</template>

<style>
.contacts-card {
  position: absolute;
  inset: 0;
  display: flex;
  flex-direction: column;
  color: var(--datalib-fg);
  background: var(--datalib-bg);
  font-size: var(--datalib-font-size);
}
.contacts-bar {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.5rem 0.75rem;
  padding: 0.5rem 0.75rem;
  border-bottom: 1px solid var(--datalib-border-soft);
}
.contacts-bar > .contacts-input {
  flex: 1 1 14rem;
}
.contacts-new {
  display: flex;
  gap: 0.4rem;
}
.contacts-input {
  font: inherit;
  padding: 0.2rem 0.45rem;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-input-bg);
  color: var(--datalib-fg);
}
.contacts-button {
  font: inherit;
  padding: 0.2rem 0.6rem;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-surface);
  color: var(--datalib-fg);
  cursor: pointer;
}
.contacts-button:disabled {
  cursor: default;
  opacity: 0.6;
}
.contacts-count {
  color: var(--datalib-muted);
}
.contacts-error {
  padding: 0.4rem 0.75rem;
  color: var(--datalib-error-fg);
}
.contacts-grid {
  position: relative;
  flex: 1 1 auto;
  min-height: 0;
}
</style>
