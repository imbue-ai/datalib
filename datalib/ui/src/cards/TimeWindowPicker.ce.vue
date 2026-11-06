<script setup lang="ts">
// The Time series card's window box: type a window (`6h`, `all`, two
// dates) and press Enter, or pick a preset from the list that opens on
// focus. What a text means is timeWindow.ts.
import { computed, ref, watch } from "vue";
import { timeView } from "@/config/timeView";
import { PRESETS, parseWindow, windowLabel, windowText, type TimeWindow } from "./timeWindow";

const props = defineProps<{ window: TimeWindow }>();
const emit = defineEmits<{ pick: [window: TimeWindow] }>();

const zone = computed(() => timeView.value.zone);
const shownText = computed(() => windowText(props.window, zone.value));

const text = ref(shownText.value);
const open = ref(false);
const active = ref(-1);
watch(shownText, (t) => {
  if (!open.value) text.value = t;
});

const presets = computed(() =>
  PRESETS.map((p) => {
    const w = parseWindow(p, zone.value)!;
    return { text: p, label: windowLabel(w), window: w };
  }),
);
const typed = computed(() => parseWindow(text.value, zone.value));
const error = computed(() =>
  open.value && text.value.trim() && !typed.value
    ? "Type a duration (15m, 6h, 7d, 1y), all, or two dates: 2369-04-14 07:00 to 2369-04-14 09:00"
    : null,
);

/// The box is as wide as its text, so `90d` takes little room beside the
/// card's other controls and two dates still fit.
const width = computed(() => `${Math.min(36, Math.max(5, text.value.length + 2))}ch`);

function choose(w: TimeWindow) {
  emit("pick", w);
  text.value = windowText(w, zone.value);
  open.value = false;
  active.value = -1;
}

function enter() {
  const p = presets.value[active.value];
  if (p) return choose(p.window);
  if (typed.value) choose(typed.value);
}

function cancel() {
  text.value = shownText.value;
  open.value = false;
  active.value = -1;
}

function move(by: number) {
  open.value = true;
  // -1 is the text box itself; the arrows cycle through it and the list.
  const slots = presets.value.length + 1;
  active.value = ((active.value + 1 + by + slots) % slots) - 1;
}
</script>

<template>
  <div class="tw">
    <input
      v-model="text"
      class="tw-input"
      :style="{ width }"
      type="text"
      role="combobox"
      aria-label="Time window"
      aria-autocomplete="list"
      :aria-expanded="open"
      aria-controls="tw-list"
      :aria-invalid="!!error"
      spellcheck="false"
      @focus="
        open = true;
        ($event.target as HTMLInputElement).select();
      "
      @blur="cancel"
      @input="
        open = true;
        active = -1;
      "
      @keydown.enter.prevent="enter"
      @keydown.escape.prevent="($event.target as HTMLInputElement).blur()"
      @keydown.down.prevent="move(1)"
      @keydown.up.prevent="move(-1)"
    />
    <ul v-if="open" id="tw-list" class="tw-list" role="listbox" aria-label="Time windows">
      <li v-if="error" class="tw-error" role="presentation">{{ error }}</li>
      <li
        v-for="(p, i) in presets"
        :key="p.text"
        role="option"
        class="tw-option"
        :class="{ 'tw-active': i === active }"
        :aria-selected="i === active"
        @mousedown.prevent="choose(p.window)"
      >
        <span>{{ p.label }}</span>
        <code class="tw-code">{{ p.text }}</code>
      </li>
      <li class="tw-hint" role="presentation">
        Or type one: <code>45m</code>, <code>2w</code>, or two dates with <code>to</code>
      </li>
    </ul>
  </div>
</template>

<style>
.tw {
  position: relative;
}
.tw-input {
  font: inherit;
  font-size: var(--datalib-font-size-small);
  max-width: 100%;
  box-sizing: content-box;
  padding: 0.15rem 0.45rem;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-input-bg);
  color: var(--datalib-fg);
}
.tw-input[aria-invalid="true"] {
  border-color: var(--datalib-error-fg);
}
.tw-list {
  position: absolute;
  right: 0;
  z-index: 5;
  width: max-content;
  min-width: 100%;
  max-width: 20rem;
  margin: 0.2rem 0 0;
  padding: 0.25rem 0;
  list-style: none;
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  background: var(--datalib-surface);
  box-shadow: 0 4px 12px rgb(0 0 0 / 0.15);
}
.tw-option {
  display: flex;
  justify-content: space-between;
  gap: 1rem;
  padding: 0.25rem 0.6rem;
  cursor: pointer;
}
.tw-option:hover,
.tw-active {
  background: var(--datalib-hover);
}
.tw-code {
  color: var(--datalib-muted);
}
.tw-hint {
  color: var(--datalib-muted);
  font-size: var(--datalib-font-size-small);
}
.tw-hint {
  padding: 0.3rem 0.6rem 0.15rem;
  border-top: 1px solid var(--datalib-border-soft);
  margin-top: 0.2rem;
}
.tw-error {
  color: var(--datalib-error-fg);
  font-size: var(--datalib-font-size-small);
  padding: 0.15rem 0.6rem 0.3rem;
  border-bottom: 1px solid var(--datalib-border-soft);
  margin-bottom: 0.2rem;
}
</style>
