<script setup lang="ts">
// `contactsView()`: every contact in the contacts app, read straight from
// it (`GET /applet/datalib_contacts/contacts`), so an edit made anywhere
// shows at once, with how much each wrote from the index
// (`GET …/people/contacts`), and sorted by recency, by that count or by
// name. The person is a chip, as in any grid: a double-click on a row
// opens the person card, a right-click offers the card, everything with
// them, copying the name, merging the selected rows into the one
// clicked, and deleting. A new contact is made from the box at the top.
// Unfiled lists everyone else who wrote (`UnfiledList.ce.vue`); Review,
// what agents proposed (`ContactsReview.ce.vue`).
import { computed, onBeforeUnmount, onMounted, ref, shallowRef } from "vue";
import { copyToClipboard } from "@/clipboard";
import { confirmAction } from "@/desktop";
import { filterToken } from "@/grid/query";
import type { MenuEntry } from "@/grid/menu";
import { subscribeLive } from "@/live";
import { CONTACT } from "@/search/queryText";
import { pushToast } from "@/toasts";
import type { ColumnSpec } from "@/api";
import { contactSubject, personSource, searchSource } from "./cardSources";
import {
  CONTACTS_APPLET,
  contactCounts,
  contactsById,
  createContact,
  deleteContact,
  isAbsent,
  mergeContacts,
  movesPeople,
  people,
  type NormalizedContact,
} from "./contacts";
import {
  contactSortKey,
  filterContacts,
  LIST_SORTS,
  mergeInto,
  sortList,
  withCountColumns,
  withCounts,
  type ContactRow,
  type ListSort,
  type Wrote,
} from "./contactsTable";
import type { ReviewRow } from "./contactsReview";
import ContactsReview from "./ContactsReview.ce.vue";
import UnfiledList from "./UnfiledList.ce.vue";
import TableGrid from "./TableGrid.ce.vue";
import type { TableGridApi } from "./tableGridApi";
import type { CardCtx } from "./types";

const props = defineProps<{ ctx: CardCtx }>();
props.ctx.setTitle("Contacts");
props.ctx.setHelp(`
<p>The people you have added, and nobody else: no source fills this list.
To add someone, double-click them anywhere and choose <em>Add to
contacts</em>, pick them from <b>Unfiled</b>, or type a new name above.
Double-click a row for the person's page: your contact, and what every
source says about them. Right-click for everything with them, or to
delete the contact. To merge duplicates, select them all and right-click
the one to keep.</p>
<p><em>Items</em> is how much someone wrote, which is what
<code>from:</code> finds, and <em>Last</em> when they last did; sort by
either, or by name.</p>
<p><b>Unfiled</b> lists everyone else who wrote something here, the most
first. Select some and <em>Add selected to contacts</em>: each comes with
every handle a source says is theirs. Senders you or an agent marked not
real are left out unless you ask for them.</p>
<p><b>Review</b> lists what agents proposed: contacts they made, handles
they put on one, and whether each person or sender is <i>real</i>, which is
to say not spam. A label is in, out, or not decided. Confirm agrees with the agent; Reject takes off the
handles it put on and flips its labels, and both are written down so no agent
proposes the same again. Select many rows to act on them at once.</p>
`);

const mode = ref<"contacts" | "unfiled" | "review">("contacts");
const waitingReview = ref<number | null>(null);

const columns = ref<ColumnSpec[]>([]);
const rows = ref<ContactRow[]>([]);
const error = ref<string | null>(null);
/// This library's config has no contacts app.
const absent = ref(false);
const filter = ref("");
const sort = ref<ListSort>("recent");
/// How much each contact wrote; null until the index answers, or where
/// it cannot.
const counts = ref<Record<string, Wrote> | null>(null);
const newName = ref("");
const creating = ref(false);
const loaded = ref(false);
const shown = computed(() => {
  const counted = counts.value ? withCounts(rows.value, counts.value) : rows.value;
  return sortList(filterContacts(counted, filter.value), sort.value, contactSortKey);
});
const shownColumns = computed(() =>
  counts.value ? withCountColumns(columns.value) : columns.value,
);
const cardEl = ref<HTMLElement | null>(null);
const grid = shallowRef<TableGridApi<ContactRow> | null>(null);

let asked = 0;
async function load() {
  const mine = ++asked;
  try {
    // Together, so the grid's first columns hold the counts: a column
    // that comes later goes after any the grid has laid out.
    const [r, wrote] = await Promise.all([
      fetch(`${CONTACTS_APPLET}/contacts`),
      // A library with no index yet shows the list without them.
      contactCounts().catch(() => null),
    ]);
    const text = await r.text();
    if (mine === asked) absent.value = isAbsent(r.status, text);
    if (absent.value) return;
    if (!r.ok) throw new Error(`contacts → ${r.status}: ${text}`);
    const t = JSON.parse(text) as { columns: ColumnSpec[]; rows: ContactRow[] };
    if (mine !== asked) return;
    counts.value = wrote;
    if (JSON.stringify(t.columns) !== JSON.stringify(columns.value)) columns.value = t.columns;
    rows.value = t.rows;
    error.value = null;
  } catch (e) {
    if (mine === asked) error.value = (e as Error).message;
  } finally {
    if (mine === asked) loaded.value = true;
  }
  void countReview();
}

/** How many review rows wait for you, for the switch's label. */
async function countReview() {
  try {
    const r = await fetch(`${CONTACTS_APPLET}/review`);
    if (!r.ok) return;
    const t = (await r.json()) as { rows: ReviewRow[] };
    waitingReview.value = t.rows.filter((row) => row.awaiting).length;
  } catch {
    // An applet from before the review answers 404; the switch says no count.
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
  props.ctx.host.openCards(personSource(contactSubject(row.key), { seenIn: null }));
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
  if (!(await confirmAction(`Delete ${what}? Their handles will be nobody’s.`))) return;
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
      name: "Open",
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

const chips = {
  who: (handle: string) => people.lookup(handle),
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
  <div ref="cardEl" class="contacts-card">
    <p v-if="absent" class="contacts-absent">
      This library has no contacts app. Add it from <b>Sources</b>, under <b>Add source</b>.
    </p>
    <div v-if="!absent" class="contacts-modes" role="tablist">
      <button
        role="tab"
        class="contacts-mode"
        :aria-selected="mode === 'contacts'"
        @click="mode = 'contacts'"
      >
        Contacts
      </button>
      <button
        role="tab"
        class="contacts-mode"
        :aria-selected="mode === 'unfiled'"
        @click="mode = 'unfiled'"
      >
        Unfiled
      </button>
      <button
        role="tab"
        class="contacts-mode"
        :aria-selected="mode === 'review'"
        @click="mode = 'review'"
      >
        Review<span v-if="waitingReview" class="contacts-waiting">{{ waitingReview }} waiting</span>
      </button>
    </div>
    <UnfiledList v-if="!absent && mode === 'unfiled'" :ctx="props.ctx" @added="load" />
    <ContactsReview
      v-else-if="!absent && mode === 'review'"
      :ctx="props.ctx"
      @waiting="(n) => (waitingReview = n)"
    />
    <template v-else-if="!absent">
      <div class="contacts-bar">
        <input
          v-model="filter"
          class="contacts-input"
          type="search"
          placeholder="Filter by name, address, number or note"
          aria-label="Filter contacts"
        />
        <select v-model="sort" class="contacts-input contacts-sort" aria-label="Sort contacts">
          <option v-for="s in LIST_SORTS" :key="s.sort" :value="s.sort">{{ s.label }}</option>
        </select>
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
            Add
          </button>
        </form>
      </div>
      <div v-if="error" class="contacts-error">{{ error }}</div>
      <p v-if="loaded && !error && rows.length === 0" class="contacts-empty">
        No contacts yet. Double-click anyone in a message and choose <em>Add to contacts</em>, or
        pick people from <b>Unfiled</b>.
      </p>
      <div class="contacts-grid">
        <TableGrid
          :columns="shownColumns"
          :rows="shown"
          rowKey="key"
          :selectable="true"
          :chips="chips"
          :menu="(anchor, targets) => menu(anchor, targets)"
          @ready="(api) => (grid = api)"
          @cellDoubleClick="(row) => openPerson(row)"
          @openCard="(source) => props.ctx.host.openCards(source)"
        />
      </div>
    </template>
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
.contacts-absent {
  padding: 0.75rem;
  color: var(--datalib-muted);
}
.contacts-modes {
  display: flex;
  gap: 0.25rem;
  padding: 0.35rem 0.75rem 0;
  border-bottom: 1px solid var(--datalib-border-soft);
}
.contacts-mode {
  font: inherit;
  padding: 0.2rem 0.7rem;
  border: 1px solid transparent;
  border-bottom: none;
  border-radius: var(--datalib-radius) var(--datalib-radius) 0 0;
  background: none;
  color: var(--datalib-muted);
  cursor: pointer;
}
.contacts-mode[aria-selected="true"] {
  border-color: var(--datalib-border-soft);
  background: var(--datalib-surface);
  color: var(--datalib-fg);
}
.contacts-waiting {
  margin-left: 0.4rem;
  padding: 0 0.4rem;
  border-radius: 999px;
  background: var(--datalib-accent);
  color: var(--datalib-accent-fg, #fff);
  font-size: 0.85em;
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
.contacts-list {
  display: flex;
  flex-direction: column;
  flex: 1 1 auto;
  min-height: 0;
}
.contacts-bar > .contacts-sort {
  flex: 0 0 auto;
}
.contacts-switch {
  display: inline-flex;
  align-items: center;
  gap: 0.3rem;
  color: var(--datalib-muted);
}
.contacts-empty {
  margin: 0;
  padding: 0.6rem 0.75rem;
  color: var(--datalib-muted);
}
</style>
