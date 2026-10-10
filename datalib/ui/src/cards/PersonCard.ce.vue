<script setup lang="ts">
// A person: your contact, when the handle is linked to one, and what each
// source says about them, each source in a section of its own. Opened by
// double-clicking a person chip. What it shows is `person.ts`; who a
// handle is comes from `people`, the resolver every chip shares. Every
// handle on the card is a chip, pale blue where it is linked to a
// contact and grey where not, with the same click, double-click and
// right-click as a chip in a document (docs/dev/chips.md § Clicks).
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { copyToClipboard } from "@/clipboard";
import { iconUrl } from "@/config/icons";
import { formatStamp } from "@/config/timeFormat";
import { openExternal } from "@/externalLinks";
import { pushToast } from "@/toasts";
import { personSource, searchSource } from "./cardSources";
import { isBrowserClick } from "./chatLink";
import { uriFromEntity } from "./chipLinks";
import ChipMenu from "./ChipMenu.ce.vue";
import {
  canLinkHandles,
  chipCell,
  chipMenu,
  composeUri,
  copyText,
  createContact,
  handleValue,
  identifierCell,
  linkHandle,
  nameOf,
  NOBODY,
  people,
  searchQueryFor,
  suggestedName,
  type ChipMenuEntry,
  type ChipMenuId,
  type NormalizedContact,
} from "./contacts";
import { entities } from "./entities";
import HandlePopover from "./HandlePopover.ce.vue";
import {
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
when you have linked this handle to one, is what you wrote; every other
section is one source's record of them, as that source has it, shown
because it holds one of your contact's handles (its "via" line says
which). Sources are never merged: one phone number can carry a WhatsApp
account, a name in a text-message backup and a Signal profile, and they
need not agree. The source of the chip you opened this from comes
first.</p>
<p>Every handle is a chip: pale blue when it is linked to a contact,
grey when not. Click one to link it, right-click for its menu, or use a
section's <em>Link</em> button to link all of that source's unlinked
handles to your contact at once.</p>
`);

function sourceUri(sourceId: string): string {
  return uriFromEntity("group", sourceId);
}

function sourceLabel(sourceId: string): string {
  void version.value;
  return entities.get(sourceUri(sourceId))?.label || sourceId;
}

function sourceIcon(sourceId: string): string | null {
  void version.value;
  return iconUrl(entities.get(sourceUri(sourceId))?.icon ?? null);
}

function everything() {
  props.ctx.host.openCards(searchSource(searchQueryFor(props.handle)));
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
  const canLink = canLinkHandles();
  el.replaceChildren(
    as === "person"
      ? chipCell(handle, shown, who, canLink)
      : identifierCell(handle, shown, who, canLink),
  );
}

function chipAt(ev: Event): HTMLElement | null {
  const t = ev.target;
  return t instanceof Element ? t.closest<HTMLElement>("a.chip[data-handle]") : null;
}

type Popover = {
  handle: string;
  shownAs: string;
  resolved: NormalizedContact | null;
  x: number;
  y: number;
};
const popover = ref<Popover | null>(null);
const menu = ref<{ chip: HTMLElement; entries: ChipMenuEntry[]; x: number; y: number } | null>(
  null,
);

function openPopover(chip: HTMLElement, x: number, y: number) {
  const handle = chip.dataset.handle ?? "";
  popover.value = {
    handle,
    shownAs: chip.dataset.shownAs ?? "",
    resolved: people.get(handle)?.mine ?? null,
    x,
    y,
  };
}

/// A click links (the popover), where a contacts app is there to; a
/// click with a modifier is the browser's, as on any link.
function onClick(ev: MouseEvent) {
  const chip = chipAt(ev);
  if (!chip || isBrowserClick(ev)) return;
  ev.preventDefault();
  if (canLinkHandles()) openPopover(chip, ev.clientX, ev.clientY);
}

function onDblClick(ev: MouseEvent) {
  const chip = chipAt(ev);
  if (!chip) return;
  ev.preventDefault();
  const handle = chip.dataset.handle ?? "";
  if (handle !== props.handle) props.ctx.host.openCards(personSource(handle, { seenIn: null }));
}

function onContextMenu(ev: MouseEvent) {
  const chip = chipAt(ev);
  if (!chip) return;
  ev.preventDefault();
  const handle = chip.dataset.handle ?? "";
  menu.value = {
    chip,
    entries: chipMenu(
      handle,
      chip.dataset.shownAs ?? "",
      people.get(handle) ?? NOBODY,
      canLinkHandles(),
    ),
    x: ev.clientX,
    y: ev.clientY,
  };
}

async function onMenuPick(id: ChipMenuId) {
  const m = menu.value;
  menu.value = null;
  if (!m) return;
  const handle = m.chip.dataset.handle ?? "";
  const name = people.get(handle)?.mine
    ? nameOf(people.get(handle)!.mine!)
    : (m.chip.dataset.label ?? "");
  const copy = async (text: string) => {
    if (await copyToClipboard(text)) pushToast(`Copied ${text}`, "info");
    else pushToast("The clipboard refused the copy");
  };
  switch (id) {
    case "open":
      if (handle !== props.handle) props.ctx.host.openCards(personSource(handle, { seenIn: null }));
      break;
    case "compose": {
      const mailto = composeUri(handle);
      if (mailto) void openExternal(mailto);
      break;
    }
    case "copy-name":
      await copy(name);
      break;
    case "copy-id":
      await copy(handleValue(handle));
      break;
    case "copy-both":
      await copy(copyText(handle, name));
      break;
    case "search":
      props.ctx.host.openCards(searchSource(searchQueryFor(handle)));
      break;
    case "edit":
      openPopover(m.chip, m.x, m.y);
      break;
  }
}

// ── Linking a source's handles to your contact ────────────────────────

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

const linking = ref(false);
async function linkAll(s: PersonSection) {
  const mine = model.value.mine;
  if (!mine) return;
  linking.value = true;
  error.value = null;
  try {
    for (const h of linkable(s)) await linkHandle(h, mine.key);
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    linking.value = false;
  }
}

function linkLabel(s: PersonSection): string {
  const n = linkable(s).length;
  return `Link ${n === 1 ? "its other handle" : `its ${n} other handles`} to ${model.value.title}`;
}

// ── Create a contact, for a handle not linked to one ──────────────────

const canCreate = computed(() => {
  void version.value;
  return !model.value.mine && canLinkHandles();
});
const newName = ref("");
const creating = ref(false);
watch(
  () => model.value.sections[0]?.contact,
  (first) => {
    if (!newName.value && first) newName.value = suggestedName(nameOf(first), props.handle);
  },
  { immediate: true },
);

async function create() {
  const name = newName.value.trim();
  if (!name) return;
  creating.value = true;
  error.value = null;
  try {
    await createContact(name, [props.handle]);
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    creating.value = false;
  }
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
        <p class="person-about">
          <template v-if="model.mine">
            Your contact · {{ model.sections.length }}
            {{ model.sections.length === 1 ? "source holds" : "sources hold" }} its handles
          </template>
          <template v-else>Not linked to a contact</template>
        </p>
        <div class="person-actions">
          <button type="button" class="person-button" @click.stop="everything">
            Everything with them
          </button>
        </div>
        <form v-if="canCreate" class="person-create" @submit.prevent="create">
          <input
            v-model="newName"
            class="person-input"
            aria-label="Name for a new contact"
            placeholder="Name"
          />
          <button type="submit" class="person-button" :disabled="creating || !newName.trim()">
            Create contact
          </button>
        </form>
        <p v-if="error" class="person-error">{{ error }}</p>
      </div>
    </header>

    <section v-if="model.mine" class="person-section person-mine" aria-label="Your contact">
      <h2 class="person-source">Your contact</h2>
      <ul class="person-handles">
        <li v-for="h in model.mine.handles" :key="h.value">
          <span
            v-if="h.handle"
            class="person-chip"
            :ref="(el) => drawInto(el, h.handle!, shownValue(h), 'identifier', version)"
          ></span>
          <span v-else>{{ h.value }}</span>
          <span v-if="h.stopped_working_by" class="person-muted">
            stopped working by {{ h.stopped_working_by }}
          </span>
        </li>
      </ul>
      <dl v-if="sectionLines(model.mine, formatStamp).length" class="person-lines">
        <template
          v-for="line in sectionLines(model.mine, formatStamp)"
          :key="line.label + line.value"
        >
          <dt>{{ line.label }}</dt>
          <dd>{{ line.value }}</dd>
        </template>
      </dl>
    </section>

    <section
      v-for="s in model.sections"
      :key="s.sourceId + '\u0000' + s.contact.key"
      class="person-section"
      :class="{ 'person-seen-here': s.seenHere }"
      :aria-label="sourceLabel(s.sourceId)"
    >
      <h2 class="person-source">
        <img
          v-if="sourceIcon(s.sourceId)"
          class="person-mark"
          :src="sourceIcon(s.sourceId)!"
          alt=""
        />
        {{ sourceLabel(s.sourceId) }}
        <span v-if="s.seenHere" class="person-badge">seen here</span>
        <span v-if="s.via.length" class="person-via">
          via {{ s.via.map(handleValue).join(", ") }}
        </span>
      </h2>
      <div class="person-record">
        <img v-if="photoOf(s.contact)" class="person-photo" :src="photoOf(s.contact)!" alt="" />
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
                linked to {{ ownerName(h.handle) }}
              </span>
            </li>
          </ul>
          <button
            v-if="linkable(s).length"
            type="button"
            class="person-button person-link"
            :disabled="linking"
            @click.stop="linkAll(s)"
          >
            {{ linkLabel(s) }}
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
    </section>

    <p v-if="!model.mine && model.sections.length === 0" class="person-muted person-empty">
      No source says anything more about {{ handleValue(props.handle) }}.
    </p>
    <ChipMenu
      v-if="menu"
      :entries="menu.entries"
      :x="menu.x"
      :y="menu.y"
      @pick="(id) => onMenuPick(id as ChipMenuId)"
      @close="menu = null"
    />
    <HandlePopover
      v-if="popover"
      :key="popover.handle"
      v-bind="popover"
      @close="popover = null"
      @changed="ask"
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
.person-mark {
  width: 14px;
  height: 14px;
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
.person-empty {
  margin-top: 0.75rem;
}
</style>
