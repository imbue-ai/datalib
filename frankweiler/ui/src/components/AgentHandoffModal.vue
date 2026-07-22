<!--
  AgentHandoffModal — the step-by-step instructions shown when existing
  work is handed to a coding agent (handoff.ts): "modify" (the 🤖
  button on a component-backed card) and "config" (the 🤖 button on the
  Manage tab's config editor). The create flow has no dialog — a
  freshly minted component renders its instructions in the card body
  (cards/libs/agentSeedView.ts). Driven entirely by the module-level
  `pendingHandoff` store; mounted once in App.

  The copy button IS step 1 — the wayfinder is deliberately not copied
  on open, so the clipboard only changes when the user asks. Both kinds
  carry the "skip these steps next time" checkbox; once checked, that
  surface's 🤖 button copies the wayfinder directly (see handoff.ts).
-->
<script setup lang="ts">
import { computed, nextTick, ref, watch, type Ref } from "vue";
import {
  copyWayfinder,
  dismissHandoff,
  pendingHandoff,
  skipConfigInstructions,
  skipModifyInstructions,
} from "@/handoff";

const copied = ref(false);
const copyBtn = ref<HTMLButtonElement | null>(null);

// The per-kind text: dialog title, what to tell the agent after the
// prompt, where the result shows up, and which persisted skip flag the
// checkbox drives.
const KIND_TEXT: Record<
  string,
  { title: string; ask: string; watch: string; skipFlag: Ref<boolean> }
> = {
  modify: {
    title: "Modify this card with a coding agent",
    ask: "what you want changed",
    watch: "Keep the card open — it re-renders every time the agent saves the component.",
    skipFlag: skipModifyInstructions,
  },
  config: {
    title: "Edit the config with a coding agent",
    ask: "what you want changed",
    watch: "Keep the editor open — it reloads every time the agent saves the config.",
    skipFlag: skipConfigInstructions,
  },
};

const text = computed(() =>
  pendingHandoff.value ? KIND_TEXT[pendingHandoff.value.kind] : null,
);

// Reset per hand-off, and focus the copy button (it's step 1, and a
// focused descendant is what lets the overlay's keydown catch Escape).
watch(pendingHandoff, (now) => {
  copied.value = false;
  if (now) void nextTick(() => copyBtn.value?.focus());
});

async function onCopy() {
  if (!pendingHandoff.value) return;
  copied.value = await copyWayfinder(pendingHandoff.value.wayfinder);
}

function onKeydown(ev: KeyboardEvent) {
  if (ev.key === "Escape") {
    ev.preventDefault();
    dismissHandoff();
  }
}
</script>

<template>
  <Teleport to="body">
    <div
      v-if="pendingHandoff"
      class="ah-overlay"
      @click.self="dismissHandoff()"
      @keydown="onKeydown"
    >
      <div
        v-if="text"
        class="ah-modal"
        role="dialog"
        aria-modal="true"
        aria-label="hand this off to a coding agent"
      >
        <header class="ah-header">
          <div class="ah-title">{{ text.title }}</div>
          <div class="ah-component">
            {{ pendingHandoff.kind === "config" ? "config file" : "component" }}
            <code>{{ pendingHandoff.subject }}</code>
          </div>
        </header>
        <ol class="ah-steps">
          <li>
            Copy the agent prompt:
            <button ref="copyBtn" class="ah-copy" @click="onCopy">
              {{ copied ? "copied ✓" : "copy prompt" }}
            </button>
          </li>
          <li>Paste it into a coding agent, followed by {{ text.ask }}.</li>
          <li>{{ text.watch }}</li>
        </ol>
        <label class="ah-skip">
          <input v-model="text.skipFlag.value" type="checkbox" />
          Skip these steps next time — 🤖 will just copy the prompt
        </label>
        <footer class="ah-footer">
          <button class="ah-done" @click="dismissHandoff()">Done</button>
        </footer>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
.ah-overlay {
  position: fixed;
  inset: 0;
  z-index: 2000;
  background: rgba(0, 0, 0, 0.35);
  display: flex;
  align-items: center;
  justify-content: center;
}
.ah-modal {
  background: var(--fw-input-bg, #fff);
  color: var(--fw-fg, #000);
  border: 1px solid var(--fw-border, #ccc);
  border-radius: 6px;
  box-shadow: 0 8px 32px rgba(0, 0, 0, 0.25);
  width: min(480px, 90vw);
  padding: 16px 18px 14px;
  display: flex;
  flex-direction: column;
  gap: 10px;
  font-size: 0.9rem;
}
.ah-header {
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.ah-title {
  font-size: 1.05rem;
  font-weight: 600;
}
.ah-component {
  font-size: 0.8rem;
  color: var(--fw-muted, #888);
}
.ah-component code {
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  background: var(--fw-code-bg, #f5f5f5);
  padding: 0 0.25rem;
  border-radius: 2px;
}
.ah-steps {
  margin: 0;
  padding-left: 1.4em;
  display: flex;
  flex-direction: column;
  gap: 8px;
  line-height: 1.45;
}
.ah-copy {
  margin-left: 0.3em;
  padding: 2px 10px;
  border: 1px solid var(--fw-accent, #4060c0);
  border-radius: 4px;
  background: var(--fw-accent, #4060c0);
  color: var(--fw-bg, #fff);
  cursor: pointer;
  font-size: 0.85rem;
}
.ah-skip {
  display: flex;
  align-items: center;
  gap: 0.45em;
  font-size: 0.82rem;
  opacity: 0.75;
  cursor: pointer;
}
.ah-footer {
  display: flex;
  justify-content: flex-end;
}
.ah-done {
  padding: 6px 14px;
  border: 1px solid var(--fw-border, #ccc);
  border-radius: 4px;
  background: transparent;
  color: inherit;
  cursor: pointer;
  font-size: 0.9rem;
}
</style>
