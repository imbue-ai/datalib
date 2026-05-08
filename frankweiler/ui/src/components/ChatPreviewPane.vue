<script setup lang="ts">
import { ref, watch, computed, nextTick } from "vue";
import { RouterLink } from "vue-router";
import MarkdownIt from "markdown-it";
import hljs from "highlight.js";
import "highlight.js/styles/github-dark.css";
import { fetchChat, type ChatResponse } from "@/api";

const props = defineProps<{
  conversationUuid: string | null;
  messageIndex: number | null;
}>();

function highlight(code: string, lang: string): string {
  if (lang && hljs.getLanguage(lang)) {
    try {
      return hljs.highlight(code, { language: lang }).value;
    } catch {
      /* fall through to escape */
    }
  }
  // Minimal HTML escape for fallback paths
  return code
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

const md: MarkdownIt = new MarkdownIt({
  html: true,
  linkify: true,
  breaks: false,
  highlight,
});

const chat = ref<ChatResponse | null>(null);
const loading = ref(false);
const error = ref<string | null>(null);
const scroller = ref<HTMLElement | null>(null);

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
      await nextTick();
      scrollToSelected();
    } catch (e) {
      error.value = (e as Error).message;
    } finally {
      loading.value = false;
    }
  },
  { immediate: true },
);

watch(() => props.messageIndex, () => scrollToSelected());

function scrollToSelected() {
  if (!scroller.value || props.messageIndex == null) return;
  const target = scroller.value.querySelector(
    `#m-idx-${props.messageIndex}`,
  ) as HTMLElement | null;
  if (target) {
    target.scrollIntoView({ block: "start", behavior: "auto" });
  }
}

const renderedMessages = computed(() => {
  if (!chat.value) return [];
  return chat.value.messages.map((m, i) => ({
    index: i,
    sender: m.sender,
    when: m.when,
    html: md.render(m.text || ""),
  }));
});
</script>

<template>
  <section class="chat-preview" ref="scroller">
    <p v-if="!conversationUuid" class="empty">
      Select a row to preview the conversation.
    </p>
    <p v-else-if="loading && !chat" class="empty">loading…</p>
    <p v-else-if="error" class="error">error: {{ error }}</p>
    <template v-else-if="chat">
      <header class="chat-header">
        <h2>{{ chat.name || chat.conversation_uuid }}</h2>
        <p class="meta">
          <RouterLink
            :to="{
              name: 'chat',
              params: { conversationUuid: chat.conversation_uuid },
            }"
            >open full view ↗</RouterLink
          >
          <span v-if="chat.created_at"> · {{ chat.created_at }}</span>
        </p>
        <p v-if="chat.summary" class="summary">{{ chat.summary }}</p>
      </header>
      <article
        v-for="m in renderedMessages"
        :key="m.index"
        :id="`m-idx-${m.index}`"
        class="message"
        :class="[m.sender.toLowerCase(), { selected: m.index === messageIndex }]"
      >
        <header class="msg-header">
          <strong>{{ m.sender }}</strong>
          <span v-if="m.when" class="when">{{ m.when }}</span>
        </header>
        <div class="body markdown-body" v-html="m.html"></div>
      </article>
    </template>
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
.summary {
  font-style: italic;
  color: var(--fw-muted);
  margin: 0 0 0.75rem;
  font-size: 0.9rem;
}
.message {
  margin: 0.75rem 0;
  padding: 0.5rem 0.75rem;
  border-left: 3px solid var(--fw-border);
  background: var(--fw-card-bg);
  scroll-margin-top: 0.5rem;
}
.message.human {
  border-left-color: var(--fw-accent);
}
.message.assistant {
  border-left-color: #16a34a;
}
.message.selected {
  outline: 2px solid var(--fw-accent);
}
.msg-header {
  display: flex;
  gap: 0.5rem;
  align-items: baseline;
  font-size: 0.85rem;
}
.when {
  color: var(--fw-muted);
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
