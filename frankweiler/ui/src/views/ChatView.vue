<script setup lang="ts">
import { useRoute } from "vue-router";
const route = useRoute();
</script>

<template>
  <section><h2>Chat {{ route.params.conversationUuid }}</h2></section>
</template>
