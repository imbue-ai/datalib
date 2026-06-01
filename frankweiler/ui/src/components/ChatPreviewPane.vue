<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { RouterLink } from "vue-router";
import { fetchChat, type ChatResponse } from "@/api";
import ChatBody from "./ChatBody.vue";
import FeedbackButton from "./FeedbackButton.vue";
import FeedbackModal from "./FeedbackModal.vue";
import {
  buildContext,
  capturePreviewSelection,
  messageAncestor,
  type FeedbackContext,
} from "@/feedback/context";

const props = defineProps<{
  // Addresses one rendered `.md` file — the file the preview pane
  // should fetch and display. Same value `/api/chat/{markdown_uuid}`
  // takes.
  markdownUuid: string | null;
  // The clicked grid row's uuid (= the message UUID for message rows,
  // the prefixed `tu-…`/`tr-…`/`th-…` for block rows, null for
  // conversation-level rows). The renderer emits each section with a
  // matching `data-section-uuid` attribute, so the UI just needs to
  // forward this prop and ChatBody does an exact-string lookup.
  selectedSectionUuid: string | null;
}>();

const chat = ref<ChatResponse | null>(null);
const loading = ref(false);
const error = ref<string | null>(null);

const feedbackOpen = ref(false);
const feedbackContext = ref<FeedbackContext | null>(null);
const feedbackSurfaceLabel = ref("");

// Right-click context menu state. We defer building the feedback
// context until the user actually picks "Feedback…", but the surface
// kind ('selection' / 'message' / 'conversation') is decided up-front
// at right-click time so the Copy and Feedback actions agree on what
// the user was pointing at.
type PendingTarget =
  | {
      kind: "selection";
      anchor: Element | null;
      conv: string;
      sel: ReturnType<typeof capturePreviewSelection>;
      selectionText: string;
    }
  | {
      kind: "message";
      anchor: Element | null;
      conv: string;
      msgUuid: string;
      msgIndex: number;
    }
  | {
      kind: "conversation";
      anchor: Element | null;
      conv: string;
    };

const ctxMenuVisible = ref(false);
const ctxMenuPos = ref({ x: 0, y: 0 });
const ctxTarget = ref<PendingTarget | null>(null);

function onPaneContextMenu(ev: MouseEvent) {
  if (!chat.value) return;
  const conv = chat.value.markdown_uuid;
  const target = ev.target instanceof Element ? ev.target : null;

  // Cascade: active selection > message under cursor > whole-page fallback.
  // Whichever path we take, we then open our custom context menu instead
  // of jumping straight into the feedback modal.
  let pending: PendingTarget;
  const sel = capturePreviewSelection();
  if (sel) {
    pending = {
      kind: "selection",
      anchor: target,
      conv,
      sel,
      selectionText: window.getSelection()?.toString() ?? "",
    };
  } else {
    const msgUuid = messageAncestor(target);
    if (msgUuid) {
      // The feedback schema's preview_message payload still has a
      // required `message_index` (see schemas/feedback.schema.json).
      // The renderer no longer emits `data-msg-index` — `message_uuid`
      // is the load-bearing field now. Pass 0 until the feedback
      // schema follow-up drops the index.
      pending = {
        kind: "message",
        anchor: target,
        conv,
        msgUuid,
        msgIndex: 0,
      };
    } else {
      pending = { kind: "conversation", anchor: target, conv };
    }
  }

  ev.preventDefault();
  ctxTarget.value = pending;
  ctxMenuPos.value = { x: ev.clientX, y: ev.clientY };
  ctxMenuVisible.value = true;
}

function closeCtxMenu() {
  ctxMenuVisible.value = false;
  ctxTarget.value = null;
}

const copyLabel = computed(() => {
  const t = ctxTarget.value;
  if (!t) return "Copy";
  if (t.kind === "selection") return "Copy selected text";
  if (t.kind === "message") return "Copy message ID";
  return "Copy conversation ID";
});

function copyTargetText(): string {
  const t = ctxTarget.value;
  if (!t) return "";
  if (t.kind === "selection") return t.selectionText;
  if (t.kind === "message") return t.msgUuid;
  return t.conv;
}

async function onCopy() {
  const text = copyTargetText();
  if (!text) {
    closeCtxMenu();
    return;
  }
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    // Fallback for non-secure contexts.
    const ta = document.createElement("textarea");
    ta.value = text;
    document.body.appendChild(ta);
    ta.select();
    document.execCommand("copy");
    document.body.removeChild(ta);
  }
  closeCtxMenu();
}

function onFeedback() {
  const t = ctxTarget.value;
  if (!t) {
    closeCtxMenu();
    return;
  }
  if (t.kind === "selection" && t.sel) {
    feedbackContext.value = buildContext({
      surface: "preview_selection",
      anchor: t.anchor,
      targetUuids: [
        t.conv,
        t.sel.start_message_uuid,
        t.sel.end_message_uuid,
      ].filter((v, i, a) => a.indexOf(v) === i),
      payload: t.sel,
    });
    feedbackSurfaceLabel.value = "Selected text";
  } else if (t.kind === "message") {
    feedbackContext.value = buildContext({
      surface: "preview_message",
      anchor: t.anchor,
      targetUuids: [t.conv, t.msgUuid],
      payload: {
        conversation_uuid: t.conv,
        message_uuid: t.msgUuid,
        message_index: t.msgIndex,
      },
    });
    feedbackSurfaceLabel.value = "Chat message";
  } else {
    feedbackContext.value = buildContext({
      surface: "page_header",
      anchor: t.anchor,
      targetUuids: [t.conv],
      payload: { entity_kind: "conversation", entity_uuid: t.conv },
    });
    feedbackSurfaceLabel.value = "Conversation";
  }
  feedbackOpen.value = true;
  closeCtxMenu();
}

watch(
  // One UUID per rendered file — when `markdownUuid` changes, refetch
  // the file. No row-uuid disambiguation needed: provider-specific
  // sharding (beeper's per-period files) is already encoded in the
  // markdown_uuid the parent passes.
  () => props.markdownUuid,
  async (uuid) => {
    if (!uuid) {
      chat.value = null;
      return;
    }
    loading.value = true;
    error.value = null;
    try {
      chat.value = await fetchChat(uuid);
    } catch (e) {
      error.value = (e as Error).message;
    } finally {
      loading.value = false;
    }
  },
  { immediate: true },
);
</script>

<template>
  <section
    class="chat-preview"
    :data-markdown-uuid="chat?.markdown_uuid ?? null"
    @contextmenu="onPaneContextMenu"
  >
    <p v-if="!markdownUuid" class="empty">
      Select a row to preview the conversation.
    </p>
    <p v-else-if="loading && !chat" class="empty">loading…</p>
    <p v-else-if="error" class="error">error: {{ error }}</p>
    <template v-else-if="chat">
      <header class="chat-header">
        <!-- Title block (with copy-id button and source-URL arrow) is
             rendered inline at the top of the body by the cross-provider
             `Title` helper. The header here only carries the
             non-title chrome: feedback button, "open full view" link,
             and timestamps. -->
        <p class="meta">
          <FeedbackButton
            :entity-uuid="chat.markdown_uuid"
            entity-kind="conversation"
            label="Conversation"
          />
          ·
          <RouterLink
            :to="{
              name: 'chat',
              params: { markdownUuid: chat.markdown_uuid },
            }"
            >open full view ↗</RouterLink
          >
          <span v-if="chat.created_at"> · {{ chat.created_at }}</span>
        </p>
      </header>
      <ChatBody :body="chat.body" :selected-section-uuid="selectedSectionUuid" />
    </template>
    <FeedbackModal
      :open="feedbackOpen"
      :surface-label="feedbackSurfaceLabel"
      :context="feedbackContext"
      @close="feedbackOpen = false"
    />
    <div
      v-if="ctxMenuVisible"
      class="ctx-overlay"
      @click="closeCtxMenu"
      @contextmenu.prevent="closeCtxMenu"
    >
      <div
        class="ctx-menu"
        :style="{ top: ctxMenuPos.y + 'px', left: ctxMenuPos.x + 'px' }"
        @click.stop
      >
        <div class="ctx-item" @click="onCopy">{{ copyLabel }}</div>
        <div class="ctx-divider" />
        <div class="ctx-item" @click="onFeedback">Feedback…</div>
      </div>
    </div>
  </section>
</template>

<style scoped>
.chat-preview {
  height: 100%;
  overflow-y: auto;
  padding: 0.75rem 1rem;
  box-sizing: border-box;
}
.chat-header h2 {
  margin: 0 0 0.25rem;
  font-size: 1.1rem;
}
.meta {
  font-size: 0.8rem;
  color: var(--fw-muted);
  margin: 0 0 0.25rem;
}
.empty,
.error {
  color: var(--fw-muted);
  padding: 1rem;
}
.error {
  color: #e35d6a;
}
.ctx-overlay {
  position: fixed;
  inset: 0;
  z-index: 1500;
  background: transparent;
}
.ctx-menu {
  position: fixed;
  background: var(--fw-input-bg, #fff);
  color: var(--fw-fg, #000);
  border: 1px solid var(--fw-border, #ccc);
  border-radius: 4px;
  box-shadow: 0 2px 10px rgba(0, 0, 0, 0.2);
  min-width: 180px;
  padding: 4px 0;
  z-index: 1501;
  font-size: 14px;
}
.ctx-item {
  padding: 8px 16px;
  cursor: pointer;
  user-select: none;
}
.ctx-item:hover {
  background: var(--fw-accent, #eee);
}
.ctx-divider {
  height: 1px;
  background: var(--fw-border, #ccc);
  margin: 4px 0;
}
</style>

<style>
/* Global markdown styling — :deep() doesn't reach v-html descendants
   reliably across Vue versions, so scope by class instead. */
.markdown-body {
  font-size: 0.9rem;
  line-height: 1.45;
}
.markdown-body p {
  margin: 0.4rem 0;
}
.markdown-body pre {
  background: var(--fw-code-bg, #0d1117);
  color: #e6edf3;
  padding: 0.6rem 0.75rem;
  border-radius: 4px;
  overflow-x: auto;
  font-size: 0.82rem;
}
.markdown-body code {
  font-family: ui-monospace, Menlo, monospace;
  font-size: 0.85em;
}
.markdown-body :not(pre) > code {
  background: var(--fw-code-bg, #f0f0f0);
  padding: 0 0.25rem;
  border-radius: 2px;
}
.markdown-body details {
  margin: 0.4rem 0;
  padding: 0.25rem 0.5rem;
  border: 1px solid var(--fw-border);
  border-radius: 4px;
  background: var(--fw-card-bg);
}
.markdown-body details > summary {
  cursor: pointer;
  font-size: 0.85rem;
  color: var(--fw-muted);
}
.markdown-body details[open] > summary {
  margin-bottom: 0.4rem;
}
.markdown-body blockquote {
  border-left: 3px solid var(--fw-border);
  margin: 0.5rem 0;
  padding-left: 0.75rem;
  color: var(--fw-muted);
}
</style>
