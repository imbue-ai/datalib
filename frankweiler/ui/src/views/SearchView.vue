<script setup lang="ts">
import { ref } from "vue";
const query = ref("");
</script>

<template>
  <section class="search-view">
    <input
      v-model="query"
      placeholder="search messages…"
      class="search-input"
      data-testid="search-input"
    />
    <p class="empty">
      Backend not wired yet — start <code>frankweiler-http</code> and the grid
      will populate.
    </p>
  </section>
</template>

<style scoped>
.search-input {
  width: 100%;
  padding: 0.5rem;
  font-size: 1rem;
}
.empty {
  margin-top: 1rem;
  color: #888;
}
</style>
