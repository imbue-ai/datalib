<script setup lang="ts">
// Renders a chat conversation. The backend serves the QMD body verbatim
// (CommonMark + per-section `<div id="m-{uuid}" data-section-uuid="…">`
// wrappers emitted by the ingest renderer — one wrapper per message,
// plus nested ones for tool_use / tool_result / thinking blocks); we
// run markdown-it once.
//
// `selectedSectionUuid` picks which section to scroll to and visually
// highlight via the `.msg.selected` CSS rule. The value must match the
// grid row's `uuid` exactly — for messages that's the message UUID;
// for block rows it's the prefixed form (`tu-…`/`tr-…`/`th-…`) the
// renderer emits.

import { ref, computed, watch, nextTick, onMounted } from "vue";
import MarkdownIt from "markdown-it";
import hljs from "highlight.js";
import "highlight.js/styles/github-dark.css";

const props = defineProps<{
  body: string;
  selectedSectionUuid?: string | null;
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
  // Scan every section the renderer marks with `data-section-uuid`:
  // top-level message wrappers AND the nested block sections we emit
  // for tool_use / tool_result / thinking. The uuid the button copies
  // is the attribute value as-is (prefixed `tu-`/`tr-`/`th-` for
  // blocks, bare for messages) — that's the form the grid row carries
  // and the deeplink consumes, so "copy section ID" round-trips.
  for (const el of root.value.querySelectorAll<HTMLElement>("[data-section-uuid]")) {
    if (el.querySelector(":scope > .msg-meta .copy-uuid, :scope > p .copy-uuid, :scope > .copy-uuid"))
      continue;
    const uuid = el.getAttribute("data-section-uuid") ?? "";
    if (!uuid) continue;
    // Prefer the explicit `.msg-meta` div (slack). Otherwise use the first
    // <p><em>…</em></p> emitted as the markdown italic meta line
    // (github/gitlab/anthropic/openai). Block sections rarely have
    // either — they fall through to a header-position button.
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
  // Page-title H1 emitted by the Rust `Title` helper: walk
  // `[data-page-title-uuid]` and append a copy-id button at the end
  // of the H1, after the source-link arrow if present. Same button
  // styling + clipboard handler as the section buttons above.
  for (const el of root.value.querySelectorAll<HTMLElement>(
    "[data-page-title-uuid]",
  )) {
    if (el.querySelector(":scope > button.copy-uuid")) continue;
    const uuid = el.getAttribute("data-page-title-uuid") ?? "";
    if (!uuid) continue;
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "copy-uuid";
    btn.dataset.uuid = uuid;
    btn.title = `Copy page ID (${uuid})`;
    btn.setAttribute("aria-label", "Copy page ID");
    btn.textContent = "🆔";
    el.append(document.createTextNode(" "), btn);
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
  if (!props.selectedSectionUuid) return;
  // Attribute-selector lookup avoids the CSS-id escaping minefield —
  // tool_use block ids look like `tu-toolu_01ABC…`, which is a valid
  // HTML id but not a valid bare CSS selector (the digit-prefixed
  // chunks need escaping). `[data-section-uuid="…"]` keeps the
  // matching pure string equality.
  const target = root.value.querySelector<HTMLElement>(
    `[data-section-uuid="${props.selectedSectionUuid.replace(/"/g, '\\"')}"]`,
  );
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
  () => props.selectedSectionUuid,
  async () => {
    // nextTick guards against a parent setting the prop in the same
    // tick that it loads a new conversation: we want the v-html patch
    // to land before we look for `[data-section-uuid]`.
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
/* Per-block sections (tool_use / tool_result / thinking). Nested
   inside their parent message wrapper, so we keep them visually
   subordinate: thinner left border, lighter accent. The selection
   outline below picks the same accent so the highlight still pops. */
.chat-body .msg--block {
  border-left-width: 2px;
  margin: 0.35rem 0;
  padding: 0.35rem 0.6rem;
}
.chat-body .msg--tool-use {
  border-left-color: #a78bfa;
}
.chat-body .msg--tool-result {
  border-left-color: #c4b5fd;
}
.chat-body .msg--thinking {
  border-left-color: #94a3b8;
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
