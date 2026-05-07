<script setup lang="ts">
import { ref, watch, onMounted } from "vue";
import { useRoute, RouterLink } from "vue-router";
import { fetchChat, type ChatResponse } from "@/api";

const route = useRoute();
const chat = ref<ChatResponse | null>(null);
const error = ref<string | null>(null);
const loading = ref(false);

async function load(uuid: string) {
  loading.value = true;
  error.value = null;
  try {
    chat.value = await fetchChat(uuid);
  } catch (e) {
    error.value = (e as Error).message;
  } finally {
    loading.value = false;
  }
}

onMounted(() => load(String(route.params.conversationUuid)));
watch(
  () => route.params.conversationUuid,
  (u) => u && load(String(u)),
);
</script>

<template>
  <section class="chat-view">
    <p><RouterLink to="/search">← back to search</RouterLink></p>
    <p v-if="loading" class="empty">loading…</p>
    <p v-else-if="error" class="error">error: {{ error }}</p>
    <template v-else-if="chat">
      <header>
        <h2>{{ chat.name || chat.conversation_uuid }}</h2>
        <p class="meta">
          <span v-if="chat.created_at">created {{ chat.created_at }}</span>
          <span v-if="chat.account_uuid"> · account {{ chat.account_uuid }}</span>
        </p>
        <p v-if="chat.summary" class="summary">{{ chat.summary }}</p>
      </header>
      <article
        v-for="(m, i) in chat.messages"
        :key="i"
        class="message"
        :class="m.sender.toLowerCase()"
      >
        <header class="msg-header">
          <strong>{{ m.sender }}</strong>
          <span v-if="m.when" class="when">{{ m.when }}</span>
        </header>
        <pre class="body">{{ m.text }}</pre>
      </article>
    </template>
  </section>
</template>

<style scoped>
.chat-view {
  max-width: 80ch;
  margin: 0 auto;
}
.meta {
  color: #777;
  font-size: 0.85rem;
}
.summary {
  font-style: italic;
  color: #444;
}
.message {
  margin: 1.25rem 0;
  padding: 0.75rem 1rem;
  border-left: 3px solid #ccc;
  background: #fafafa;
}
.message.human {
  border-left-color: #2563eb;
  background: #f5f8ff;
}
.message.assistant {
  border-left-color: #16a34a;
  background: #f5fbf6;
}
.msg-header {
  display: flex;
  gap: 0.75rem;
  align-items: baseline;
}
.when {
  color: #777;
  font-size: 0.85rem;
}
.body {
  white-space: pre-wrap;
  font-family: inherit;
  margin: 0.5rem 0 0;
}
.error {
  color: #b00020;
}
</style>
