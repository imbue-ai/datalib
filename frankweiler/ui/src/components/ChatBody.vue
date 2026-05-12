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

function injectCopyUuidButtons() {
  if (!root.value) return;
  for (const el of root.value.querySelectorAll<HTMLElement>(".msg[id^='m-']")) {
    if (el.querySelector(":scope > .msg-meta .copy-uuid, :scope > p .copy-uuid"))
      continue;
    const uuid = el.id.slice(2);
    if (!uuid) continue;
    // Prefer the explicit `.msg-meta` div (slack). Otherwise use the first
    // <p><em>…</em></p> emitted as the markdown italic meta line
    // (github/gitlab/anthropic/openai).
    let host: HTMLElement | null = el.querySelector(":scope > .msg-meta");
    if (!host) {
      for (const p of el.querySelectorAll<HTMLElement>(":scope > p")) {
        if (p.firstElementChild?.tagName === "EM" && p.children.length === 1) {
          host = p;
          break;
        }
      }
    }
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "copy-uuid";
    btn.dataset.uuid = uuid;
    btn.title = `Copy section ID (${uuid})`;
    btn.setAttribute("aria-label", "Copy section ID");
    btn.textContent = "🆔";
    if (host) {
      host.append(document.createTextNode(" · "), btn);
    } else {
      // No meta line — drop the button at the top of the section.
      el.prepend(btn);
    }
  }
}

async function onCopyClick(ev: MouseEvent) {
  const btn = (ev.target as HTMLElement | null)?.closest<HTMLButtonElement>(
    "button.copy-uuid",
  );
  if (!btn) return;
  ev.preventDefault();
  ev.stopPropagation();
  const uuid = btn.dataset.uuid || "";
  if (!uuid) return;
  try {
    await navigator.clipboard.writeText(uuid);
    const prev = btn.textContent;
    btn.textContent = "✓";
    btn.classList.add("copied");
    setTimeout(() => {
      btn.textContent = prev;
      btn.classList.remove("copied");
    }, 900);
  } catch {
    btn.classList.add("copy-failed");
    setTimeout(() => btn.classList.remove("copy-failed"), 900);
  }
}

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
  injectCopyUuidButtons();
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
onMounted(() => {
  injectCopyUuidButtons();
  applySelection();
});
</script>

<template>
  <div
    class="chat-body markdown-body"
    ref="root"
    v-html="html"
    @click="onCopyClick"
  ></div>
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
.chat-body button.copy-uuid {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  vertical-align: baseline;
  padding: 0 0.25rem;
  margin: 0;
  background: transparent;
  border: 1px solid transparent;
  border-radius: 4px;
  color: inherit;
  font: inherit;
  line-height: 1;
  cursor: pointer;
  opacity: 0.7;
}
.chat-body button.copy-uuid:hover {
  opacity: 1;
  border-color: var(--fw-muted, #94a3b8);
}
.chat-body button.copy-uuid.copied {
  color: #16a34a;
  opacity: 1;
}
.chat-body button.copy-uuid.copy-failed {
  color: #dc2626;
  opacity: 1;
}
</style>
