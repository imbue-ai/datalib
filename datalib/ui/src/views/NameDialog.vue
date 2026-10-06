<script setup lang="ts">
// Asks for a name: a tab's, a container's, a composite's. `check` says
// what is wrong with a name, if anything, and the dialog stays open
// saying so until the name is fixed or the dialog is cancelled.
import { computed, ref } from "vue";

const props = defineProps<{
  title: string;
  initial: string;
  check?: (name: string) => string | null;
}>();
const emit = defineEmits<{ done: [name: string | null] }>();

const value = ref(props.initial);
const tried = ref(false);
const problem = computed(() => {
  const name = value.value.trim();
  if (name === "") return "A name cannot be empty.";
  return props.check?.(name) ?? null;
});

function submit() {
  tried.value = true;
  if (problem.value === null) emit("done", value.value.trim());
}

const vFocusSelect = {
  mounted(el: HTMLInputElement) {
    el.focus();
    el.select();
  },
};
</script>

<template>
  <div class="nd-backdrop" @click.self="emit('done', null)">
    <form class="nd" @submit.prevent="submit">
      <label class="nd-title" for="nd-input">{{ title }}</label>
      <input
        id="nd-input"
        v-model="value"
        v-focus-select
        :aria-invalid="tried && problem !== null"
        aria-describedby="nd-problem"
        @keydown.esc.prevent="emit('done', null)"
      />
      <p v-if="tried && problem" id="nd-problem" class="nd-problem">{{ problem }}</p>
      <div class="nd-buttons">
        <button type="button" @click="emit('done', null)">Cancel</button>
        <button type="submit" class="nd-ok">OK</button>
      </div>
    </form>
  </div>
</template>

<style scoped>
.nd-backdrop {
  position: fixed;
  inset: 0;
  z-index: 30;
  background: rgba(0, 0, 0, 0.3);
  display: flex;
  align-items: center;
  justify-content: center;
}
.nd {
  width: min(360px, 90vw);
  display: flex;
  flex-direction: column;
  gap: 10px;
  padding: 16px;
  background: var(--datalib-bg);
  color: var(--datalib-fg);
  border: 1px solid var(--datalib-border);
  border-radius: calc(var(--datalib-radius) + 4px);
  box-shadow: 0 18px 48px rgba(0, 0, 0, 0.22);
}
.nd-title {
  font-weight: 600;
}
.nd input {
  font: inherit;
  padding: 4px 6px;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-input-bg);
  color: var(--datalib-fg);
}
.nd input[aria-invalid="true"] {
  border-color: var(--datalib-error-fg);
}
.nd-problem {
  margin: 0;
  color: var(--datalib-error-fg);
  font-size: var(--datalib-font-size-small);
}
.nd-buttons {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
.nd-buttons button {
  font: inherit;
  padding: 3px 12px;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-surface);
  color: var(--datalib-fg);
  cursor: pointer;
}
.nd-buttons .nd-ok {
  background: var(--datalib-accent);
  border-color: var(--datalib-accent);
  color: var(--datalib-on-accent);
}
</style>
