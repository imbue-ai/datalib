<script setup lang="ts">
import { ref, onMounted, onUnmounted } from "vue";
import { RouterView, RouterLink } from "vue-router";
import SyncProgressChrome from "@/components/SyncProgressChrome.vue";

const menuOpen = ref(false);

function toggleMenu() {
  menuOpen.value = !menuOpen.value;
}

function closeMenu() {
  menuOpen.value = false;
}

function onKey(e: KeyboardEvent) {
  if (e.key === "Escape" && menuOpen.value) {
    menuOpen.value = false;
  }
}

onMounted(() => {
  window.addEventListener("keydown", onKey);
});

onUnmounted(() => {
  window.removeEventListener("keydown", onKey);
});
</script>

<template>
  <main class="frankweiler-shell" data-feedback-root>
    <header class="fw-header">
      <button
        class="fw-hamburger"
        type="button"
        aria-label="Open navigation"
        :aria-expanded="menuOpen"
        @click="toggleMenu"
      >
        <span class="fw-hamburger-bar" />
        <span class="fw-hamburger-bar" />
        <span class="fw-hamburger-bar" />
      </button>
      <h1>Frankweiler</h1>
      <div class="fw-spacer" />
    </header>

    <SyncProgressChrome />
    <RouterView />

    <transition name="fw-drawer">
      <div
        v-if="menuOpen"
        class="fw-drawer-overlay"
        @click.self="closeMenu"
      >
        <nav
          class="fw-drawer"
          role="dialog"
          aria-label="Navigation"
          aria-modal="true"
        >
          <div class="fw-drawer-header">
            <strong>Navigate</strong>
            <button
              class="fw-drawer-close"
              type="button"
              aria-label="Close navigation"
              @click="closeMenu"
            >
              ×
            </button>
          </div>
          <ul class="fw-drawer-list">
            <li>
              <RouterLink to="/search" @click="closeMenu">Search</RouterLink>
            </li>
            <li>
              <RouterLink to="/sync" @click="closeMenu">Sync</RouterLink>
            </li>
            <li>
              <RouterLink to="/prefs" @click="closeMenu">Preferences</RouterLink>
            </li>
          </ul>
        </nav>
      </div>
    </transition>
  </main>
</template>

<style>
:root {
  color-scheme: light dark;
  --fw-bg: #ffffff;
  --fw-fg: #1a1a1a;
  --fw-muted: #6b6b6b;
  --fw-border: #d8d8d8;
  --fw-input-bg: #ffffff;
  --fw-code-bg: #f4f4f4;
  --fw-hover: #f0f0f0;
  --fw-accent: #2563eb;
  --fw-card-bg: #fafafa;
}

@media (prefers-color-scheme: dark) {
  :root {
    --fw-bg: #1a1b1e;
    --fw-fg: #e6e6e6;
    --fw-muted: #9aa0a6;
    --fw-border: #2f3136;
    --fw-input-bg: #232428;
    --fw-code-bg: #2a2b2f;
    --fw-hover: #2a2b2f;
    --fw-accent: #6ea8fe;
    --fw-card-bg: #232428;
  }
}

html,
body,
#app {
  background: var(--fw-bg);
  color: var(--fw-fg);
  margin: 0;
  min-height: 100vh;
}

body {
  font-family: system-ui, sans-serif;
}

a {
  color: var(--fw-accent);
}

.frankweiler-shell {
  padding: 1rem;
}

.fw-header {
  display: flex;
  align-items: center;
  gap: 0.6rem;
  margin: 0 0 0.75rem 0;
}
.fw-header h1 {
  margin: 0;
  font-size: 1.25rem;
}
.fw-spacer {
  flex: 1;
}
.fw-hamburger {
  display: inline-flex;
  flex-direction: column;
  justify-content: space-between;
  width: 28px;
  height: 22px;
  padding: 4px 3px;
  background: transparent;
  border: 1px solid var(--fw-border);
  border-radius: 4px;
  cursor: pointer;
}
.fw-hamburger:hover {
  background: var(--fw-hover);
}
.fw-hamburger-bar {
  display: block;
  height: 2px;
  width: 100%;
  background: var(--fw-fg);
  border-radius: 1px;
}
.fw-drawer-overlay {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.35);
  z-index: 2000;
}
.fw-drawer {
  position: fixed;
  top: 0;
  left: 0;
  bottom: 0;
  width: 260px;
  max-width: 80vw;
  background: var(--fw-bg);
  color: var(--fw-fg);
  border-right: 1px solid var(--fw-border);
  box-shadow: 2px 0 12px rgba(0, 0, 0, 0.2);
  padding: 0.75rem 0;
  display: flex;
  flex-direction: column;
}
.fw-drawer-header {
  display: flex;
  align-items: center;
  justify-content: space-between;
  padding: 0 0.85rem 0.5rem;
  border-bottom: 1px solid var(--fw-border);
}
.fw-drawer-close {
  background: transparent;
  border: none;
  color: var(--fw-fg);
  font-size: 1.4rem;
  cursor: pointer;
  line-height: 1;
  padding: 0 0.25rem;
}
.fw-drawer-list {
  list-style: none;
  padding: 0;
  margin: 0.25rem 0 0 0;
}
.fw-drawer-list li a {
  display: block;
  padding: 0.55rem 0.85rem;
  color: var(--fw-fg);
  text-decoration: none;
}
.fw-drawer-list li a:hover {
  background: var(--fw-hover);
}
.fw-drawer-list li a.router-link-active {
  color: var(--fw-accent);
  font-weight: 600;
}
.fw-drawer-enter-active,
.fw-drawer-leave-active {
  transition: opacity 120ms ease;
}
.fw-drawer-enter-from,
.fw-drawer-leave-to {
  opacity: 0;
}
</style>
