<script setup lang="ts">
// Bottom-right toast tray. Mounted once in App.vue; reads the module-
// level `toasts` array from `@/toasts`. No props — every consumer pushes
// via `pushToast(...)`.
import { toasts, dismissToast } from "@/toasts";
</script>

<template>
  <div class="fw-toast-stack" role="region" aria-label="Notifications">
    <transition-group name="fw-toast">
      <div
        v-for="t in toasts"
        :key="t.id"
        :class="['fw-toast', `fw-toast--${t.level}`]"
        role="status"
        :aria-live="t.level === 'error' ? 'assertive' : 'polite'"
      >
        <span class="fw-toast__msg">{{ t.message }}</span>
        <button
          class="fw-toast__close"
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
.fw-toast-stack {
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
.fw-toast {
  pointer-events: auto;
  display: flex;
  align-items: flex-start;
  gap: 0.5rem;
  padding: 0.55rem 0.65rem 0.55rem 0.75rem;
  border-radius: 6px;
  border: 1px solid var(--fw-border);
  background: var(--fw-card-bg);
  color: var(--fw-fg);
  box-shadow: 0 4px 16px rgba(0, 0, 0, 0.18);
  font-size: 0.85rem;
  line-height: 1.35;
}
.fw-toast--error {
  border-color: #c0392b;
  background: color-mix(in srgb, #c0392b 12%, var(--fw-card-bg));
}
.fw-toast--warn {
  border-color: #b7791f;
  background: color-mix(in srgb, #b7791f 12%, var(--fw-card-bg));
}
.fw-toast--info {
  border-color: var(--fw-accent);
  background: color-mix(in srgb, var(--fw-accent) 10%, var(--fw-card-bg));
}
.fw-toast__msg {
  flex: 1;
  word-break: break-word;
  white-space: pre-wrap;
}
.fw-toast__close {
  background: transparent;
  border: none;
  color: inherit;
  font-size: 1.1rem;
  line-height: 1;
  cursor: pointer;
  padding: 0 0.2rem;
  opacity: 0.7;
}
.fw-toast__close:hover {
  opacity: 1;
}
.fw-toast-enter-active,
.fw-toast-leave-active {
  transition: opacity 150ms ease, transform 150ms ease;
}
.fw-toast-enter-from,
.fw-toast-leave-to {
  opacity: 0;
  transform: translateY(6px);
}
</style>
