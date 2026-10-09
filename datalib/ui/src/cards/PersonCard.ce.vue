<script setup lang="ts">
// A person: your contact, when the handle is linked to one, and what each
// source says about them, each source in a section of its own. Opened by
// double-clicking a person chip. What it shows is `person.ts`; who a
// handle is comes from `people`, the resolver every chip shares.
import { computed, onMounted, onUnmounted, ref, watch } from "vue";
import { iconUrl } from "@/config/icons";
import { formatStamp } from "@/config/timeFormat";
import { searchSource } from "./cardSources";
import { uriFromEntity } from "./chipLinks";
import {
  canLinkHandles,
  createContact,
  handleIcon,
  handleValue,
  nameOf,
  people,
  searchQueryFor,
  suggestedName,
  type NormalizedContact,
} from "./contacts";
import { entities } from "./entities";
import { personHandles, personModel, sectionLines } from "./person";
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
    await entities.ask(model.value.sections.map((s) => sourceUri(s.sourceId)));
  } catch (e) {
    error.value = (e as Error).message;
  }
  version.value++;
}

const stopPeople = people.subscribe((changed) => {
  if (model.value.handles.some((h) => changed.has(h)) || changed.has(props.handle)) void ask();
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
section is one source's record of them, as that source has it. Sources
are never merged: one phone number can carry a WhatsApp account, a name
in a text-message backup and a Signal profile, and they need not agree.
The source of the chip you opened this from comes first.</p>
<p>A single click on a chip still links it to a contact; this card is for
reading.</p>
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

function handleMark(h: string | null): string | null {
  return h ? iconUrl(handleIcon(h)) : null;
}

function everything() {
  props.ctx.host.openCards(searchSource(searchQueryFor(props.handle)));
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
</script>

<template>
  <div class="person">
    <header class="person-head">
      <h1 class="person-name">{{ model.title }}</h1>
      <p class="person-about">
        <img
          v-if="handleMark(props.handle)"
          class="person-mark"
          :src="handleMark(props.handle)!"
          alt=""
        />
        <span class="person-handle">{{ handleValue(props.handle) }}</span>
        ·
        <template v-if="model.mine">your contact</template>
        <template v-else>not linked to a contact</template>
      </p>
      <div class="person-actions">
        <button type="button" class="person-button" @click="everything">
          Everything from them
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
    </header>

    <section v-if="model.mine" class="person-section person-mine" aria-label="Your contact">
      <h2 class="person-source">Your contact</h2>
      <ul class="person-handles">
        <li v-for="h in model.mine.handles" :key="h.value">
          <img
            v-if="handleMark(h.handle)"
            class="person-mark"
            :src="handleMark(h.handle)!"
            alt=""
          />
          <span :class="{ 'person-stopped': h.stopped_working_by }">{{ h.value }}</span>
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
      </h2>
      <div class="person-record">
        <img v-if="photoOf(s.contact)" class="person-photo" :src="photoOf(s.contact)!" alt="" />
        <div>
          <p class="person-record-name">{{ nameOf(s.contact) }}</p>
          <ul class="person-handles">
            <li v-for="h in s.contact.handles" :key="h.value">
              <img
                v-if="handleMark(h.handle)"
                class="person-mark"
                :src="handleMark(h.handle)!"
                alt=""
              />
              <span>{{ h.handle && h.medium === "other" ? handleValue(h.handle) : h.value }}</span>
              <span v-if="h.label" class="person-muted">{{ h.label }}</span>
            </li>
          </ul>
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
  margin-bottom: 0.75rem;
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
