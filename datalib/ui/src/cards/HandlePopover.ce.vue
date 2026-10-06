<script setup lang="ts">
// What a click on a handle chip opens: for an unlinked handle, find a
// contact to link it to or make a new one; for a linked one, who it
// belongs to, and unlink it or mark it as no longer working. Every
// action is one commit in the contacts app's store; `changed` tells the
// document to redraw its chips.
import { computed, onMounted, onUnmounted, ref, watch } from "vue";

import { iconUrl } from "@/config/icons";
import {
  createContact,
  handleIcon,
  handleValue,
  linkHandle,
  searchContacts,
  setStoppedWorking,
  suggestedName,
  todayPartialDate,
  unlinkHandle,
  type ContactSummary,
  nameOf,
  stoppedBy as stoppedByOf,
  type DatalibContact,
} from "./contacts";

const props = defineProps<{
  handle: string;
  shownAs: string;
  resolved: DatalibContact | null;
  x: number;
  y: number;
}>();
const emit = defineEmits<{ close: []; changed: [] }>();

const query = ref(suggestedName(props.shownAs, props.handle));
const matches = ref<ContactSummary[]>([]);
const error = ref<string | null>(null);
const busy = ref(false);
const stopped = computed(() => stoppedByOf(props.resolved, props.handle));
const stoppedBy = ref(stopped.value ?? "");
const input = ref<HTMLInputElement | null>(null);
const box = ref<HTMLElement | null>(null);

const mark = computed(() => iconUrl(handleIcon(props.handle)));
const style = computed(() => ({
  left: `${Math.min(props.x, window.innerWidth - 340)}px`,
  top: `${Math.min(props.y + 8, window.innerHeight - 260)}px`,
}));

let searchSeq = 0;
async function runSearch() {
  if (props.resolved) return;
  const seq = ++searchSeq;
  try {
    const found = await searchContacts(query.value.trim());
    if (seq === searchSeq) matches.value = found;
  } catch (e) {
    error.value = (e as Error).message;
  }
}
watch(query, runSearch);

async function act(f: () => Promise<void>) {
  busy.value = true;
  error.value = null;
  try {
    await f();
    emit("changed");
    emit("close");
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    busy.value = false;
  }
}

const linkTo = (c: ContactSummary) => act(() => linkHandle(props.handle, c.contact_id));
const createNew = () =>
  act(async () => {
    await createContact(query.value.trim(), [props.handle]);
  });
const unlink = () => act(() => unlinkHandle(props.handle));
const markStopped = () =>
  act(() => setStoppedWorking(props.handle, stoppedBy.value.trim() || todayPartialDate()));
const markWorks = () => act(() => setStoppedWorking(props.handle, null));

function onKey(ev: KeyboardEvent) {
  if (ev.key === "Escape") emit("close");
}
function onOutside(ev: MouseEvent) {
  if (box.value && !ev.composedPath().includes(box.value)) emit("close");
}
onMounted(() => {
  window.addEventListener("keydown", onKey);
  // On the next tick, or the click that opened this would close it.
  setTimeout(() => window.addEventListener("mousedown", onOutside), 0);
  input.value?.focus();
  void runSearch();
});
onUnmounted(() => {
  window.removeEventListener("keydown", onKey);
  window.removeEventListener("mousedown", onOutside);
});
</script>

<template>
  <div ref="box" class="handle-popover" :style="style" role="dialog" aria-label="Contact">
    <div class="hp-handle">
      <img v-if="mark" :src="mark" alt="" class="hp-mark" />
      <span class="hp-value">{{ handleValue(handle) }}</span>
    </div>
    <template v-if="resolved">
      <div class="hp-contact">{{ nameOf(resolved) }}</div>
      <div class="hp-row">
        <button type="button" :disabled="busy" @click="unlink">Unlink</button>
      </div>
      <div class="hp-row">
        <template v-if="stopped">
          <span class="hp-note">Stopped working by {{ stopped }}</span>
          <button type="button" :disabled="busy" @click="markWorks">It works</button>
        </template>
        <template v-else>
          <input
            v-model="stoppedBy"
            class="hp-date"
            :placeholder="todayPartialDate()"
            aria-label="Stopped working by (2019, 2019-06 or 2019-06-14)"
          />
          <button type="button" :disabled="busy" @click="markStopped">No longer works</button>
        </template>
      </div>
    </template>
    <template v-else>
      <input
        ref="input"
        v-model="query"
        class="hp-query"
        placeholder="Contact name"
        aria-label="Contact name"
        @keydown.enter.prevent="
          matches.length === 1 ? linkTo(matches[0]) : query.trim() && createNew()
        "
      />
      <ul v-if="matches.length" class="hp-matches">
        <li v-for="c in matches" :key="c.contact_id">
          <button type="button" :disabled="busy" @click="linkTo(c)">
            Link to <b>{{ c.name }}</b>
          </button>
        </li>
      </ul>
      <div class="hp-row">
        <button type="button" :disabled="busy || !query.trim()" @click="createNew">
          New contact “{{ query.trim() || "…" }}”
        </button>
      </div>
    </template>
    <div v-if="error" class="hp-error">{{ error }}</div>
  </div>
</template>

<style>
.handle-popover {
  position: fixed;
  z-index: 50;
  width: 320px;
  padding: 10px 12px;
  background: var(--datalib-input-bg, #fff);
  color: inherit;
  border: 1px solid var(--datalib-border, #d8d8d8);
  border-radius: 8px;
  box-shadow: 0 6px 24px rgb(0 0 0 / 18%);
  font-size: 13px;
}
.handle-popover .hp-handle {
  display: flex;
  align-items: center;
  gap: 6px;
  color: var(--datalib-muted, #94a3b8);
  margin-bottom: 8px;
  overflow-wrap: anywhere;
}
.handle-popover .hp-mark {
  width: 14px;
  height: 14px;
}
.handle-popover .hp-contact {
  font-weight: 600;
  margin-bottom: 8px;
}
.handle-popover .hp-row {
  display: flex;
  align-items: center;
  gap: 6px;
  margin-top: 6px;
}
.handle-popover .hp-query,
.handle-popover .hp-date {
  width: 100%;
  box-sizing: border-box;
  padding: 4px 6px;
}
.handle-popover .hp-date {
  width: 110px;
}
.handle-popover .hp-matches {
  list-style: none;
  margin: 6px 0 0;
  padding: 0;
  max-height: 160px;
  overflow-y: auto;
}
.handle-popover .hp-matches button {
  width: 100%;
  text-align: left;
}
.handle-popover .hp-note {
  color: var(--datalib-muted, #94a3b8);
}
.handle-popover .hp-error {
  margin-top: 8px;
  color: var(--datalib-error-fg, #b00020);
}
</style>
