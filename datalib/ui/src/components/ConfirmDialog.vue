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
  <div class="cfm-backdrop" @click.self="answer(false)" @keydown="onKeydown">
    <div class="cfm" role="dialog" aria-modal="true" :aria-label="props.title">
      <header class="cfm-head">
        <h2>{{ props.title }}</h2>
      </header>
      <div class="cfm-body">
        <p v-for="(para, i) in props.message.split(/\n\n+/)" :key="i">{{ para }}</p>
        <label class="cfm-check">
          <input v-model="checked" type="checkbox" />
          <span>{{ props.checkLabel }}</span>
        </label>
      </div>
      <footer class="cfm-foot">
        <button class="btn ghost" @click="answer(false)">Cancel</button>
        <button ref="confirmButton" class="btn primary" @click="answer(true)">
          {{ props.confirmLabel }}
        </button>
      </footer>
    </div>
  </div>
</template>

<style scoped>
.cfm-backdrop {
  position: fixed;
  inset: 0;
  background: rgba(0, 0, 0, 0.45);
  display: flex;
  align-items: flex-start;
  justify-content: center;
  padding: 12vh 16px;
  z-index: 50;
}
.cfm {
  background: var(--datalib-bg);
  color: var(--datalib-fg);
  border: 1px solid var(--datalib-border);
  border-radius: 8px;
  width: min(520px, 100%);
  display: flex;
  flex-direction: column;
  box-shadow: 0 18px 48px rgba(0, 0, 0, 0.35);
}
.cfm-head,
.cfm-foot {
  display: flex;
  align-items: center;
  gap: 12px;
  padding: 14px 18px;
}
.cfm-head {
  border-bottom: 1px solid var(--datalib-border);
}
.cfm-foot {
  border-top: 1px solid var(--datalib-border);
  justify-content: flex-end;
}
.cfm-head h2 {
  margin: 0;
  font-size: 17px;
}
.cfm-body {
  padding: 16px 18px;
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
