<script setup lang="ts">
// One agent's suggestion, put to the person: "These two are Will Riker,
// because …", the handles as chips (grey, since none is anyone's yet),
// and Yes and No. The contacts card's suggestions and a person's page
// both draw it. `suggestions.ts` has the words.
import { computed, onBeforeUnmount, onMounted, ref } from "vue";
import { chipCell, NOBODY, people } from "./contacts";
import { said, type Suggestion } from "./suggestions";

const props = defineProps<{ suggestion: Suggestion; busy: boolean }>();
const emit = defineEmits<{ yes: []; no: [] }>();

const words = computed(() => said(props.suggestion));
const chipped = computed(() => [
  ...props.suggestion.handles,
  ...(props.suggestion.handle ? [props.suggestion.handle] : []),
]);
const version = ref(0);

function drawChip(el: unknown, handle: string, _version: number) {
  if (!(el instanceof HTMLElement)) return;
  el.replaceChildren(chipCell(handle, "", people.get(handle) ?? NOBODY));
}

const stop = people.subscribe(() => version.value++);
onMounted(() => void people.ask(chipped.value).then(() => version.value++));
onBeforeUnmount(stop);
</script>

<template>
  <div class="suggestion" :data-subject="suggestion.subject">
    <p class="suggestion-said">
      {{ words.before }} <b v-if="words.name">{{ words.name }}</b>{{ words.after }}
    </p>
    <ul v-if="chipped.length" class="suggestion-handles">
      <li v-for="h in chipped" :key="h" :ref="(el) => drawChip(el, h, version)"></li>
    </ul>
    <p class="suggestion-why">
      <template v-if="words.because">Because {{ words.because }}. </template>
      Suggested by {{ words.by }}.
    </p>
    <div class="suggestion-answer">
      <button type="button" class="suggestion-button" :disabled="busy" @click="emit('yes')">
        Yes
      </button>
      <button type="button" class="suggestion-button" :disabled="busy" @click="emit('no')">
        No
      </button>
    </div>
  </div>
</template>

<style>
.suggestion {
  display: flex;
  flex-direction: column;
  gap: 0.3rem;
  padding: 0.5rem 0.65rem;
  border: 1px dashed var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-surface);
}
.suggestion-said,
.suggestion-why {
  margin: 0;
}
.suggestion-why {
  color: var(--datalib-muted);
}
.suggestion-handles {
  display: flex;
  flex-wrap: wrap;
  gap: 0.3rem;
  margin: 0;
  padding: 0;
  list-style: none;
}
.suggestion-answer {
  display: flex;
  gap: 0.4rem;
}
.suggestion-button {
  font: inherit;
  padding: 0.15rem 0.8rem;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-bg);
  color: var(--datalib-fg);
  cursor: pointer;
}
.suggestion-button:disabled {
  cursor: default;
  opacity: 0.6;
}
</style>
