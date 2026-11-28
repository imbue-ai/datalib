<script setup lang="ts">
// A person: your contact, when they are one (edited in `ContactEditor`),
// else the handles adding them would take, ticked; and what each source
// says about them, each source in a section of its own. Opened by
// double-clicking a person's chip. What it shows is `person.ts`; who a
// handle is comes from `people`, the resolver every chip shares. Every
// handle on the card is a chip, pale blue where it is on a contact and
// grey where not, with the double-click and right-click of a chip in a
// document (docs/dev/chips.md § Clicks); its menu also puts the handle on
// your contact or takes it off.
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from "vue";
import { formatStamp } from "@/config/timeFormat";
import { contactSubject, personSource, searchSource } from "./cardSources";
import { isBrowserClick } from "./chatLink";
import { uriFromEntity } from "./chipLinks";
import ChipMenu from "./ChipMenu.ce.vue";
import ContactEditor from "./ContactEditor.ce.vue";
import { handleValueOf, holds, valueText, type SourceValue } from "./contactFields";
import EntityChip from "./EntityChip.ce.vue";
import { contactPageCell, contactPages } from "./contactPages";
import {
  chipCell,
  confirmReview,
  contactCell,
  addedSummary,
  addPeople,
  contactsById,
  createContact,
  handleValue,
  hasContactsApp,
  identifierCell,
  isMinted,
  labelContact,
  labelCounts,
  labelsOf,
  linkHandle,
  nameOf,
  NOBODY,
  notTheirs,
  people,
  rejectReview,
  searchContacts,
  setMe,
  setStoppedWorking,
  todayPartialDate,
  unlinkHandle,
  type ContactSummary,
  type HandleFrom,
  type NormalizedContact,
  type WhenHeld,
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
  nameToOffer,
  offerNote,
  offers,
  othersWith,
  parseSubject,
  personModel,
  sectionLines,
  shownHandles,
  type Answers,
  type Offer,
  type PersonSection,
} from "./person";
import SuggestionItem from "./SuggestionItem.ce.vue";
import { loadSuggestions, suggestionsAbout, type Suggestion } from "./suggestions";
import type { CardCtx } from "./types";

const props = defineProps<{ ctx: CardCtx; subject: string; seenIn: string | null }>();

/// Who the page is about: what it was opened on, until someone with no
/// handle is added here, whom only their contact then names.
const written = ref(props.subject);
watch(
  () => props.subject,
  (s) => (written.value = s),
);
const subject = computed(() => parseSubject(written.value));

// Bumped whenever an answer this card reads changes, so the computed
// model reads the resolvers again.
const version = ref(0);
const error = ref<string | null>(null);

const answers: Answers = {
  who: (h) => people.get(h),
  contact: (id) => contactsById.get(id),
  page: (p) => contactPages.get(p),
};
const modelNow = () => personModel(subject.value, props.seenIn, answers);
const model = computed(() => {
  void version.value;
  return modelNow();
});
/// The answers about who the page is about are in, so one with nothing to
/// show can say so.
const asked = ref(false);
/// The handles you took off your contact as not theirs.
const turnedDown = ref<ReadonlySet<string>>(new Set());

/// What agents suggested that waits for the person; the page shows those
/// about whoever it is about.
const waiting = ref<Suggestion[]>([]);
const aboutHere = computed(() => {
  void version.value;
  const now = model.value;
  const opened = subject.value.kind === "handle" ? [subject.value.handle] : [];
  return suggestionsAbout(
    waiting.value,
    [...opened, ...now.handles, ...shownHandles(now)],
    now.mine?.key ?? null,
  );
});

async function ask() {
  error.value = null;
  // A contacts app from before suggestions answers 404: none to show.
  const suggested = loadSuggestions().catch(() => []);
  try {
    const s = subject.value;
    if (s.kind === "handle") await people.ask([s.handle]);
    if (s.kind === "contact") await contactsById.ask([s.id]);
    if (s.kind === "page") await contactPages.ask([s.page]);
    // Each answer names more handles: a card's numbers say whose contact
    // it is, whose handles say which sources know them.
    await people.ask(shownHandles(modelNow()));
    await people.ask(modelNow().handles);
    const now = modelNow();
    await people.ask(shownHandles(now));
    await entities.ask(now.sections.map((x) => sourceUri(x.sourceId)));
    turnedDown.value = now.mine ? await notTheirs(now.mine.key) : new Set();
    myLabels.value = now.mine ? await labelsOf(now.mine.key) : [];
    knownLabels.value = now.mine ? (await labelCounts()).map((l) => l.label) : [];
    waiting.value = await suggested;
  } catch (e) {
    error.value = (e as Error).message;
  }
  asked.value = true;
  version.value++;
  await takeKeyboard();
}

const stopPeople = people.subscribe((changed) => {
  const now = modelNow();
  const opened = subject.value.kind === "handle" ? [subject.value.handle] : [];
  if ([...opened, ...now.handles, ...shownHandles(now)].some((h) => changed.has(h))) void ask();
});
const stopContacts = contactsById.subscribe((ids) => {
  if (subject.value.kind === "contact" && ids.has(subject.value.id)) void ask();
});
const stopPages = contactPages.subscribe((pages) => {
  if (subject.value.kind === "page" && pages.has(subject.value.page)) void ask();
});
const stopEntities = entities.subscribe(() => version.value++);
onUnmounted(() => {
  stopPeople();
  stopContacts();
  stopPages();
  stopEntities();
});
onMounted(ask);
watch([written, () => props.seenIn], () => {
  asked.value = false;
  ticks.value = {};
  heldAs.value = {};
  void ask();
});

watch(
  () => model.value.title,
  (title) => props.ctx.setTitle(title || "Contact"),
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
<p>When they are not in your contacts, <em>Add to contacts</em> adds them
with every handle ticked: the one you opened, and every other a source
says is theirs. Untick what is wrong. One that is on another contact is not
ticked: tick it and say whether <em>both</em> have it, as two people who
share an address do, or it <em>moves here</em> and comes off the other. A
number a card writes without its country code is offered as the full
number with the same digits, where a source knows one. Opening this page
adds nobody.</p>
<p>An address several of your contacts share has a page of its own, with
each of them on it. Their own pages say it is shared.</p>
<p>Under <em>Your contact</em>, <em>Edit</em> changes its name, note and
details. Each address and number on it is one line: its label and ×
change it at once. Anything else you type is kept as a draft that only
you see, even after a reload, until <em>Save</em> publishes it or
<em>Discard</em> drops it. An address or number you type is theirs from
the Save on, and when another contact has it, you say whether both have
it or it moves here. If the contact was changed somewhere else
meanwhile, Save says so and marks what changed rather than overwriting
it. A photo is set or removed at once.</p>
<p><em>This is me</em> says your contact is you: <code>from:me</code>,
<code>to:me</code> and <code>with:me</code> then find every handle
on it, an old address or a second number included. More than one
contact can be you, such as a duplicate not merged yet.</p>
<p>Beside what each source says (a number, a title, an address),
<em>Use</em> puts it in your contact's draft; you pick where sources
disagree. A value you use does not follow its source afterwards: the
card says when the source now says something else.</p>
<p>Every handle is a chip: pale blue when it is on a contact, grey when
not. Right-click one to add it to your contact (beside whoever else has
it, or moved off them), to say it is <em>not theirs</em>, or that it no
longer works for them. <em>Not theirs</em> is remembered, so no agent
suggests it again.</p>
`);

function sourceUri(sourceId: string): string {
  return uriFromEntity("group", sourceId);
}

function sourceLabel(sourceId: string): string {
  void version.value;
  return entities.get(sourceUri(sourceId))?.label || sourceId;
}

/// The search for everything with them; none for a card with no handle,
/// which no search names.
const everythingQuery = computed(() => {
  const s = subject.value;
  const { mine } = model.value;
  if (s.kind === "handle") return everythingWith(s.handle, people.get(s.handle)?.mine ?? []);
  return mine ? everythingWith("", [mine]) : null;
});
function everything() {
  if (everythingQuery.value) props.ctx.host.openCards(searchSource(everythingQuery.value));
}

// ── Chips ─────────────────────────────────────────────────────────────

/** What a handle line shows: the identifier as the source wrote it, or,
 *  for a kind with no address of its own (a Slack id), the handle. */
/** The handles that put a record on this page, as a person reads them;
 *  an id a service minted is left to the record's own chip below. */
function readableVia(s: { via: string[] }): string[] {
  return s.via.filter((h) => !isMinted(h)).map(handleValue);
}

function shownValue(h: NormalizedContact["handles"][number]): string {
  return h.handle && h.medium === "other" ? handleValue(h.handle) : h.value;
}

/// The heading: whoever the page is about, as their chip.
function drawHeading(el: unknown, _version: number) {
  if (!(el instanceof HTMLElement)) return;
  const s = subject.value;
  const { title, mine } = model.value;
  if (s.kind === "handle") {
    el.replaceChildren(chipCell(s.handle, title, people.get(s.handle) ?? NOBODY));
  } else if (mine) {
    el.replaceChildren(contactCell(mine.key, title, mine));
  } else if (s.kind === "page") {
    el.replaceChildren(contactPageCell(s.page, title, contactPages.get(s.page)));
  } else {
    el.replaceChildren();
  }
}

/// One of the contacts an address is shared by, as their chip.
function drawContact(el: unknown, c: NormalizedContact, _version: number) {
  if (!(el instanceof HTMLElement)) return;
  el.replaceChildren(contactCell(c.key, nameOf(c), c));
}

/// Draw a handle's chip into its slot, showing the identifier. Called from a
/// function ref, so every render (each `version` bump) draws it again from
/// the latest answer.
function drawInto(el: unknown, handle: string, shown: string, _version: number) {
  if (!(el instanceof HTMLElement)) return;
  el.replaceChildren(identifierCell(handle, shown, people.get(handle) ?? NOBODY));
}

function chipAt(ev: Event): HTMLElement | null {
  const t = ev.target;
  return t instanceof Element
    ? t.closest<HTMLElement>("a.chip[data-handle], a.chip[data-contact], a.chip[data-contact-page]")
    : null;
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
    if (source !== personSource(written.value, { seenIn: null })) props.ctx.host.openCards(source);
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
      case "handle-share":
        return linkHandle(handle, mine.key, "share");
      case "handle-move":
        return linkHandle(handle, mine.key, "move");
      case "handle-remove":
        return unlinkHandle(handle, mine.key);
      case "handle-stopped":
        return setStoppedWorking(handle, mine.key, todayPartialDate());
      case "handle-works":
        return setStoppedWorking(handle, mine.key, null);
    }
  });
}

function stoppedBy(handle: string, ev: Event) {
  const mine = model.value.mine;
  const by = (ev.target as HTMLInputElement).value.trim();
  if (mine) void act(() => setStoppedWorking(handle, mine.key, by || null));
}

const busy = ref(false);

async function answerSuggestion(s: Suggestion, yes: boolean) {
  busy.value = true;
  error.value = null;
  try {
    await (yes ? confirmReview : rejectReview)([s.subject]);
    await ask();
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    busy.value = false;
  }
}
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
  return linkableHandles(s, model.value, whoOf, turnedDown.value);
}

/// The full numbers a number with no country code may be.
function numbersLike(value: string): string {
  return (model.value.fullNumbers[value] ?? []).map(handleValue).join(" or ");
}

function stateOf(h: string | null) {
  return h ? linkState(h, model.value.mine, whoOf) : null;
}

/// Who else has a handle, by name: "Robert Picard and Marie Picard".
function othersNamed(h: string | null): string {
  return h ? othersWith(h, model.value.mine, whoOf).map(nameOf).join(" and ") : "";
}

async function addAll(s: PersonSection) {
  const mine = model.value.mine;
  if (!mine) return;
  await act(async () => {
    for (const h of linkable(s)) await linkHandle(h, mine.key, undefined, saidBy(s, h));
  });
}

/// Your contact's labels, and every label your contacts carry, which the
/// box offers.
const myLabels = ref<string[]>([]);
const knownLabels = ref<string[]>([]);
const newLabel = ref("");
const offeredLabels = computed(() => knownLabels.value.filter((l) => !myLabels.value.includes(l)));

async function giveLabel() {
  const mine = model.value.mine;
  const label = newLabel.value.trim();
  if (!mine || !label) return;
  await act(async () => {
    await labelContact(mine.key, label, true);
    newLabel.value = "";
    await ask();
  });
}

async function takeLabel(label: string) {
  const mine = model.value.mine;
  if (mine) await act(() => labelContact(mine.key, label, false).then(ask));
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
  if (model.value.mine || model.value.sharedBy.length || !hasContactsApp()) return false;
  switch (subject.value.kind) {
    case "handle":
      return true;
    case "page":
      return model.value.sections[0]?.contact.kind === "person";
    case "contact":
      return false;
  }
});

/// What the person ticked or unticked; the rest are as offered.
const ticks = ref<Record<string, boolean>>({});
const offered = computed(() => {
  void version.value;
  return canAdd.value ? offers(subject.value, model.value, answers) : [];
});
const isTicked = (o: Offer) => ticks.value[o.handle] ?? o.ticked;
const ticked = computed(() => offered.value.filter(isTicked));
function tick(o: Offer, ev: Event) {
  ticks.value = { ...ticks.value, [o.handle]: (ev.target as HTMLInputElement).checked };
}
/// For a ticked handle another contact has: both have it, unless the
/// person says it moves here.
const heldAs = ref<Record<string, WhenHeld>>({});
const whenHeld = (o: Offer): WhenHeld | undefined =>
  o.owners.length ? (heldAs.value[o.handle] ?? "share") : undefined;
function holdAs(o: Offer, held: WhenHeld) {
  heldAs.value = { ...heldAs.value, [o.handle]: held };
}
const newName = ref("");
watch(
  () => nameToOffer(subject.value, model.value),
  (name) => {
    newName.value ||= name;
  },
  { immediate: true },
);

/// Your contacts the name finds, the one of exactly that name first: the
/// handle may belong on one of them.
const found = ref<ContactSummary[]>([]);
let searched = 0;
async function findContacts() {
  const mine = ++searched;
  const name = newName.value.trim();
  let hits: ContactSummary[] = [];
  try {
    if (canAdd.value && name) hits = await searchContacts(name);
  } catch (e) {
    error.value = (e as Error).message;
  }
  if (mine !== searched) return;
  found.value = [...hits.filter(named), ...hits.filter((c) => !named(c))].slice(0, 5);
}
const named = (c: ContactSummary) => c.name.toLowerCase() === newName.value.trim().toLowerCase();
watch([canAdd, newName], findContacts);

/// What the card an offered handle came from said of it.
const offerSaid = (o: Offer): HandleFrom => ({ label: o.label, copied_from: o.card });

const addTo = (c: ContactSummary) =>
  act(async () => {
    for (const o of ticked.value) {
      await linkHandle(o.handle, c.contact_id, whenHeld(o), offerSaid(o));
    }
  });
const add = () =>
  act(async () => {
    const name = newName.value.trim();
    if (ticked.value.length === 0) {
      // Nothing but the contact names someone added with no handle.
      written.value = contactSubject(await createContact(name, []));
      return;
    }
    const held: Record<string, WhenHeld> = {};
    for (const o of ticked.value) {
      const when = whenHeld(o);
      if (when) held[o.handle] = when;
    }
    const added = await addPeople([
      {
        name,
        handles: ticked.value.filter((o) => !o.owners.length).map((o) => o.handle),
        held,
        said: Object.fromEntries(ticked.value.map((o) => [o.handle, offerSaid(o)])),
      },
    ]);
    if (added.skipped.length) error.value = addedSummary(added);
  });
function openContact(c: ContactSummary) {
  props.ctx.host.openCards(personSource(contactSubject(c.contact_id), { seenIn: null }));
}

const addForm = ref<HTMLElement | null>(null);
/// Opened from a chip, the page takes the keyboard, so that Enter adds.
async function takeKeyboard() {
  if (justOpened.value?.subject !== written.value) return;
  await findContacts();
  await nextTick();
  if (justOpened.value?.subject !== written.value) return;
  justOpened.value = null;
  const first = found.value[0];
  const button = first && named(first) ? ".person-found button" : "button[type=submit]";
  addForm.value?.querySelector<HTMLButtonElement>(button)?.focus();
}
watch(justOpened, () => {
  if (asked.value) void takeKeyboard();
});

// ── Copying what a source says into your contact ─────────────────────

const editor = ref<InstanceType<typeof ContactEditor> | null>(null);

/// What *Use* beside a value copies, while your contact does not hold it.
function usable(v: SourceValue | null): SourceValue | null {
  if (!v || !model.value.mine) return null;
  const current = editor.value?.current;
  return current && holds(current, v) ? null : v;
}

const nameValue = (c: NormalizedContact): SourceValue | null => {
  const name = c.names[0]?.trim();
  return name ? { part: "name", value: name } : null;
};

async function copyFrom(s: PersonSection, v: SourceValue) {
  await editor.value?.copy(v, { source_id: s.sourceId, key: s.contact.key });
}

/// What a source's card says of one of its handles, which linking it keeps.
function saidBy(s: PersonSection, handle: string): HandleFrom {
  const h = s.contact.handles.find((x) => x.handle === handle);
  return { label: h?.label ?? null, copied_from: { source_id: s.sourceId, key: s.contact.key } };
}

/// What the page says where it has nobody to show.
const nothing = computed(() => {
  const s = subject.value;
  if (model.value.mine || model.value.sections.length > 0) return null;
  switch (s.kind) {
    case "handle":
      return `No source says anything more about ${handleValue(s.handle)}.`;
    case "contact":
      return asked.value ? "This contact is no longer in your contacts." : null;
    case "page":
      return asked.value ? "No source has this card any more." : null;
  }
});

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
        <h1 class="person-name" :ref="(el) => drawHeading(el, version)"></h1>
        <p v-if="model.mine || model.sharedBy.length || canAdd" class="person-about">
          <template v-if="model.sharedBy.length">
            Shared by {{ model.sharedBy.length }} of your contacts
          </template>
          <template v-else-if="model.mine">
            In your contacts · known to {{ model.sections.length }}
            {{ model.sections.length === 1 ? "source" : "sources" }}
          </template>
          <template v-else>Not in your contacts</template>
        </p>
        <div class="person-actions">
          <button
            v-if="everythingQuery"
            type="button"
            class="person-button"
            @click.stop="everything"
          >
            Everything with them
          </button>
        </div>
        <ul v-if="model.sharedBy.length" class="person-shared" aria-label="Who has it">
          <li
            v-for="c in model.sharedBy"
            :key="c.key"
            :ref="(el) => drawContact(el, c, version)"
          ></li>
        </ul>
        <div v-if="canAdd" ref="addForm" class="person-add">
          <ul v-if="offered.length" class="person-offers" aria-label="Handles to add">
            <li v-for="o in offered" :key="o.handle">
              <input
                type="checkbox"
                :aria-label="o.shown"
                :checked="isTicked(o)"
                :disabled="busy"
                @change="tick(o, $event)"
              />
              <span
                class="person-chip"
                :ref="(el) => drawInto(el, o.handle, o.shown, version)"
              ></span>
              <span v-if="o.label" class="person-muted">{{ o.label }}</span>
              <span class="person-muted">{{ offerNote(o, sourceLabel) }}</span>
              <span
                v-if="o.owners.length && isTicked(o)"
                class="person-held"
                role="radiogroup"
                :aria-label="`${o.shown}: ${offerNote(o, sourceLabel)}`"
              >
                <label>
                  <input
                    type="radio"
                    :name="`held-${o.handle}`"
                    :checked="whenHeld(o) === 'share'"
                    :disabled="busy"
                    @change="holdAs(o, 'share')"
                  />
                  Both
                </label>
                <label>
                  <input
                    type="radio"
                    :name="`held-${o.handle}`"
                    :checked="whenHeld(o) === 'move'"
                    :disabled="busy"
                    @change="holdAs(o, 'move')"
                  />
                  Move it here
                </label>
              </span>
            </li>
          </ul>
          <ul v-if="found.length" class="person-found">
            <li v-for="c in found" :key="c.contact_id">
              <button
                v-if="ticked.length"
                type="button"
                class="person-button"
                :disabled="busy"
                @click="addTo(c)"
              >
                Add to <b>{{ c.name }}</b>
              </button>
              <button v-else type="button" class="person-button" @click="openContact(c)">
                Open <b>{{ c.name }}</b>
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

    <section v-if="aboutHere.length" class="person-section" aria-label="Suggested">
      <h2 class="person-source">Suggested</h2>
      <SuggestionItem
        v-for="x in aboutHere"
        :key="x.subject"
        :suggestion="x"
        :busy="busy"
        @yes="answerSuggestion(x, true)"
        @no="answerSuggestion(x, false)"
      />
    </section>

    <section v-if="model.mine" class="person-section person-mine" aria-label="Your contact">
      <h2 class="person-source">Your contact</h2>
      <label class="person-me" title="from:me, to:me and with:me find every handle on this contact">
        <input
          type="checkbox"
          data-testid="contact-is-me"
          :checked="!!model.mine.account"
          :disabled="busy"
          @change="toggleMe"
        />
        This is me
      </label>
      <div class="person-labels" role="group" aria-label="Labels">
        <span v-for="l in myLabels" :key="l" class="person-label">
          {{ l }}
          <button
            type="button"
            class="person-label-off"
            :aria-label="`Take the label ${l} off`"
            :disabled="busy"
            @click.stop="takeLabel(l)"
          >
            ×
          </button>
        </span>
        <form class="person-label-add" @submit.prevent="giveLabel">
          <input
            v-model="newLabel"
            class="person-input"
            list="person-known-labels"
            placeholder="Add a label"
            aria-label="Add a label"
            :disabled="busy"
          />
          <datalist id="person-known-labels">
            <option v-for="l in offeredLabels" :key="l" :value="l"></option>
          </datalist>
        </form>
      </div>
      <ul v-if="!editor?.editing" class="person-handles">
        <li v-for="h in model.mine.handles" :key="h.value">
          <span
            v-if="h.handle"
            class="person-chip"
            :ref="(el) => drawInto(el, h.handle!, shownValue(h), version)"
          ></span>
          <span v-else>{{ h.value }}</span>
          <span v-if="h.label" class="person-muted">{{ h.label }}</span>
          <span v-if="othersNamed(h.handle)" class="person-muted">
            shared with {{ othersNamed(h.handle) }}
          </span>
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
        <span v-if="s.sameDigits.length" class="person-badge">same digits</span>
        <span v-if="readableVia(s).length" class="person-via">
          via {{ readableVia(s).join(", ") }}
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
          <p class="person-record-name">
            {{ nameOf(s.contact) }}
            <button
              v-if="usable(nameValue(s.contact))"
              type="button"
              class="person-button person-use"
              :aria-label="`Use ${valueText(nameValue(s.contact)!)}`"
              @click.stop="copyFrom(s, nameValue(s.contact)!)"
            >
              Use
            </button>
          </p>
          <ul class="person-handles">
            <li v-for="h in s.contact.handles" :key="h.value">
              <span
                v-if="h.handle"
                class="person-chip"
                :ref="(el) => drawInto(el, h.handle!, shownValue(h), version)"
              ></span>
              <span v-else>{{ h.value }}</span>
              <span v-if="h.label" class="person-muted">{{ h.label }}</span>
              <span v-if="!h.handle && numbersLike(h.value)" class="person-muted">
                same digits as {{ numbersLike(h.value) }}
              </span>
              <span v-if="stateOf(h.handle) === 'other'" class="person-muted">
                on {{ othersNamed(h.handle) }}
              </span>
              <button
                v-if="stateOf(h.handle) !== 'this' && usable(handleValueOf(h))"
                type="button"
                class="person-button person-use"
                :aria-label="`Use ${valueText(handleValueOf(h)!)}`"
                @click.stop="copyFrom(s, handleValueOf(h)!)"
              >
                Use
              </button>
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
          <dd>
            {{ line.value }}
            <button
              v-if="usable(line.use)"
              type="button"
              class="person-button person-use"
              :aria-label="`Use ${valueText(line.use!)}`"
              @click.stop="copyFrom(s, line.use!)"
            >
              Use
            </button>
          </dd>
        </template>
      </dl>
    </section>

    <p v-if="nothing" class="person-muted person-empty">{{ nothing }}</p>
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
.person-offers {
  margin: 0.5rem 0 0;
  padding: 0;
  list-style: none;
}
.person-offers li {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.35rem;
}
.person-offers li + li {
  margin-top: 0.15rem;
}
.person-held {
  display: inline-flex;
  gap: 0.6rem;
}
.person-held label {
  display: inline-flex;
  align-items: center;
  gap: 0.2rem;
}
.person-shared {
  display: flex;
  flex-wrap: wrap;
  gap: 0.5rem;
  margin: 0.5rem 0 0;
  padding: 0;
  list-style: none;
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
.person-labels {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.3rem;
  margin-bottom: 0.45rem;
}
.person-label {
  display: inline-flex;
  align-items: center;
  gap: 0.15rem;
  padding: 0 0.15rem 0 0.5rem;
  border: 1px solid var(--datalib-border);
  border-radius: 999px;
  background: var(--datalib-surface);
}
.person-label-off {
  font: inherit;
  padding: 0 0.3rem;
  border: none;
  background: none;
  color: var(--datalib-muted);
  cursor: pointer;
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
.person-use {
  margin-left: 0.4rem;
  padding: 0 0.4rem;
  font-size: var(--datalib-font-size-small);
}
.person-empty {
  margin-top: 0.75rem;
}
</style>
