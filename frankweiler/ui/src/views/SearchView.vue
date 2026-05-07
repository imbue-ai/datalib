<script setup lang="ts">
import { ref, watch, onMounted } from "vue";
import { RouterLink } from "vue-router";
import { fetchHealth, fetchSearch, type Health, type SearchRow } from "@/api";

const query = ref("");
const rows = ref<SearchRow[]>([]);
const total = ref(0);
const loading = ref(false);
const error = ref<string | null>(null);
const health = ref<Health | null>(null);

let inflight: AbortController | null = null;
let debounceTimer: ReturnType<typeof setTimeout> | null = null;

async function runSearch(q: string) {
  inflight?.abort();
  inflight = new AbortController();
  loading.value = true;
  error.value = null;
  try {
    const r = await fetchSearch(q, 200, inflight.signal);
    rows.value = r.rows;
    total.value = r.total_estimated;
  } catch (e) {
    if ((e as { name?: string }).name === "AbortError") return;
    error.value = (e as Error).message;
  } finally {
    loading.value = false;
  }
}

watch(query, (q) => {
  if (debounceTimer) clearTimeout(debounceTimer);
  debounceTimer = setTimeout(() => runSearch(q), 150);
});

onMounted(async () => {
  try {
    health.value = await fetchHealth();
  } catch {
    /* health is best-effort */
  }
  runSearch("");
});
</script>

<template>
  <section class="search-view">
    <input
      v-model="query"
      placeholder="search messages…  (try: type:chat, account:…, before:2025-01-01)"
      class="search-input"
      data-testid="search-input"
      autofocus
    />

    <div v-if="health" class="health">
      backend ok · {{ total }} conversations indexed under
      <code>{{ health.root }}</code>
      <span v-if="!health.root_exists" class="warn"> (root does not exist)</span>
    </div>

    <p v-if="error" class="error">error: {{ error }}</p>
    <p v-else-if="loading && rows.length === 0" class="empty">searching…</p>
    <p v-else-if="rows.length === 0" class="empty">no matches.</p>

    <table v-else class="results">
      <thead>
        <tr>
          <th>Snippet</th>
          <th>Sender</th>
          <th>When</th>
          <th>Conversation</th>
        </tr>
      </thead>
      <tbody>
        <tr
          v-for="(r, i) in rows"
          :key="`${r.conversation_uuid}:${r.message_index ?? '-'}:${i}`"
          class="row"
        >
          <td class="snippet">{{ r.snippet }}</td>
          <td class="sender">{{ r.sender }}</td>
          <td class="when">{{ r.when }}</td>
          <td class="conv">
            <RouterLink :to="{ name: 'chat', params: { conversationUuid: r.conversation_uuid } }">
              {{ r.conversation_name || r.conversation_uuid }}
            </RouterLink>
          </td>
        </tr>
      </tbody>
    </table>
  </section>
</template>

<style scoped>
.search-input {
  width: 100%;
  padding: 0.5rem 0.75rem;
  font-size: 1rem;
  box-sizing: border-box;
}
.health {
  margin-top: 0.5rem;
  font-size: 0.85rem;
  color: #666;
}
.health code {
  background: #f4f4f4;
  padding: 0 0.25rem;
}
.warn {
  color: #b15a00;
  margin-left: 0.5rem;
}
.empty,
.error {
  margin-top: 1rem;
  color: #888;
}
.error {
  color: #b00020;
}
.results {
  width: 100%;
  margin-top: 1rem;
  border-collapse: collapse;
  font-size: 0.9rem;
}
.results th,
.results td {
  text-align: left;
  padding: 0.4rem 0.6rem;
  border-bottom: 1px solid #eee;
  vertical-align: top;
}
.results th {
  font-weight: 600;
  background: #fafafa;
}
.snippet {
  max-width: 60ch;
}
.sender,
.when {
  white-space: nowrap;
  color: #555;
}
</style>
