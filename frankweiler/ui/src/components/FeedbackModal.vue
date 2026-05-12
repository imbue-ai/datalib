<!--
  FeedbackModal — the "Feedback…" dialog every right-click context menu and
  page-header button funnels into. Surface-agnostic: the caller hands us a
  `FeedbackContext` (already populated by `feedback/context.ts`) and a
  short surface label for the title; we own the thumbs / comment / submit
  state and the POST itself, so each call site stays a one-liner.

  Submit is disabled until `comment` is non-empty — matching the same
  invariant the backend enforces (handlers return 400 on whitespace-only
  comments). After a successful submit we surface the server-stamped
  `feedback_uuid` in a brief toast so the user knows the row landed and
  can reference it against `dolt log`.
-->
<script setup lang="ts">
import { computed, nextTick, ref, watch } from "vue";

import { submitFeedback } from "../api";
import type { FeedbackContext } from "../feedback/context";

const props = defineProps<{
  /** When false, the modal is fully unmounted (no fade-out — keep state crisp). */
  open: boolean;
  /** Title-bar surface label, e.g. "Grid cell · author". */
  surfaceLabel: string;
  /** Fully-populated context. Producer-side concerns (DOM path, target
   * UUIDs, payload variant) are baked in before we get it. */
  context: FeedbackContext | null;
}>();

const emit = defineEmits<{
  (e: "close"): void;
  /** Fires only after the POST succeeds. The parent uses this to clear
   * any pending state — we don't try to share status back through props. */
  (e: "submitted", feedbackUuid: string): void;
}>();

const sentiment = ref<"up" | "down" | null>(null);
const comment = ref("");
const submitting = ref(false);
const errorMsg = ref<string | null>(null);
const successUuid = ref<string | null>(null);
const commentRef = ref<HTMLTextAreaElement | null>(null);

// Reset state on each open. Without this, dismissing and reopening the
// modal would carry over the previous attempt's comment and sentiment —
// almost certainly not what the user wants on a new surface.
watch(
  () => props.open,
  (now) => {
    if (now) {
      // Default to thumbs-down: in practice every feedback we file is
      // "this is wrong / could be better", and clicking the modal open
      // is itself the signal. The user can clear it or flip to thumbs-up.
      sentiment.value = "down";
      comment.value = "";
      submitting.value = false;
      errorMsg.value = null;
      successUuid.value = null;
      // Wait one tick so the textarea is in the DOM before focus.
      nextTick(() => commentRef.value?.focus());
    }
  },
);

const canSubmit = computed(
  () => !submitting.value && comment.value.trim().length > 0 && props.context !== null,
);

function toggleSentiment(v: "up" | "down") {
  sentiment.value = sentiment.value === v ? null : v;
}

async function onSubmit() {
  if (!canSubmit.value || !props.context) return;
  submitting.value = true;
  errorMsg.value = null;
  try {
    const resp = await submitFeedback({
      sentiment: sentiment.value,
      comment: comment.value,
      context: props.context,
    });
    successUuid.value = resp.feedback_uuid;
    emit("submitted", resp.feedback_uuid);
    // Brief confirmation, then auto-close. 1.2s matches the toast feel
    // already used elsewhere (CopyUuidButton's "Copied!" flash).
    setTimeout(() => emit("close"), 1200);
  } catch (e) {
    errorMsg.value = e instanceof Error ? e.message : String(e);
    submitting.value = false;
  }
}

function onCancel() {
  if (submitting.value) return;
  emit("close");
}

function onKeydown(ev: KeyboardEvent) {
  if (ev.key === "Escape") {
    ev.preventDefault();
    onCancel();
  }
}
</script>

<template>
  <Teleport to="body">
    <div
      v-if="open"
      class="fb-overlay"
      @click.self="onCancel"
      @keydown="onKeydown"
    >
      <div
        class="fb-modal"
        role="dialog"
        aria-modal="true"
        :aria-label="`Feedback on ${surfaceLabel}`"
      >
        <header class="fb-header">
          <div class="fb-title">Feedback</div>
          <div class="fb-surface">{{ surfaceLabel }}</div>
        </header>
        <div class="fb-sentiments">
          <button
            type="button"
            class="fb-thumb"
            :class="{ active: sentiment === 'up' }"
            :aria-pressed="sentiment === 'up'"
            @click="toggleSentiment('up')"
          >
            👍 Helpful
          </button>
          <button
            type="button"
            class="fb-thumb"
            :class="{ active: sentiment === 'down' }"
            :aria-pressed="sentiment === 'down'"
            @click="toggleSentiment('down')"
          >
            👎 Wrong
          </button>
        </div>
        <textarea
          ref="commentRef"
          v-model="comment"
          class="fb-comment"
          rows="4"
          placeholder="What's wrong or what should this do? (required)"
          :disabled="submitting"
        />
        <div v-if="errorMsg" class="fb-error">Couldn't submit: {{ errorMsg }}</div>
        <div v-if="successUuid" class="fb-success">
          Filed as <code>{{ successUuid }}</code>
        </div>
        <footer class="fb-footer">
          <button
            type="button"
            class="fb-btn fb-cancel"
            :disabled="submitting"
            @click="onCancel"
          >
            Cancel
          </button>
          <button
            type="button"
            class="fb-btn fb-submit"
            :disabled="!canSubmit"
            @click="onSubmit"
          >
            {{ submitting ? "Submitting…" : "Submit" }}
          </button>
        </footer>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
.fb-overlay {
  position: fixed;
  inset: 0;
  z-index: 2000;
  background: rgba(0, 0, 0, 0.35);
  display: flex;
  align-items: center;
  justify-content: center;
}
.fb-modal {
  background: var(--fw-input-bg, #fff);
  color: var(--fw-fg, #000);
  border: 1px solid var(--fw-border, #ccc);
  border-radius: 6px;
  box-shadow: 0 8px 32px rgba(0, 0, 0, 0.25);
  width: min(520px, 90vw);
  padding: 16px 18px 14px;
  display: flex;
  flex-direction: column;
  gap: 10px;
}
.fb-header {
  display: flex;
  flex-direction: column;
  gap: 2px;
}
.fb-title {
  font-size: 1.05rem;
  font-weight: 600;
}
.fb-surface {
  font-size: 0.8rem;
  color: var(--fw-fg-muted, #888);
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
}
.fb-sentiments {
  display: flex;
  gap: 8px;
}
.fb-thumb {
  flex: 1 1 0;
  padding: 6px 10px;
  border: 1px solid var(--fw-border, #ccc);
  border-radius: 4px;
  background: transparent;
  color: inherit;
  cursor: pointer;
  font-size: 0.9rem;
}
.fb-thumb.active {
  background: var(--fw-accent, #e0e8ff);
  border-color: var(--fw-accent-border, #88a);
}
.fb-comment {
  width: 100%;
  box-sizing: border-box;
  padding: 8px 10px;
  font: inherit;
  background: var(--fw-bg, #fff);
  color: inherit;
  border: 1px solid var(--fw-border, #ccc);
  border-radius: 4px;
  resize: vertical;
  min-height: 80px;
}
.fb-error {
  color: #e35d6a;
  font-size: 0.85rem;
}
.fb-success {
  color: #3a8a3a;
  font-size: 0.85rem;
}
.fb-success code {
  background: var(--fw-code-bg, #f5f5f5);
  padding: 0 0.25rem;
  border-radius: 2px;
}
.fb-footer {
  display: flex;
  justify-content: flex-end;
  gap: 8px;
}
.fb-btn {
  padding: 6px 14px;
  border: 1px solid var(--fw-border, #ccc);
  border-radius: 4px;
  background: transparent;
  color: inherit;
  cursor: pointer;
  font-size: 0.9rem;
}
.fb-btn:disabled {
  opacity: 0.5;
  cursor: not-allowed;
}
.fb-submit {
  background: var(--fw-accent, #4060c0);
  color: var(--fw-accent-fg, #fff);
  border-color: var(--fw-accent-border, #4060c0);
}
</style>
