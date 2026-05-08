<script setup lang="ts">
import { ref, watch, onMounted } from "vue";
import { useRoute, RouterLink } from "vue-router";
import { fetchChat, type ChatResponse } from "@/api";
import ChatBody from "@/components/ChatBody.vue";

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
          <span v-if="chat.account"> · account {{ chat.account }}</span>
        </p>
      </header>
      <ChatBody :body="chat.body" />
    </template>
  </section>
</template>

<style scoped>
.chat-view {
  max-width: 80ch;
  margin: 0 auto;
}
.meta {
  color: var(--fw-muted);
  font-size: 0.85rem;
}
.error {
  color: #e35d6a;
}
</style>
