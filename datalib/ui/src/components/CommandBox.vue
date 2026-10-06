<script setup lang="ts">
// The search box in the toolbar: type, press Enter, and a search card
// opens on what you typed. ⌘K (Ctrl+K elsewhere) puts the caret here
// from anywhere in the app.
import { onBeforeUnmount, onMounted, ref, useTemplateRef } from "vue";
import { searchFor } from "@/surface";

const query = ref("");
const input = useTemplateRef<HTMLInputElement>("input");

function submit() {
  const q = query.value.trim();
  if (!q) return;
  searchFor(q);
  query.value = "";
  input.value?.blur();
}

function onKey(ev: KeyboardEvent) {
  if ((ev.metaKey || ev.ctrlKey) && !ev.shiftKey && !ev.altKey && ev.key.toLowerCase() === "k") {
    ev.preventDefault();
    input.value?.focus();
    input.value?.select();
  }
}
onMounted(() => window.addEventListener("keydown", onKey));
onBeforeUnmount(() => window.removeEventListener("keydown", onKey));

const shortcut = /Mac|iPhone|iPad/.test(navigator.platform) ? "⌘K" : "Ctrl K";
</script>

<template>
  <form class="command-box" role="search" @submit.prevent="submit">
    <svg class="command-icon" viewBox="0 0 24 24" aria-hidden="true">
      <path
        fill="currentColor"
        d="M15.5 14h-.79l-.28-.27C15.41 12.59 16 11.11 16 9.5 16 5.91 13.09 3 9.5 3S3 5.91 3 9.5 5.91 16 9.5 16c1.61 0 3.09-.59 4.23-1.57l.27.28v.79l5 4.99L20.49 19l-4.99-5zm-6 0C7.01 14 5 11.99 5 9.5S7.01 5 9.5 5 14 7.01 14 9.5 11.99 14 9.5 14z"
      />
    </svg>
    <input
      ref="input"
      v-model="query"
      type="search"
      aria-label="Search your data"
      placeholder="Search your data"
      @keydown.esc="input?.blur()"
    />
    <kbd class="command-kbd">{{ shortcut }}</kbd>
  </form>
</template>

<style scoped>
.command-box {
  display: flex;
  align-items: center;
  gap: 6px;
  width: 100%;
  height: calc(var(--datalib-control-h) + 2px);
  box-sizing: border-box;
  padding: 0 8px;
  background: var(--datalib-input-bg);
  border: 1px solid var(--datalib-border);
  border-radius: var(--datalib-radius);
  color: var(--datalib-faint);
}
.command-box:focus-within {
  border-color: var(--datalib-accent);
  box-shadow: 0 0 0 3px color-mix(in srgb, var(--datalib-accent) 18%, transparent);
}
.command-icon {
  flex: 0 0 auto;
  width: var(--datalib-icon-size);
  height: var(--datalib-icon-size);
}
.command-box input {
  flex: 1 1 auto;
  min-width: 0;
  border: 0;
  outline: none;
  background: transparent;
  color: var(--datalib-fg);
  font: inherit;
}
.command-kbd {
  flex: 0 0 auto;
  font: inherit;
  font-size: var(--datalib-font-size-small);
  padding: 0 5px;
  border: 1px solid var(--datalib-border);
  border-radius: 4px;
}
</style>
