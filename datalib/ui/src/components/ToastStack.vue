<script setup lang="ts">
// Bottom-right toast tray. Mounted once in App.vue; reads the module-
// level `toasts` array from `@/toasts`. No props — every consumer pushes
// via `pushToast(...)`.
//
// A toast is drawn over everything and owns none of it: only its two
// buttons take the pointer, so a click on whatever it happens to cover
// — a dialog's submit button, say — still lands there. That is also
// why there is a Copy button: the text cannot be selected.
import { ref } from "vue";
import { toasts, dismissToast } from "@/toasts";
import { copyToClipboard } from "@/clipboard";

const copiedId = ref<number | null>(null);
async function copy(id: number, message: string) {
  if (await copyToClipboard(message)) {
    copiedId.value = id;
    window.setTimeout(() => {
      if (copiedId.value === id) copiedId.value = null;
    }, 1500);
  }
}
</script>

<template>
  <div class="datalib-toast-stack" role="region" aria-label="Notifications">
    <transition-group name="datalib-toast">
      <div
        v-for="t in toasts"
        :key="t.id"
        :class="['datalib-toast', `datalib-toast--${t.level}`]"
        role="status"
        :aria-live="t.level === 'error' ? 'assertive' : 'polite'"
      >
        <span class="datalib-toast__msg">{{ t.message }}</span>
        <button
          class="datalib-toast__copy"
          type="button"
          :aria-label="copiedId === t.id ? 'Copied' : 'Copy'"
          @click="copy(t.id, t.message)"
        >
          {{ copiedId === t.id ? "Copied" : "Copy" }}
        </button>
        <button
          class="datalib-toast__close"
          type="button"
          aria-label="Dismiss"
          @click="dismissToast(t.id)"
        >
          ×
        </button>
      </div>
    </transition-group>
  </div>
</template>

<style scoped>
.datalib-toast-stack {
  position: fixed;
  right: 0.75rem;
  bottom: 0.75rem;
  z-index: 3000;
  display: flex;
  flex-direction: column;
  gap: 0.4rem;
  max-width: min(440px, calc(100vw - 1.5rem));
  pointer-events: none;
}
.datalib-toast {
  pointer-events: none;
  display: flex;
  align-items: flex-start;
  gap: 0.5rem;
  padding: 0.55rem 0.65rem 0.55rem 0.75rem;
  border-radius: 6px;
  border: 1px solid var(--datalib-border);
  background: var(--datalib-card-bg);
  color: var(--datalib-fg);
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.18);
  font-size: 0.85rem;
  line-height: 1.35;
}
.datalib-toast--error {
  border-color: #c0392b;
  background: color-mix(in srgb, #c0392b 12%, var(--datalib-card-bg));
}
.datalib-toast--warn {
  border-color: #b7791f;
  background: color-mix(in srgb, #b7791f 12%, var(--datalib-card-bg));
}
.datalib-toast--info {
  border-color: var(--datalib-accent);
  background: color-mix(in srgb, var(--datalib-accent) 10%, var(--datalib-card-bg));
}
.datalib-toast__msg {
  flex: 1;
  word-break: break-word;
  white-space: pre-wrap;
}
.datalib-toast__copy,
.datalib-toast__close {
  pointer-events: auto;
  background: transparent;
  border: none;
  color: inherit;
  line-height: 1;
  cursor: pointer;
  padding: 0 0.2rem;
  opacity: 0.7;
}
.datalib-toast__copy {
  font-size: 0.75rem;
  align-self: center;
  white-space: nowrap;
}
.datalib-toast__close {
  font-size: 1.1rem;
}
.datalib-toast__copy:hover,
.datalib-toast__close:hover {
  opacity: 1;
}
.datalib-toast-enter-active,
.datalib-toast-leave-active {
  transition: opacity 150ms ease, transform 150ms ease;
}
.datalib-toast-enter-from,
.datalib-toast-leave-to {
  opacity: 0;
  transform: translateY(6px);
}
</style>
