<script setup lang="ts">
// A confirm with one checkbox, for a question `confirmAction` cannot
// ask: the platform's confirm is OK or Cancel and nothing else. Used when
// removing a comparison, whose computed changes can go with it.
import { onMounted, ref } from "vue";

const props = defineProps<{
  title: string;
  /// Paragraphs, split on blank lines.
  message: string;
  checkLabel: string;
  confirmLabel: string;
}>();

const emit = defineEmits<{
  (e: "answer", answer: { ok: boolean; checked: boolean }): void;
}>();

const checked = ref(true);
const confirmButton = ref<HTMLButtonElement | null>(null);

onMounted(() => confirmButton.value?.focus());

function answer(ok: boolean) {
  emit("answer", { ok, checked: checked.value });
}

function onKeydown(e: KeyboardEvent) {
  if (e.key === "Escape") answer(false);
}
</script>

<template>
  <div class="dialog-backdrop" @click.self="answer(false)" @keydown="onKeydown">
    <div class="cfm dialog" role="dialog" aria-modal="true" :aria-label="props.title">
      <header class="dialog-head">
        <h2>{{ props.title }}</h2>
      </header>
      <div class="cfm-body dialog-body">
        <p v-for="(para, i) in props.message.split(/\n\n+/)" :key="i">{{ para }}</p>
        <label class="cfm-check">
          <input v-model="checked" type="checkbox" />
          <span>{{ props.checkLabel }}</span>
        </label>
      </div>
      <footer class="dialog-foot">
        <button class="btn ghost" @click="answer(false)">Cancel</button>
        <button ref="confirmButton" class="btn primary" @click="answer(true)">
          {{ props.confirmLabel }}
        </button>
      </footer>
    </div>
  </div>
</template>

<style scoped src="./dialog.css"></style>
<style scoped>
.cfm {
  width: min(520px, 100%);
}
.cfm-body {
  display: flex;
  flex-direction: column;
  gap: 12px;
}
.cfm-body p {
  margin: 0;
}
.cfm-check {
  display: flex;
  align-items: flex-start;
  gap: 8px;
  cursor: pointer;
}
.cfm-check input {
  margin-top: 3px;
}
</style>
