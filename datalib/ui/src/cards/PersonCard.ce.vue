<script setup lang="ts">
// A person: your contact, when the handle is on one (edited in
// `ContactEditor`), else the way to add them; and what each source says
// about them, each source in a section of its own. Opened by
// double-clicking a person chip. What it shows is `person.ts`; who a
// handle is comes from `people`, the resolver every chip shares. Every
// handle on the card is a chip, pale blue where it is on a contact and
// grey where not, with the double-click and right-click of a chip in a
// document (docs/dev/chips.md § Clicks); its menu also puts the handle on
// your contact or takes it off.
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from "vue";
import { formatStamp } from "@/config/timeFormat";
import { personSource, searchSource } from "./cardSources";
import { isBrowserClick } from "./chatLink";
import { uriFromEntity } from "./chipLinks";
import ChipMenu from "./ChipMenu.ce.vue";
import ContactEditor from "./ContactEditor.ce.vue";
import { holds, sourceValues, valueText, type SourceValue } from "./contactFields";
import EntityChip from "./EntityChip.ce.vue";
import {
  chipCell,
  createContact,
  handleValue,
  hasContactsApp,
  identifierCell,
  linkHandle,
  nameOf,
  NOBODY,
  people,
  searchContacts,
  setMe,
  setStoppedWorking,
  suggestedName,
  todayPartialDate,
  unlinkHandle,
  type ContactSummary,
  type NormalizedContact,
} from "./contacts";
import {
  chipElAt,
  chipMenuOf,
  chipOf,
  everythingWith,
  justOpened,
  runChipAction,
  type Chip,
  type ChipEntryId,
  type ChipHost,
  type MenuItem,
} from "./chipActions";
import { entities } from "./entities";
import {
  handleMenu,
  isHandleAction,
  linkableHandles,
  linkState,
  personHandles,
  personModel,
  sectionHandles,
  sectionLines,
  type PersonSection,
} from "./person";
import type { CardCtx } from "./types";

const props = defineProps<{ ctx: CardCtx; handle: string; seenIn: string | null }>();

// Bumped whenever an answer this card reads changes, so the computed
// model reads `people` and `entities` again.
const version = ref(0);
const error = ref<string | null>(null);

const model = computed(() => {
  void version.value;
  return personModel(props.handle, props.seenIn, (h) => people.get(h));
});

async function ask() {
  error.value = null;
  try {
    await people.ask([props.handle]);
    await people.ask(personHandles(props.handle, people.get(props.handle)));
    await people.ask(sectionHandles(model.value));
    await entities.ask(model.value.sections.map((s) => sourceUri(s.sourceId)));
  } catch (e) {
    error.value = (e as Error).message;
  }
  version.value++;
  await takeKeyboard();
}

const stopPeople = people.subscribe((changed) => {
  const shown = [props.handle, ...model.value.handles, ...sectionHandles(model.value)];
  if (shown.some((h) => changed.has(h))) void ask();
});
const stopEntities = entities.subscribe(() => version.value++);
onUnmounted(() => {
  stopPeople();
  stopEntities();
});
onMounted(ask);
watch(() => [props.handle, props.seenIn], ask);

watch(
  () => model.value.title,
  (title) => props.ctx.setTitle(title),
  { immediate: true },
);
props.ctx.setHelp(`
<p>Everything this library knows about one person. <em>Your contact</em>,
when they are in your contacts, is what you wrote; every other section is
one source's record of them, as that source has it, shown because it
holds one of your contact's handles (its "via" line says which). Sources
are never merged: one phone number can carry a WhatsApp account, a name
in a text-message backup and a Signal profile, and they need not agree.
The source of the chip you opened this from comes first.</p>
<p>When they are not in your contacts, <em>Add to contacts</em> adds them.
Nothing else does: opening this page adds nobody.</p>
<p>Under <em>Your contact</em>, <em>Edit</em> changes its name, note and
details. What you type is kept as a draft that only you see, even after a
reload, until <em>Save</em> publishes it or <em>Discard</em> drops it. If
the contact was changed somewhere else meanwhile, Save says so and marks
what changed rather than overwriting it. A photo is set or removed at
once.</p>
<p><em>This is me</em> says your contact is you: <code>from:me</code>,
<code>to:me</code> and <code>with:me</code> then find every handle
on it, an old address or a second number included. More than one
contact can be you, such as a duplicate not merged yet.</p>
<p>Under each source, <em>Use what it says</em> lists what that source
says (a number, a title, an address), and <em>Use</em> puts one in your
contact's draft; you pick where sources disagree. A value you use does
not follow its source afterwards: the card says when the source now says
something else.</p>
<p>Every handle is a chip: pale blue when it is on a contact, grey when
not. Right-click one to add it to your contact, to say it is <em>not
theirs</em>, or that it no longer works. <em>Not theirs</em> is
remembered, so no agent suggests it again.</p>
`);

function sourceUri(sourceId: string): string {
  return uriFromEntity("group", sourceId);
}

function sourceLabel(sourceId: string): string {
  void version.value;
  return entities.get(sourceUri(sourceId))?.label || sourceId;
}

function everything() {
  props.ctx.host.openCards(searchSource(everythingWith(props.handle, model.value.mine)));
}

// ── Chips ─────────────────────────────────────────────────────────────

/** What a handle line shows: the identifier as the source wrote it, or,
 *  for a kind with no address of its own (a Slack id), the handle. */
function shownValue(h: NormalizedContact["handles"][number]): string {
  return h.handle && h.medium === "other" ? handleValue(h.handle) : h.value;
}

/// Draw a chip into its slot. Called from a function ref, so every
/// render (each `version` bump) draws it again from the latest answer.
function drawInto(
  el: unknown,
  handle: string,
  shown: string,
  as: "person" | "identifier",
  _version: number,
) {
  if (!(el instanceof HTMLElement)) return;
  const who = people.get(handle) ?? NOBODY;
  el.replaceChildren(
    as === "person" ? chipCell(handle, shown, who) : identifierCell(handle, shown, who),
  );
}

function chipAt(ev: Event): HTMLElement | null {
  const t = ev.target;
  return t instanceof Element ? t.closest<HTMLElement>("a.chip[data-handle]") : null;
}

const menu = ref<{ chip: Chip; entries: MenuItem[]; x: number; y: number } | null>(null);

/// A click on a person does nothing, and above all does not open the mail
/// client its href names; a click with a modifier is the browser's, as on
/// any link.
function onClick(ev: MouseEvent) {
  if (chipAt(ev) && !isBrowserClick(ev)) ev.preventDefault();
}

/// What a chip's entries do on this card: open a card beside it, but not
/// this person's own again.
const chipHost: ChipHost = {
  openCard: (source) => {
    if (source !== personSource(props.handle, { seenIn: null })) props.ctx.host.openCards(source);
  },
};

function onDblClick(ev: MouseEvent) {
  const el = chipElAt(ev.target);
  const chip = el ? chipOf(el) : null;
  if (!el || !chip) return;
  ev.preventDefault();
  void runChipAction("open", chip, chipHost);
}

function onContextMenu(ev: MouseEvent) {
  const el = chipElAt(ev.target);
  const chip = el ? chipOf(el) : null;
  if (!el || !chip) return;
  ev.preventDefault();
  const handle = chip.ref.kind === "person" ? chip.ref.handle : null;
  const entries = [...chipMenuOf(chip), ...(handle ? handleMenu(handle, model.value, whoOf) : [])];
  menu.value = { chip, entries, x: ev.clientX, y: ev.clientY };
}

async function onMenuPick(id: string) {
  const m = menu.value;
  menu.value = null;
  if (!m) return;
  const handle = m.chip.ref.kind === "person" ? m.chip.ref.handle : null;
  const mine = model.value.mine;
  if (!handle || !mine || !isHandleAction(id)) {
    await runChipAction(id as ChipEntryId, m.chip, chipHost);
    return;
  }
  await act(() => {
    switch (id) {
      case "handle-add":
        return linkHandle(handle, mine.key);
      case "handle-remove":
        return unlinkHandle(handle);
      case "handle-stopped":
        return setStoppedWorking(handle, todayPartialDate());
      case "handle-works":
        return setStoppedWorking(handle, null);
    }
  });
}

function stoppedBy(handle: string, ev: Event) {
  const by = (ev.target as HTMLInputElement).value.trim();
  void act(() => setStoppedWorking(handle, by || null));
}

const busy = ref(false);
async function act(f: () => Promise<void>) {
  busy.value = true;
  error.value = null;
  try {
    await f();
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    busy.value = false;
  }
}

// ── A source's handles, against your contact ──────────────────────────

function whoOf(h: string) {
  void version.value;
  return people.get(h);
}

function linkable(s: PersonSection): string[] {
  return linkableHandles(s, model.value.mine, whoOf);
}

function stateOf(h: string | null) {
  return h ? linkState(h, model.value.mine, whoOf) : null;
}

function ownerName(h: string | null): string {
  const owner = h ? whoOf(h)?.mine : null;
  return owner ? nameOf(owner) : "";
}

async function addAll(s: PersonSection) {
  const mine = model.value.mine;
  if (!mine) return;
  await act(async () => {
    for (const h of linkable(s)) await linkHandle(h, mine.key);
  });
}

async function toggleMe(ev: Event) {
  const mine = model.value.mine;
  const me = (ev.target as HTMLInputElement).checked;
  if (mine) await act(() => setMe(mine.key, me));
}

function addLabel(s: PersonSection): string {
  const n = linkable(s).length;
  return `Add ${n === 1 ? "its other handle" : `its ${n} other handles`} to ${model.value.title}`;
}

// ── Adding them, when they are not in your contacts ───────────────────

const canAdd = computed(() => {
  void version.value;
  return !model.value.mine && hasContactsApp();
});
const newName = ref("");
watch(
  () => model.value.sections[0]?.contact,
  (first) => {
    if (!newName.value && first) newName.value = suggestedName(nameOf(first), props.handle);
  },
  { immediate: true },
);

/// Your contacts the name finds, the one of exactly that name first: the
/// handle may belong on one of them.
const found = ref<ContactSummary[]>([]);
let asked = 0;
async function findContacts() {
  const mine = ++asked;
  const name = newName.value.trim();
  let hits: ContactSummary[] = [];
  try {
    if (canAdd.value && name) hits = await searchContacts(name);
  } catch (e) {
    error.value = (e as Error).message;
  }
  if (mine !== asked) return;
  found.value = [...hits.filter(named), ...hits.filter((c) => !named(c))].slice(0, 5);
}
const named = (c: ContactSummary) => c.name.toLowerCase() === newName.value.trim().toLowerCase();
watch([canAdd, newName], findContacts);

const addTo = (c: ContactSummary) => act(() => linkHandle(props.handle, c.contact_id));
const add = () =>
  act(async () => {
    await createContact(newName.value.trim(), [props.handle]);
  });

const addForm = ref<HTMLElement | null>(null);
/// Opened from a chip, the page takes the keyboard, so that Enter adds.
async function takeKeyboard() {
  if (justOpened.value?.handle !== props.handle) return;
  await findContacts();
  await nextTick();
  if (justOpened.value?.handle !== props.handle) return;
  justOpened.value = null;
  const first = found.value[0];
  const button = first && named(first) ? ".person-found button" : "button[type=submit]";
  addForm.value?.querySelector<HTMLButtonElement>(button)?.focus();
}
watch(justOpened, () => {
  if (people.get(props.handle)) void takeKeyboard();
});

// ── Copying what a source says into your contact ─────────────────────

const editor = ref<InstanceType<typeof ContactEditor> | null>(null);

function held(v: SourceValue): boolean {
  const current = editor.value?.current;
  return !!current && holds(current, v);
}

async function copyFrom(s: PersonSection, values: SourceValue[], all = false) {
  await editor.value?.copy(values, { source_id: s.sourceId, key: s.contact.key }, all);
}

function photoOf(c: NormalizedContact): string | null {
  return c.photo_url ?? null;
}

const headPhoto = computed(() => {
  void version.value;
  const m = model.value;
  return (
    (m.mine && photoOf(m.mine)) || m.sections.map((s) => photoOf(s.contact)).find(Boolean) || null
  );
});
</script>
<template>
  <div class="person" @click="onClick" @dblclick="onDblClick" @contextmenu="onContextMenu">
    <header class="person-head">
      <img v-if="headPhoto" class="person-head-photo" :src="headPhoto" alt="" />
      <div class="person-head-text">
        <h1
          class="person-name"
          :ref="(el) => drawInto(el, props.handle, model.title, 'person', version)"
        ></h1>
        <p v-if="model.mine || canAdd" class="person-about">
          <template v-if="model.mine">
            In your contacts · known to {{ model.sections.length }}
            {{ model.sections.length === 1 ? "source" : "sources" }}
          </template>
          <template v-else>Not in your contacts</template>
        </p>
        <div class="person-actions">
          <button type="button" class="person-button" @click.stop="everything">
            Everything with them
          </button>
        </div>
        <div v-if="canAdd" ref="addForm" class="person-add">
          <ul v-if="found.length" class="person-found">
            <li v-for="c in found" :key="c.contact_id">
              <button type="button" class="person-button" :disabled="busy" @click="addTo(c)">
                Add to <b>{{ c.name }}</b>
              </button>
            </li>
          </ul>
          <form class="person-create" @submit.prevent="add">
            <input
              v-model="newName"
              class="person-input"
              aria-label="Name for a new contact"
              placeholder="Name"
            />
            <button type="submit" class="person-button" :disabled="busy || !newName.trim()">
              Add to contacts
            </button>
          </form>
        </div>
        <p v-if="error" class="person-error">{{ error }}</p>
      </div>
    </header>

    <section v-if="model.mine" class="person-section person-mine" aria-label="Your contact">
      <h2 class="person-source">Your contact</h2>
      <label
        v-if="model.mine.kind === 'person'"
        class="person-me"
        title="from:me, to:me and with:me find every handle on this contact"
      >
        <input
          type="checkbox"
          data-testid="contact-is-me"
          :checked="!!model.mine.account"
          :disabled="busy"
          @change="toggleMe"
        />
        This is me
      </label>
      <ul class="person-handles">
        <li v-for="h in model.mine.handles" :key="h.value">
          <span
            v-if="h.handle"
            class="person-chip"
            :ref="(el) => drawInto(el, h.handle!, shownValue(h), 'identifier', version)"
          ></span>
          <span v-else>{{ h.value }}</span>
          <label v-if="h.handle && h.stopped_working_by" class="person-muted">
            stopped working by
            <input
              class="person-input person-date"
              :value="h.stopped_working_by"
              :disabled="busy"
              title="2019, 2019-06 or 2019-06-14; empty when it works again"
              @change="stoppedBy(h.handle, $event)"
            />
          </label>
        </li>
      </ul>
      <ContactEditor
        ref="editor"
        :contact-id="model.mine.key"
        :photo-url="model.mine.photo_url ?? null"
        :linked="model.mine.handles"
        :sources="model.sections.map((s) => s.contact)"
        :source-name="sourceLabel"
      />
    </section>

    <section
      v-for="s in model.sections"
      :key="s.sourceId + '\u0000' + s.contact.key"
      class="person-section"
      :class="{ 'person-seen-here': s.seenHere }"
      :aria-label="sourceLabel(s.sourceId)"
    >
      <h2 class="person-source">
        <EntityChip
          :uri="sourceUri(s.sourceId)"
          :shown="s.sourceId"
          quiet
          :open-card="(source) => props.ctx.host.openCards(source)"
        />
        <span v-if="s.seenHere" class="person-badge">seen here</span>
        <span v-if="s.via.length" class="person-via">
          via {{ s.via.map(handleValue).join(", ") }}
        </span>
      </h2>
      <div class="person-record">
        <img
          v-if="photoOf(s.contact)"
          class="person-photo"
          :src="photoOf(s.contact)!"
          alt=""
          :title="model.mine ? 'Drag onto your contact’s photo to use it' : undefined"
        />
        <div>
          <p class="person-record-name">{{ nameOf(s.contact) }}</p>
          <ul class="person-handles">
            <li v-for="h in s.contact.handles" :key="h.value">
              <span
                v-if="h.handle"
                class="person-chip"
                :ref="(el) => drawInto(el, h.handle!, shownValue(h), 'identifier', version)"
              ></span>
              <span v-else>{{ h.value }}</span>
              <span v-if="h.label" class="person-muted">{{ h.label }}</span>
              <span v-if="stateOf(h.handle) === 'other'" class="person-muted">
                on {{ ownerName(h.handle) }}
              </span>
            </li>
          </ul>
          <button
            v-if="linkable(s).length"
            type="button"
            class="person-button person-link"
            :disabled="busy"
            @click.stop="addAll(s)"
          >
            {{ addLabel(s) }}
          </button>
        </div>
      </div>
      <dl v-if="sectionLines(s.contact, formatStamp).length" class="person-lines">
        <template
          v-for="line in sectionLines(s.contact, formatStamp)"
          :key="line.label + line.value"
        >
          <dt>{{ line.label }}</dt>
          <dd>{{ line.value }}</dd>
        </template>
      </dl>
      <details v-if="model.mine && sourceValues(s.contact).length" class="person-copy">
        <summary>Use what it says</summary>
        <ul class="person-copy-values">
          <li v-for="(v, i) in sourceValues(s.contact)" :key="i">
            <span>{{ valueText(v) }}</span>
            <span v-if="held(v)" class="person-muted">in your contact</span>
            <button
              v-else
              type="button"
              class="person-button"
              :aria-label="`Use ${valueText(v)}`"
              @click.stop="copyFrom(s, [v])"
            >
              Use
            </button>
          </li>
        </ul>
        <button
          type="button"
          class="person-button"
          :disabled="sourceValues(s.contact).every((v) => v.part === 'name' || held(v))"
          @click.stop="copyFrom(s, sourceValues(s.contact), true)"
        >
          Use all
        </button>
      </details>
    </section>

    <p v-if="!model.mine && model.sections.length === 0" class="person-muted person-empty">
      No source says anything more about {{ handleValue(props.handle) }}.
    </p>
    <ChipMenu
      v-if="menu"
      :entries="menu.entries"
      :x="menu.x"
      :y="menu.y"
      @pick="onMenuPick"
      @close="menu = null"
    />
  </div>
</template>

<style>
.person {
  height: 100%;
  overflow-y: auto;
  padding: 0.75rem 1rem;
  box-sizing: border-box;
  font-size: var(--datalib-font-size);
  color: var(--datalib-fg);
  background: var(--datalib-bg);
}
.person-head {
  display: flex;
  gap: 0.9rem;
  align-items: flex-start;
  margin-bottom: 0.75rem;
}
.person-head-photo {
  width: 64px;
  height: 64px;
  border-radius: 50%;
  object-fit: cover;
  flex-shrink: 0;
}
.person-head-text {
  min-width: 0;
}
.person-name {
  margin: 0;
  font-size: 1.3em;
  font-weight: 600;
}
.person-about {
  display: flex;
  align-items: center;
  gap: 0.3rem;
}
.person-about,
.person-muted {
  margin: 0.1rem 0 0;
  color: var(--datalib-muted);
}
.person-actions,
.person-create {
  display: flex;
  flex-wrap: wrap;
  gap: 0.5rem;
  margin-top: 0.5rem;
}
.person-found {
  display: flex;
  flex-wrap: wrap;
  gap: 0.5rem;
  margin: 0.5rem 0 0;
  padding: 0;
  list-style: none;
}
.person-date {
  width: 7.5em;
}
.person-button {
  font: inherit;
  padding: 0.2rem 0.6rem;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-surface);
  color: var(--datalib-fg);
  cursor: pointer;
}
.person-button:hover:not(:disabled) {
  background: var(--datalib-hover);
}
.person-input {
  font: inherit;
  padding: 0.2rem 0.4rem;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-input-bg);
  color: var(--datalib-fg);
}
.person-error {
  color: var(--datalib-error-fg);
}
.person-me {
  display: inline-flex;
  align-items: center;
  gap: 0.3rem;
  margin-bottom: 0.35rem;
}
.person-section {
  border-top: 1px solid var(--datalib-border-soft);
  padding: 0.6rem 0;
}
.person-seen-here {
  border-left: 3px solid var(--datalib-accent);
  padding-left: 0.6rem;
}
.person-source {
  display: flex;
  align-items: center;
  gap: 0.35rem;
  margin: 0 0 0.35rem;
  font-size: 1em;
  font-weight: 600;
}
.person-via {
  font-weight: normal;
  font-size: var(--datalib-font-size-small);
  color: var(--datalib-muted);
}
.person-link {
  margin-top: 0.4rem;
}
.person-handles li + li {
  margin-top: 0.15rem;
}
.person-badge {
  font-weight: normal;
  font-size: var(--datalib-font-size-small);
  color: var(--datalib-accent);
}
.person-record {
  display: flex;
  gap: 0.6rem;
  align-items: flex-start;
}
.person-photo {
  width: 48px;
  height: 48px;
  border-radius: 50%;
  object-fit: cover;
}
.person-record-name {
  margin: 0;
}
.person-handles {
  list-style: none;
  margin: 0.2rem 0 0;
  padding: 0;
}
.person-handles li {
  display: flex;
  align-items: center;
  gap: 0.35rem;
}
.person-stopped {
  text-decoration: line-through;
}
.person-lines {
  display: grid;
  grid-template-columns: max-content 1fr;
  gap: 0.15rem 0.75rem;
  margin: 0.4rem 0 0;
}
.person-lines dt {
  color: var(--datalib-muted);
}
.person-lines dd {
  margin: 0;
}
.person-copy {
  margin-top: 0.4rem;
}
.person-copy summary {
  cursor: pointer;
  color: var(--datalib-muted);
}
.person-copy-values {
  list-style: none;
  margin: 0.3rem 0;
  padding: 0;
}
.person-copy-values li {
  display: flex;
  align-items: center;
  gap: 0.5rem;
}
.person-copy-values li + li {
  margin-top: 0.15rem;
}
.person-empty {
  margin-top: 0.75rem;
}
</style>
