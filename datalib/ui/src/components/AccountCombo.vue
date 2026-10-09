<script setup lang="ts">
// One box for a latchkey account name: type any name, or open the list of
// names latchkey holds and pick one. Typing stays possible because latchkey
// can hold a name this server could not list.
import { computed, ref, useId } from "vue";

export type AccountOption = { value: string; note?: string };

const props = defineProps<{
  modelValue: string;
  /// `null` while the list is still being fetched.
  options: AccountOption[] | null;
  label: string;
  placeholder?: string;
}>();
const emit = defineEmits<{ "update:modelValue": [value: string] }>();

const open = ref(false);
const active = ref(-1);
const listId = useId();
const shown = computed(() => props.options ?? []);

function toggle() {
  open.value = !open.value;
  active.value = shown.value.findIndex((o) => o.value === props.modelValue);
}

function pick(value: string) {
  emit("update:modelValue", value);
  open.value = false;
}

function onKeydown(ev: KeyboardEvent) {
  if (ev.key === "ArrowDown" || ev.key === "ArrowUp") {
    ev.preventDefault();
    if (!open.value) return toggle();
    const step = ev.key === "ArrowDown" ? 1 : -1;
    active.value = Math.min(Math.max(active.value + step, 0), shown.value.length - 1);
  } else if (ev.key === "Enter" && open.value && shown.value[active.value]) {
    ev.preventDefault();
    pick(shown.value[active.value]!.value);
  } else if (ev.key === "Escape" && open.value) {
    // Closes the list, not the dialog around it.
    ev.preventDefault();
    ev.stopPropagation();
    open.value = false;
  }
}

function onFocusout(ev: FocusEvent) {
  const root = ev.currentTarget as HTMLElement;
  if (!root.contains(ev.relatedTarget as Node | null)) open.value = false;
}
</script>

<template>
  <div class="acct" @focusout="onFocusout">
    <input
      class="wiz-input acct-input"
      role="combobox"
      :aria-label="label"
      :aria-expanded="open"
      :aria-controls="listId"
      aria-autocomplete="none"
      :value="modelValue"
      :placeholder="placeholder"
      autocomplete="off"
      spellcheck="false"
      @input="emit('update:modelValue', ($event.target as HTMLInputElement).value)"
      @keydown="onKeydown"
      @click="open || toggle()"
    />
    <button
      type="button"
      class="acct-toggle"
      :aria-label="`Show the stored ${label} names`"
      @mousedown.prevent
      @click="toggle"
    >
      ▾
    </button>
    <ul v-if="open" :id="listId" class="acct-list" role="listbox" :aria-label="label">
      <li v-if="options === null" class="acct-empty">Looking…</li>
      <li v-else-if="shown.length === 0" class="acct-empty">none stored yet</li>
      <li
        v-for="(o, i) in shown"
        :key="o.value || '(no name)'"
        role="option"
        class="acct-option"
        :class="{ 'acct-active': i === active }"
        :aria-selected="o.value === modelValue"
        @mousedown.prevent="pick(o.value)"
      >
        <span>{{ o.value || "(no name)" }}</span>
        <small v-if="o.note" class="acct-note">{{ o.note }}</small>
      </li>
    </ul>
  </div>
</template>

<style scoped>
.acct {
  position: relative;
}
/* The wizard's `.wiz-input` is scoped to it and does not reach in here. */
.acct-input {
  width: 100%;
  box-sizing: border-box;
  padding: 8px 34px 8px 10px;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-input-bg);
  color: var(--datalib-fg);
  font: inherit;
}
.acct-toggle {
  position: absolute;
  top: 0;
  right: 0;
  bottom: 0;
  width: 32px;
  border: 0;
  background: none;
  color: var(--datalib-muted);
  cursor: pointer;
  font: inherit;
}
.acct-toggle:hover {
  color: var(--datalib-fg);
}
.acct-list {
  position: absolute;
  z-index: 10;
  left: 0;
  right: 0;
  top: calc(100% + 4px);
  margin: 0;
  padding: 4px;
  list-style: none;
  max-height: 220px;
  overflow-y: auto;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-surface);
  box-shadow: 0 10px 28px rgba(0, 0, 0, 0.18);
}
.acct-option,
.acct-empty {
  display: flex;
  justify-content: space-between;
  gap: 12px;
  padding: 6px 8px;
  border-radius: var(--datalib-radius);
}
.acct-option {
  cursor: pointer;
}
.acct-option:hover,
.acct-active {
  background: var(--datalib-hover);
}
.acct-empty,
.acct-note {
  color: var(--datalib-muted);
}
</style>
