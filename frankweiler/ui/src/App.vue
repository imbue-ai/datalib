<script setup lang="ts">
import { RouterView } from "vue-router";
</script>

<template>
  <main class="frankweiler-shell">
    <header><h1>Frankweiler</h1></header>
    <RouterView />
  </main>
</template>

<style>
.frankweiler-shell {
  font-family: system-ui, sans-serif;
  padding: 1rem;
}
</style>
