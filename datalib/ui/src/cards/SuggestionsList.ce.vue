<script setup lang="ts">
// The contacts card's suggestions: everything agents suggested that waits
// for the person, least sure first, each with Yes and No, and Yes to all
// for each run of an agent. Until Yes none of it is a contact, a link or
// a label anything reads (docs/dev/contacts.md § "The contacts list").
import { computed, ref } from "vue";
import { pushToast } from "@/toasts";
import { chipElAt, chipOf, runChipAction } from "./chipActions";
import { confirmReview, rejectReview } from "./contacts";
import SuggestionItem from "./SuggestionItem.ce.vue";
import { runsOf, type Run, type Suggestion } from "./suggestions";
import type { CardCtx } from "./types";

const props = defineProps<{ ctx: CardCtx; suggestions: Suggestion[] }>();
const emit = defineEmits<{ changed: []; back: [] }>();

const busy = ref(false);
const error = ref<string | null>(null);
const runs = computed(() => runsOf(props.suggestions).filter((r) => r.subjects.length > 1));

async function answer(subjects: string[], yes: boolean) {
  busy.value = true;
  error.value = null;
  try {
    const n = await (yes ? confirmReview(subjects) : rejectReview(subjects));
    if (subjects.length > 1) pushToast(`Said yes to ${n} suggestions`, "info");
    emit("changed");
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    busy.value = false;
  }
}

function runLabel(r: Run): string {
  return `Yes to all ${r.subjects.length} from ${r.agent}${r.run ? ` (${r.run})` : ""}`;
}

function onDblClick(ev: MouseEvent) {
  const el = chipElAt(ev.target);
  const chip = el ? chipOf(el) : null;
  if (!chip) return;
  ev.preventDefault();
  void runChipAction("open", chip, { openCard: (s) => props.ctx.host.openCards(s) });
}
</script>

<template>
  <div class="suggestions" @dblclick="onDblClick">
    <div class="contacts-bar">
      <button type="button" class="contacts-button" @click="emit('back')">← Contacts</button>
      <span class="contacts-count">
        {{ suggestions.length }} {{ suggestions.length === 1 ? "suggestion" : "suggestions" }}
      </span>
      <button
        v-for="r in runs"
        :key="`${r.agent}\u0000${r.run}`"
        type="button"
        class="contacts-button"
        :disabled="busy"
        @click="answer(r.subjects, true)"
      >
        {{ runLabel(r) }}
      </button>
    </div>
    <div v-if="error" class="contacts-error">{{ error }}</div>
    <p v-if="suggestions.length === 0" class="contacts-empty">Nothing waits for you.</p>
    <ul class="suggestions-list">
      <li v-for="s in suggestions" :key="s.subject">
        <SuggestionItem
          :suggestion="s"
          :busy="busy"
          @yes="answer([s.subject], true)"
          @no="answer([s.subject], false)"
        />
      </li>
    </ul>
  </div>
</template>

<style>
.suggestions {
  display: flex;
  flex-direction: column;
  min-height: 0;
  flex: 1;
}
.suggestions-list {
  display: flex;
  flex-direction: column;
  gap: 0.5rem;
  margin: 0;
  padding: 0.6rem 0.75rem;
  overflow: auto;
  list-style: none;
}
</style>
