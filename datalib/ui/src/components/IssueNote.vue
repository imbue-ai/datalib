<script setup lang="ts">
// One failure, said the way the wizard says every failure: a mark and a
// sentence for its kind, what to do, and the raw text folded away for
// whoever needs it. See config/issues.ts for the words.
import { computed } from "vue";
import type { Failure } from "@/api";
import { STATUS_GLYPHS } from "@/config/glyphs";
import { issueText, type SignInWhere } from "@/config/issues";

const props = defineProps<{ failure: Failure; service: string; where: SignInWhere }>();
const text = computed(() => issueText(props.failure, props.service, props.where));
</script>

<template>
  <div class="issue" :data-issue="failure.issue">
    <p class="issue-headline">
      <svg class="issue-mark" viewBox="0 0 24 24" role="img" aria-label="Failed">
        <path :d="STATUS_GLYPHS.failed" fill="currentColor" />
      </svg>
      {{ text.headline }}
    </p>
    <p v-if="text.advice" class="issue-advice">{{ text.advice }}</p>
    <details v-if="failure.detail">
      <summary>Details</summary>
      <pre class="issue-detail">{{ failure.detail }}</pre>
    </details>
  </div>
</template>

<style scoped>
.issue {
  font-size: var(--datalib-font-size-small);
  line-height: 1.45;
}
.issue p {
  margin: 0 0 2px;
}
.issue-headline {
  color: var(--datalib-error-fg);
}
.issue-mark {
  width: 14px;
  height: 14px;
  vertical-align: -3px;
  margin-right: 3px;
  color: var(--datalib-log-error);
}
.issue-advice,
.issue summary {
  color: var(--datalib-muted);
}
.issue summary {
  cursor: pointer;
}
/* The step's text as written: numbered recipes and shell commands,
   which reflowed into a paragraph are unreadable. */
.issue-detail {
  margin: 6px 0 0;
  padding: 8px 10px;
  max-height: 220px;
  overflow: auto;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  background: var(--datalib-code-bg);
  border-radius: var(--datalib-radius);
  line-height: 1.5;
}
</style>
