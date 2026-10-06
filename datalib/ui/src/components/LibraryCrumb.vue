<script setup lang="ts">
// "Data Liberation ✊ › <library>" at the start of the top bar. In the
// desktop app the name goes up to the libraries screen and the
// library's name opens a menu of the other libraries to switch to. A
// browser shows the same two words with nothing behind them, since only
// the app can open another library.
import { computed, onBeforeUnmount, ref, watch } from "vue";
import {
  isDesktopApp,
  libraryMenu,
  showLibraries,
  switchLibrary,
  type LibraryMenu,
} from "@/desktop";
import { pushToast } from "@/toasts";

const props = defineProps<{
  // Absolute path of the open library's `config.toml`.
  configPath: string | null;
}>();

const desktop = isDesktopApp();
const rootPath = computed(() => props.configPath?.replace(/[/\\][^/\\]*$/, "") ?? null);
const name = computed(() => rootPath.value?.split(/[/\\]/).pop() || "");

const open = ref(false);
const menu = ref<LibraryMenu | null>(null);
const el = ref<HTMLElement | null>(null);

async function toggle() {
  open.value = !open.value;
  if (open.value) menu.value = await libraryMenu();
}

async function pick(path: string) {
  open.value = false;
  const refused = await switchLibrary(path);
  if (refused) pushToast(refused, "error");
}

function goUp() {
  open.value = false;
  void showLibraries();
}

// A press anywhere else closes the menu. `pointerdown` in the capture
// phase, because the top bar is a window-drag region whose handler
// takes the `mousedown` before it would reach the document.
function onDocDown(e: PointerEvent) {
  if (el.value && !el.value.contains(e.target as Node)) open.value = false;
}
function onKey(e: KeyboardEvent) {
  if (e.key === "Escape") open.value = false;
}
function onBlur() {
  open.value = false;
}
watch(open, (now) => {
  if (now) {
    document.addEventListener("pointerdown", onDocDown, true);
    window.addEventListener("keydown", onKey);
    window.addEventListener("blur", onBlur);
  } else {
    document.removeEventListener("pointerdown", onDocDown, true);
    window.removeEventListener("keydown", onKey);
    window.removeEventListener("blur", onBlur);
  }
});
onBeforeUnmount(() => (open.value = false));
</script>

<template>
  <div ref="el" class="crumb" data-tauri-drag-region>
    <button v-if="desktop" class="crumb-home" title="All libraries" @click="goUp()">
      Data Liberation ✊
    </button>
    <span v-else class="crumb-home" data-tauri-drag-region>Data Liberation ✊</span>
    <template v-if="name">
      <span class="crumb-sep" aria-hidden="true">›</span>
      <button
        v-if="desktop"
        class="crumb-lib"
        :class="{ 'is-open': open }"
        aria-haspopup="menu"
        :aria-expanded="open"
        @click="toggle"
      >
        <span class="crumb-name">{{ name }}</span>
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path fill="currentColor" d="M7.41 8.59 12 13.17l4.59-4.58L18 10l-6 6-6-6z" />
        </svg>
      </button>
      <span v-else class="crumb-lib" data-tauri-drag-region
        ><span class="crumb-name">{{ name }}</span></span
      >
    </template>
    <div v-if="open" class="crumb-menu" role="menu">
      <div class="crumb-item is-current" role="menuitem" aria-disabled="true">
        <svg viewBox="0 0 24 24" aria-hidden="true">
          <path fill="currentColor" d="M9 16.17 4.83 12l-1.42 1.41L9 19 21 7l-1.41-1.41z" />
        </svg>
        <strong>{{ name }}</strong>
      </div>
      <button
        v-for="o in menu?.others ?? []"
        :key="o.path"
        class="crumb-item"
        role="menuitem"
        :disabled="!o.found"
        :title="o.path"
        @click="pick(o.path)"
      >
        <span class="crumb-gap" />{{ o.name }}
        <small v-if="!o.found">not found</small>
      </button>
    </div>
  </div>
</template>

<style scoped>
.crumb {
  position: relative;
  display: flex;
  align-items: center;
  gap: 2px;
  min-width: 0;
  padding-left: 4px;
  font-size: var(--datalib-title-size);
  font-weight: 600;
  white-space: nowrap;
}
.crumb-home,
.crumb-lib {
  display: inline-flex;
  align-items: center;
  gap: 4px;
  height: var(--datalib-control-h);
  padding: 0 6px;
  border: none;
  border-radius: var(--datalib-radius);
  background: transparent;
  color: var(--datalib-fg);
  font: inherit;
}
.crumb-home {
  color: var(--datalib-muted);
}
button.crumb-home,
button.crumb-lib {
  cursor: pointer;
}
button.crumb-home:hover,
button.crumb-lib:hover,
.crumb-lib.is-open {
  background: var(--datalib-hover);
}
.crumb-lib {
  min-width: 0;
  max-width: 200px;
}
.crumb-name {
  min-width: 0;
  overflow: hidden;
  text-overflow: ellipsis;
}
.crumb-lib svg {
  width: 14px;
  height: 14px;
  flex: 0 0 auto;
}
.crumb-sep {
  color: var(--datalib-faint);
}
.crumb-menu {
  position: absolute;
  top: calc(100% + 4px);
  left: 0;
  z-index: 1000;
  min-width: 260px;
  padding: 6px;
  display: flex;
  flex-direction: column;
  background: var(--datalib-bg);
  border: 1px solid var(--datalib-border);
  border-radius: calc(var(--datalib-radius) + 2px);
  box-shadow: 0 12px 32px rgba(0, 0, 0, 0.16);
  font-weight: 400;
}
.crumb-item {
  display: flex;
  align-items: center;
  gap: 8px;
  padding: 6px 8px;
  border: none;
  border-radius: var(--datalib-radius);
  background: transparent;
  color: var(--datalib-fg);
  font: inherit;
  text-align: left;
  cursor: pointer;
}
.crumb-item:hover:not(:disabled):not(.is-current) {
  background: var(--datalib-hover);
}
.crumb-item:disabled {
  color: var(--datalib-faint);
  cursor: default;
}
.crumb-item.is-current {
  cursor: default;
}
.crumb-item svg {
  width: 14px;
  height: 14px;
  color: var(--datalib-accent);
}
.crumb-item small {
  margin-left: auto;
  color: var(--datalib-warn-fg);
}
.crumb-gap {
  width: 14px;
  flex: 0 0 auto;
}
</style>
