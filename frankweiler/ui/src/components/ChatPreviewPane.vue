<script setup lang="ts">
import { ref, watch } from "vue";
import { RouterLink } from "vue-router";
import { fetchChat, type ChatResponse } from "@/api";
import ChatBody from "./ChatBody.vue";
import CopyUuidButton from "./CopyUuidButton.vue";
import FeedbackModal from "./FeedbackModal.vue";
import {
  buildContext,
  capturePreviewSelection,
  messageAncestor,
  type FeedbackContext,
} from "@/feedback/context";

const props = defineProps<{
  conversationUuid: string | null;
  messageIndex: number | null;
}>();

const chat = ref<ChatResponse | null>(null);
const loading = ref(false);
const error = ref<string | null>(null);

const feedbackOpen = ref(false);
const feedbackContext = ref<FeedbackContext | null>(null);
const feedbackSurfaceLabel = ref("");

function onPaneContextMenu(ev: MouseEvent) {
  // Cascade: active selection > message under cursor > whole-page fallback.
  // We never want to swallow the browser's right-click chrome unless we
  // actually have something to file feedback on, so bail before
  // preventDefault when no conversation is loaded.
  if (!chat.value) return;
  const conv = chat.value.conversation_uuid;
  const target = ev.target instanceof Element ? ev.target : null;

  const sel = capturePreviewSelection();
  if (sel) {
    ev.preventDefault();
    feedbackContext.value = buildContext({
      surface: "preview_selection",
      anchor: target,
      targetUuids: [
        conv,
        sel.start_message_uuid,
        sel.end_message_uuid,
      ].filter((v, i, a) => a.indexOf(v) === i),
      payload: sel,
    });
    feedbackSurfaceLabel.value = "Selected text";
    feedbackOpen.value = true;
    return;
  }

  const msgUuid = messageAncestor(target);
  if (msgUuid) {
    ev.preventDefault();
    const msgEl = target?.closest("[data-msg-index]");
    const idxAttr = msgEl?.getAttribute("data-msg-index") ?? "0";
    const msgIndex = Number.parseInt(idxAttr, 10);
    feedbackContext.value = buildContext({
      surface: "preview_message",
      anchor: target,
      targetUuids: [conv, msgUuid],
      payload: {
        conversation_uuid: conv,
        message_uuid: msgUuid,
        message_index: Number.isFinite(msgIndex) ? msgIndex : 0,
      },
    });
    feedbackSurfaceLabel.value = "Chat message";
    feedbackOpen.value = true;
    return;
  }

  // Click landed in the preview but outside any message — treat as
  // page-level feedback so the user always gets a way in.
  ev.preventDefault();
  feedbackContext.value = buildContext({
    surface: "page_header",
    anchor: target,
    targetUuids: [conv],
    payload: { entity_kind: "conversation", entity_uuid: conv },
  });
  feedbackSurfaceLabel.value = "Conversation";
  feedbackOpen.value = true;
}

watch(
  () => props.conversationUuid,
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
    :data-conversation-uuid="chat?.conversation_uuid ?? null"
    @contextmenu="onPaneContextMenu"
  >
    <p v-if="!conversationUuid" class="empty">
      Select a row to preview the conversation.
    </p>
    <p v-else-if="loading && !chat" class="empty">loading…</p>
    <p v-else-if="error" class="error">error: {{ error }}</p>
    <template v-else-if="chat">
      <header class="chat-header">
        <h2>
          {{ chat.name || chat.conversation_uuid }}
          <CopyUuidButton :uuid="chat.conversation_uuid" label="Copy page ID" />
        </h2>
        <p class="meta">
          <RouterLink
            :to="{
              name: 'chat',
              params: { conversationUuid: chat.conversation_uuid },
            }"
            >open full view ↗</RouterLink
          >
          <template v-if="chat.source_url">
            ·
            <a
              :href="chat.source_url"
              target="_blank"
              rel="noopener noreferrer"
              >Open in {{ chat.source_label || "source" }} ↗</a
            >
          </template>
          <span v-if="chat.created_at"> · {{ chat.created_at }}</span>
        </p>
      </header>
      <ChatBody :body="chat.body" :selected-message-index="messageIndex" />
    </template>
    <FeedbackModal
      :open="feedbackOpen"
      :surface-label="feedbackSurfaceLabel"
      :context="feedbackContext"
      @close="feedbackOpen = false"
    />
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
