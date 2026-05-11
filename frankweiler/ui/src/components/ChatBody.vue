<script setup lang="ts">
// Renders a chat conversation. The backend serves the QMD body verbatim
// (CommonMark + per-message `<div id="m-{uuid}" data-msg-index="…">`
// wrappers emitted by the ingest renderer); we run markdown-it once.
//
// `selectedMessageIndex` (or `selectedMessageUuid`) selects which message
// to scroll to and visually highlight via the `.msg.selected` CSS rule.

import { ref, computed, watch, nextTick, onMounted } from "vue";
import MarkdownIt from "markdown-it";
import hljs from "highlight.js";
import "highlight.js/styles/github-dark.css";

const props = defineProps<{
  body: string;
  selectedMessageIndex?: number | null;
  selectedMessageUuid?: string | null;
}>();

function highlight(code: string, lang: string): string {
  if (lang && hljs.getLanguage(lang)) {
    try {
      return hljs.highlight(code, { language: lang }).value;
    } catch {
      /* fall through to escape */
    }
  }
  return code
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

const md = new MarkdownIt({
  html: true,
  linkify: true,
  breaks: false,
  highlight,
});

const html = computed(() => md.render(props.body || ""));
const root = ref<HTMLElement | null>(null);

function applySelection() {
  if (!root.value) return;
  for (const el of root.value.querySelectorAll(".msg.selected")) {
    el.classList.remove("selected");
  }
  let target: HTMLElement | null = null;
  if (props.selectedMessageUuid) {
    target = root.value.querySelector(
      `#m-${CSS.escape(props.selectedMessageUuid)}`,
    );
  } else if (props.selectedMessageIndex != null) {
    target = root.value.querySelector(
      `[data-msg-index="${props.selectedMessageIndex}"]`,
    );
  }
  if (!target) return;
  target.classList.add("selected");
  // Set scrollTop on the known scrollport directly instead of calling
  // target.scrollIntoView: scrollIntoView silently no-ops on
  // same-conversation prop changes in Chromium (probably racing layout
  // re-flow), and the user reported "nothing changes when I click
  // around inside one thread" as a result.
  const pane = target.closest(".chat-preview") as HTMLElement | null;
  if (pane) {
    pane.scrollTop +=
      target.getBoundingClientRect().top - pane.getBoundingClientRect().top;
  }
}

watch(html, async () => {
  await nextTick();
  applySelection();
});
watch(
  () => [props.selectedMessageIndex, props.selectedMessageUuid],
  async () => {
    // nextTick guards against a parent setting messageIndex in the same
    // tick that it loads a new conversation: we want the html v-html
    // patch to land before we look for `[data-msg-index]`.
    await nextTick();
    applySelection();
  },
);
onMounted(() => applySelection());
</script>

<template>
  <div class="chat-body markdown-body" ref="root" v-html="html"></div>
</template>

<style>
/* Per-message wrappers emitted by ingest. Unscoped on purpose so the rules
   reach inside `v-html`. */
.chat-body .msg {
  scroll-margin-top: 1rem;
  padding: 0.5rem 0.75rem;
  border-left: 3px solid transparent;
  margin: 0.5rem 0;
}
.chat-body .msg--anthropic {
  border-left-color: var(--fw-accent, #6366f1);
}
.chat-body .msg--openai {
  border-left-color: #16a34a;
}
.chat-body .msg--slack {
  border-left-color: #4a154b;
}
.chat-body .msg.selected {
  background: var(--fw-card-bg, #1f2937);
  border-left-width: 4px;
  /* `outline` (not border) so moving the selection doesn't reflow. */
  outline: 2px solid var(--fw-accent, #6366f1);
}
.chat-body .msg-meta {
  color: var(--fw-muted, #94a3b8);
  font-size: 0.85rem;
  margin: 0 0 0.5rem;
}
.chat-body .msg-meta a {
  color: inherit;
  text-decoration: underline;
}
</style>
