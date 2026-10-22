<script setup lang="ts">
// Your contact's own part of the person card: its fields and note, read
// as published, and the editor that changes them, the name with them.
// An edit goes to the contact's draft as you type, which nobody else
// sees, until Save publishes it or Discard drops it. A save that would
// overwrite a change made elsewhere is refused and the card marks what
// moved (`contactEdit.ts`). The photo changes at once, not through the
// draft. docs/dev/contacts.md § "Fields, and editing a contact through
// a draft".
import { computed, nextTick, onMounted, onUnmounted, ref, watch } from "vue";
import {
  conflictText,
  editLines,
  FIELD_KINDS,
  hasChanges,
  kindLabel,
  merge3,
  newField,
  photoDrop,
  removedText,
  takeTheirs,
  toStore,
  type Conflict,
  type ContactEdit,
  type DraftView,
  type FieldKind,
} from "./contactEdit";
import {
  autosaveDraft,
  discardDraft,
  draftOf,
  dropContactPhoto,
  movesPeople,
  openDraft,
  PHOTO_TYPES,
  publishedEdit,
  saveDraft,
  setContactPhoto,
} from "./contacts";
import { subscribeLive } from "@/live";

const props = defineProps<{ contactId: string; photoUrl: string | null }>();

const AUTOSAVE_PAUSE_MS = 700;

const published = ref<ContactEdit | null>(null);
const editing = ref(false);
const form = ref<ContactEdit>({ name: "", note: null, fields: [] });
const base = ref<ContactEdit | null>(null);
const seen = ref("");
const conflicts = ref<Conflict[]>([]);
/** What the card last did, shown beside Edit. */
const status = ref("");
/** An autosave is waiting for typing to pause, or on its way. */
const pending = ref(false);
const error = ref<string | null>(null);
const stale = ref(false);
const busy = ref(false);
const fieldsBox = ref<HTMLElement | null>(null);
const photoInput = ref<HTMLInputElement | null>(null);

// Conflicts the person answered "keep mine" to, so a later look at the
// published state does not ask again about the same value.
const kept = new Set<string>();
const conflictKey = (c: Conflict) =>
  `${c.part}\u0000${c.part === "field" ? c.fieldId : ""}\u0000${JSON.stringify(c.theirs)}`;

async function loadPublished() {
  published.value = await publishedEdit(props.contactId);
}

/** Show `view`, with `mine` (the form, when editing already) as what the
 *  draft says now. */
function show(view: DraftView, mine: ContactEdit | null) {
  const draft = mine ?? view.mine ?? view.published;
  if (!draft) return;
  base.value = view.base ?? draft;
  const merged = merge3(base.value, draft, view.published ?? draft);
  form.value = merged.edit;
  conflicts.value = merged.conflicts.filter((c) => !kept.has(conflictKey(c)));
  seen.value = view.published_commit;
  if (!mine) lastSent = JSON.stringify(toStore(view.mine ?? draft));
  editing.value = true;
}

async function load() {
  error.value = null;
  try {
    await loadPublished();
    const view = await draftOf(props.contactId);
    if (view) show(view, null);
  } catch (e) {
    error.value = (e as Error).message;
  }
}

async function startEditing() {
  error.value = null;
  stale.value = false;
  try {
    status.value = "";
    show(await openDraft(props.contactId), null);
  } catch (e) {
    error.value = (e as Error).message;
  }
}

// ── Autosave ──────────────────────────────────────────────────────────

let timer: ReturnType<typeof setTimeout> | null = null;
let lastSent = "";
// Autosaves go out one at a time, in order, so a slow one never lands
// after the edit that followed it.
let chain: Promise<void> = Promise.resolve();

const nameMissing = computed(() => editing.value && !form.value.name.trim());

function flush(): Promise<void> {
  if (timer) clearTimeout(timer);
  timer = null;
  chain = chain.then(async () => {
    const edit = toStore(form.value);
    const body = JSON.stringify(edit);
    if (!editing.value || !edit.name.trim() || body === lastSent) {
      pending.value = false;
      return;
    }
    try {
      await autosaveDraft(props.contactId, edit);
      lastSent = body;
    } catch (e) {
      const msg = (e as Error).message;
      if (msg.includes("has no draft")) {
        editing.value = false;
        error.value = "This draft was saved or discarded elsewhere.";
        await loadPublished();
      } else {
        error.value = msg;
      }
    } finally {
      pending.value = !!timer;
    }
  });
  return chain;
}

watch(
  form,
  () => {
    if (!editing.value) return;
    if (timer) clearTimeout(timer);
    pending.value = true;
    timer = setTimeout(() => void flush(), AUTOSAVE_PAUSE_MS);
  },
  { deep: true },
);

const editStatus = computed(() => {
  if (pending.value) return "Keeping your draft…";
  if (published.value && hasChanges(form.value, published.value)) return "Draft kept, not saved";
  return "No changes";
});

// ── Save and discard ──────────────────────────────────────────────────

async function save() {
  busy.value = true;
  error.value = null;
  try {
    await flush();
    if (!editing.value) return;
    const saved = await saveDraft(props.contactId, seen.value);
    if (saved.outcome === "saved") {
      editing.value = false;
      stale.value = false;
      conflicts.value = [];
      kept.clear();
      status.value = "Saved";
      await loadPublished();
    } else {
      stale.value = true;
      show(saved.view, form.value);
    }
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    busy.value = false;
  }
}

async function discard() {
  busy.value = true;
  error.value = null;
  if (timer) clearTimeout(timer);
  timer = null;
  pending.value = false;
  try {
    await chain;
    await discardDraft(props.contactId);
    editing.value = false;
    stale.value = false;
    conflicts.value = [];
    kept.clear();
    status.value = "Discarded";
    await loadPublished();
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    busy.value = false;
  }
}

function useTheirs(c: Conflict) {
  form.value = takeTheirs(form.value, c);
  conflicts.value = conflicts.value.filter((x) => x !== c);
}

function keepMine(c: Conflict) {
  kept.add(conflictKey(c));
  conflicts.value = conflicts.value.filter((x) => x !== c);
}

const conflictOf = (part: "name" | "note") => conflicts.value.find((c) => c.part === part);
const fieldConflict = (id: string) =>
  conflicts.value.find((c) => c.part === "field" && c.fieldId === id);
/** Fields removed here that were changed elsewhere: shown below the list. */
const removedHere = computed(() =>
  conflicts.value.filter(
    (c) => c.part === "field" && !form.value.fields.some((f) => f.field_id === c.fieldId),
  ),
);

// ── Fields ────────────────────────────────────────────────────────────

const adding = ref("");
async function addField() {
  const kind = adding.value as FieldKind;
  adding.value = "";
  if (!kind) return;
  form.value.fields.push(newField(kind, crypto.randomUUID()));
  await nextTick();
  const values = fieldsBox.value?.querySelectorAll<HTMLInputElement>(".contact-edit-value");
  values?.[values.length - 1]?.focus();
}

function removeField(id: string) {
  form.value.fields = form.value.fields.filter((f) => f.field_id !== id);
}

const lines = computed(() => (published.value ? editLines(published.value) : []));

// ── Photo ─────────────────────────────────────────────────────────────

const photoBusy = ref(false);

async function onPhotoPicked() {
  const file = photoInput.value?.files?.[0];
  if (photoInput.value) photoInput.value.value = "";
  if (!file) return;
  photoBusy.value = true;
  error.value = null;
  try {
    await setContactPhoto(props.contactId, file);
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    photoBusy.value = false;
  }
}

/// A drop on the photo: an image file, or an image dragged from the app
/// itself (a source's photo on this card), fetched from this origin.
const dropping = ref(false);
async function onPhotoDrop(ev: DragEvent) {
  dropping.value = false;
  if (!ev.dataTransfer) return;
  const drop = photoDrop(ev.dataTransfer, location.origin, PHOTO_TYPES);
  if (!drop) return;
  error.value = null;
  if ("refused" in drop) {
    error.value = drop.refused;
    return;
  }
  photoBusy.value = true;
  try {
    let photo: Blob;
    if ("file" in drop) {
      photo = drop.file;
    } else {
      const r = await fetch(drop.url);
      if (!r.ok) throw new Error(`could not read that image (${r.status})`);
      photo = await r.blob();
      if (!PHOTO_TYPES.includes(photo.type)) {
        throw new Error("that image is not a PNG, JPEG, GIF or WebP, so it cannot be a photo");
      }
    }
    await setContactPhoto(props.contactId, photo);
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    photoBusy.value = false;
  }
}

function onPhotoDragOver(ev: DragEvent) {
  if (ev.dataTransfer) ev.dataTransfer.dropEffect = "copy";
  dropping.value = true;
}

async function dropPhoto() {
  photoBusy.value = true;
  error.value = null;
  try {
    await dropContactPhoto(props.contactId);
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    photoBusy.value = false;
  }
}

// ── Changes made elsewhere ────────────────────────────────────────────

/// A save from another window or an agent: read what is published again,
/// and while editing, show it against the draft. Not while an autosave
/// is waiting, since the form is then ahead of the draft it would read.
async function refresh() {
  try {
    await loadPublished();
    if (!editing.value || timer) return;
    await chain;
    const view = await draftOf(props.contactId);
    if (!view) {
      editing.value = false;
      error.value = "This draft was saved or discarded elsewhere.";
      return;
    }
    show(view, form.value);
  } catch (e) {
    error.value = (e as Error).message;
  }
}

const stopLive = subscribeLive({
  root: (e) => {
    if (movesPeople(e)) void refresh();
  },
  resync: () => void refresh(),
});

onMounted(load);
watch(() => props.contactId, load);
onUnmounted(() => {
  stopLive();
  if (timer) void flush();
});
</script>

<template>
  <div class="contact-edit">
    <div class="contact-edit-photo">
      <div
        class="contact-edit-drop"
        :class="{ 'contact-edit-dropping': dropping }"
        title="Drop an image here, or a source's photo from this card"
        @dragover.prevent.stop="onPhotoDragOver"
        @dragleave="dropping = false"
        @drop.prevent.stop="onPhotoDrop"
      >
        <img v-if="props.photoUrl" class="contact-edit-thumb" :src="props.photoUrl" alt="" />
        <span v-else>Drop a photo</span>
      </div>
      <input
        ref="photoInput"
        type="file"
        class="contact-edit-file"
        :accept="PHOTO_TYPES.join(',')"
        aria-label="Photo file"
        @change="onPhotoPicked"
      />
      <button
        type="button"
        class="person-button"
        :disabled="photoBusy"
        @click.stop="photoInput?.click()"
      >
        {{ props.photoUrl ? "Change photo…" : "Set photo…" }}
      </button>
      <button
        v-if="props.photoUrl"
        type="button"
        class="person-button"
        :disabled="photoBusy"
        @click.stop="dropPhoto"
      >
        Remove photo
      </button>
    </div>

    <template v-if="!editing">
      <dl v-if="lines.length" class="person-lines">
        <template v-for="(line, i) in lines" :key="i">
          <dt>{{ line.label }}</dt>
          <dd class="contact-edit-shown">{{ line.value }}</dd>
        </template>
      </dl>
      <div class="person-actions">
        <button type="button" class="person-button" @click.stop="startEditing">Edit</button>
        <span v-if="status" class="person-muted" role="status">{{ status }}</span>
      </div>
    </template>

    <form v-else class="contact-edit-form" @submit.prevent="save" @focusout="flush">
      <p v-if="stale" class="contact-edit-stale" role="alert">
        This contact was changed elsewhere since you started editing, so nothing was saved. The
        marked lines show what changed; Save again keeps what you have here.
      </p>

      <label class="contact-edit-row">
        <span class="contact-edit-label">Name</span>
        <input v-model="form.name" class="person-input contact-edit-name" aria-label="Name" />
      </label>
      <p v-if="nameMissing" class="person-error">A contact needs a name.</p>
      <p v-if="conflictOf('name')" class="contact-edit-conflict">
        {{ conflictText(conflictOf("name")!) }}
        <button type="button" class="person-button" @click.stop="useTheirs(conflictOf('name')!)">
          Use theirs
        </button>
        <button type="button" class="person-button" @click.stop="keepMine(conflictOf('name')!)">
          Keep mine
        </button>
      </p>

      <div ref="fieldsBox" class="contact-edit-fields">
        <div v-for="f in form.fields" :key="f.field_id" class="contact-edit-field">
          <div class="contact-edit-row">
            <select
              v-model="f.kind"
              class="person-input"
              :aria-label="`Kind of ${f.value || 'field'}`"
            >
              <option v-for="k in FIELD_KINDS" :key="k.kind" :value="k.kind">{{ k.label }}</option>
            </select>
            <input
              v-model="f.value"
              class="person-input contact-edit-value"
              :aria-label="kindLabel(f.kind)"
              :placeholder="kindLabel(f.kind)"
            />
            <input
              :value="f.label ?? ''"
              class="person-input contact-edit-tag"
              :aria-label="`Label for ${kindLabel(f.kind)}`"
              placeholder="label"
              @input="f.label = ($event.target as HTMLInputElement).value"
            />
            <button
              type="button"
              class="person-button contact-edit-remove"
              :aria-label="`Remove ${kindLabel(f.kind)} ${f.value}`.trim()"
              title="Remove"
              @click.stop="removeField(f.field_id)"
            >
              ×
            </button>
          </div>
          <p v-if="fieldConflict(f.field_id)" class="contact-edit-conflict">
            {{ conflictText(fieldConflict(f.field_id)!) }}
            <button
              type="button"
              class="person-button"
              @click.stop="useTheirs(fieldConflict(f.field_id)!)"
            >
              Use theirs
            </button>
            <button
              type="button"
              class="person-button"
              @click.stop="keepMine(fieldConflict(f.field_id)!)"
            >
              Keep mine
            </button>
          </p>
        </div>
        <p v-for="c in removedHere" :key="conflictKey(c)" class="contact-edit-conflict">
          {{ c.part === "field" && c.theirs ? removedText(c.theirs) : conflictText(c) }}
          <button type="button" class="person-button" @click.stop="useTheirs(c)">Use theirs</button>
          <button type="button" class="person-button" @click.stop="keepMine(c)">Keep mine</button>
        </p>
        <select v-model="adding" class="person-input" aria-label="Add a field" @change="addField">
          <option value="" disabled>Add a field…</option>
          <option v-for="k in FIELD_KINDS" :key="k.kind" :value="k.kind">{{ k.label }}</option>
        </select>
      </div>

      <label class="contact-edit-row contact-edit-note-row">
        <span class="contact-edit-label">Note</span>
        <textarea
          :value="form.note ?? ''"
          class="person-input contact-edit-note"
          aria-label="Note"
          rows="3"
          @input="form.note = ($event.target as HTMLTextAreaElement).value"
        ></textarea>
      </label>
      <p v-if="conflictOf('note')" class="contact-edit-conflict">
        {{ conflictText(conflictOf("note")!) }}
        <button type="button" class="person-button" @click.stop="useTheirs(conflictOf('note')!)">
          Use theirs
        </button>
        <button type="button" class="person-button" @click.stop="keepMine(conflictOf('note')!)">
          Keep mine
        </button>
      </p>

      <div class="person-actions">
        <button type="submit" class="person-button" :disabled="busy || nameMissing">Save</button>
        <button type="button" class="person-button" :disabled="busy" @click.stop="discard">
          Discard
        </button>
        <span class="person-muted" role="status">
          {{ editStatus }}
        </span>
      </div>
    </form>
    <p v-if="error" class="person-error">{{ error }}</p>
  </div>
</template>

<style>
.contact-edit-photo {
  display: flex;
  align-items: center;
  gap: 0.5rem;
  margin: 0.4rem 0;
}
.contact-edit-drop {
  display: flex;
  align-items: center;
  justify-content: center;
  width: 48px;
  height: 48px;
  border: 1px dashed var(--datalib-border);
  border-radius: 50%;
  overflow: hidden;
  font-size: var(--datalib-font-size-small);
  text-align: center;
  color: var(--datalib-muted);
}
.contact-edit-dropping {
  border: 2px solid var(--datalib-accent);
}
.contact-edit-thumb {
  width: 100%;
  height: 100%;
  object-fit: cover;
}
.contact-edit-file {
  display: none;
}
.contact-edit-shown {
  white-space: pre-wrap;
}
.contact-edit-form {
  display: flex;
  flex-direction: column;
  gap: 0.35rem;
  margin-top: 0.4rem;
}
.contact-edit-row {
  display: flex;
  align-items: center;
  gap: 0.4rem;
}
.contact-edit-label {
  width: 3.5rem;
  color: var(--datalib-muted);
}
.contact-edit-name {
  flex: 1;
  min-width: 0;
}
.contact-edit-fields {
  display: flex;
  flex-direction: column;
  gap: 0.3rem;
  align-items: flex-start;
}
.contact-edit-field {
  width: 100%;
}
.contact-edit-value {
  flex: 1;
  min-width: 0;
}
.contact-edit-tag {
  width: 6rem;
}
.contact-edit-remove {
  padding: 0.1rem 0.45rem;
}
.contact-edit-note-row {
  align-items: flex-start;
}
.contact-edit-note {
  flex: 1;
  min-width: 0;
  resize: vertical;
}
.contact-edit-stale,
.contact-edit-conflict {
  margin: 0;
  padding: 0.3rem 0.5rem;
  border: 1px solid var(--datalib-warn-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-warn-bg);
  color: var(--datalib-warn-fg);
}
.contact-edit-conflict .person-button {
  margin-left: 0.3rem;
}
</style>
