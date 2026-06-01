<script setup lang="ts">
import { ref, watch, onMounted } from "vue";
import { useRoute, RouterLink } from "vue-router";
import { fetchChat, type ChatResponse } from "@/api";
import ChatBody from "@/components/ChatBody.vue";
import FeedbackButton from "@/components/FeedbackButton.vue";

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

onMounted(() => load(String(route.params.markdownUuid)));
watch(
  () => route.params.markdownUuid,
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
        <!-- Title block + copy-id button + source-URL arrow render
             inline at the top of the body via the cross-provider
             `Title` helper. We only carry the chrome that ISN'T the
             title here: feedback button + creation/account metadata. -->
        <p class="meta">
          <FeedbackButton
            :entity-uuid="chat.markdown_uuid"
            entity-kind="conversation"
            label="Conversation"
          />
          <span v-if="chat.created_at"> · created {{ chat.created_at }}</span>
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
