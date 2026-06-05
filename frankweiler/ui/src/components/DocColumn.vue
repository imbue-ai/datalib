<script setup lang="ts">
import { computed, ref, watch } from "vue";
import { RouterLink } from "vue-router";
import { fetchChat, type ChatResponse, type EdgeOut } from "@/api";
import ChatBody from "./ChatBody.vue";
import FeedbackButton from "./FeedbackButton.vue";
import FeedbackModal from "./FeedbackModal.vue";
import {
  buildContext,
  capturePreviewSelection,
  messageAncestor,
  type FeedbackContext,
} from "@/feedback/context";
import { chatHrefFromClick } from "@/router/chat_link";
import { encodeStack } from "@/router/columns";

const props = defineProps<{
  // Addresses one rendered `.md` file — the file the preview pane
  // should fetch and display. Same value `/api/chat/{markdown_uuid}`
  // takes.
  markdownUuid: string | null;
  // The clicked grid row's uuid (= the message UUID for message rows,
  // the prefixed `tu-…`/`tr-…`/`th-…` for block rows, null for
  // conversation-level rows). The renderer emits each section with a
  // matching `data-section-uuid` attribute, so the UI just needs to
  // forward this prop and ChatBody does an exact-string lookup.
  selectedSectionUuid: string | null;
  /**
   * When the user hovers an edge-source elsewhere whose destination
   * is THIS doc as a whole (no anchor), the parent passes true so
   * we can outline the whole column. Distinct from `hoverAnchor`,
   * which lights up an inline span instead.
   */
  isHoverTarget?: boolean;
  /**
   * Anchor uuid inside this doc to highlight as the in-flight edge
   * hover destination. Null when no hover is active or the hovered
   * edge's destination is in a different column.
   */
  hoverAnchor?: string | null;
}>();

// Miller columns: when the body contains a markdown link to another
// chat (`href="/chat/<uuid>"`), intercept the click and emit so the
// parent MillerView can push a new column to the right instead of
// performing a full-page navigation. `open-chat` carries an optional
// anchor so edge-driven navigation (with a known destination span)
// scrolls + highlights on the other side; grid-driven navigation
// passes anchor=null because the grid row's `uuid` already drives
// selection.
const emit = defineEmits<{
  (e: "open-chat", markdownUuid: string, anchor: string | null): void;
  /**
   * Forwarded from `ChatBody` (for inline span sources) and from the
   * doc-level destinations list (for whole-doc sources). MillerView
   * uses the payload to decide which sibling column to outline /
   * span-highlight while the hover is active. Null on hover-out.
   */
  (
    e: "hover-edge",
    target: { md: string; anchor: string | null } | null,
  ): void;
}>();

function onBodyClick(ev: MouseEvent) {
  const uuid = chatHrefFromClick(ev);
  if (!uuid) return;
  ev.preventDefault();
  emit("open-chat", uuid, null);
}

function onOpenEdge(edge: EdgeOut) {
  // Falsy → "whole-doc destination", don't seed a highlight target.
  // See the `docLevelOutgoing` comment for the null/"" tolerance
  // rationale.
  emit("open-chat", edge.dst_markdown_uuid, edge.dst_anchor_uuid || null);
}

function onHoverEdge(target: { md: string; anchor: string | null } | null) {
  emit("hover-edge", target);
}

function onDocLevelHover(edge: EdgeOut) {
  emit("hover-edge", {
    md: edge.dst_markdown_uuid,
    anchor: edge.dst_anchor_uuid || null,
  });
}

function onDocLevelLeave() {
  emit("hover-edge", null);
}

// Doc-level outgoing edges (whole-doc source) drive the
// "destinations" list at the top of the preview. Span-level edges
// (truthy `src_anchor_uuid`) drive inline clickable highlights
// inside the body and are NOT listed here — they appear in context
// where the user can read what they're navigating from.
//
// We treat both `null` and `""` as "whole doc": the backend's SQL
// representation of a missing anchor can land as either depending
// on the SQLite driver path, and the UI doesn't care which.
const docLevelOutgoing = computed<EdgeOut[]>(() => {
  if (!chat.value) return [];
  return (chat.value.outgoing_edges ?? []).filter((e) => !e.src_anchor_uuid);
});

const chat = ref<ChatResponse | null>(null);
const loading = ref(false);
const error = ref<string | null>(null);

const feedbackOpen = ref(false);
const feedbackContext = ref<FeedbackContext | null>(null);
const feedbackSurfaceLabel = ref("");

// Right-click context menu state. We defer building the feedback
// context until the user actually picks "Feedback…", but the surface
// kind ('selection' / 'message' / 'conversation') is decided up-front
// at right-click time so the Copy and Feedback actions agree on what
// the user was pointing at.
type PendingTarget =
  | {
      kind: "selection";
      anchor: Element | null;
      conv: string;
      sel: ReturnType<typeof capturePreviewSelection>;
      selectionText: string;
    }
  | {
      kind: "message";
      anchor: Element | null;
      conv: string;
      msgUuid: string;
      msgIndex: number;
    }
  | {
      kind: "conversation";
      anchor: Element | null;
      conv: string;
    };

const ctxMenuVisible = ref(false);
const ctxMenuPos = ref({ x: 0, y: 0 });
const ctxTarget = ref<PendingTarget | null>(null);

function onPaneContextMenu(ev: MouseEvent) {
  if (!chat.value) return;
  const conv = chat.value.markdown_uuid;
  const target = ev.target instanceof Element ? ev.target : null;

  // Cascade: active selection > message under cursor > whole-page fallback.
  // Whichever path we take, we then open our custom context menu instead
  // of jumping straight into the feedback modal.
  let pending: PendingTarget;
  const sel = capturePreviewSelection();
  if (sel) {
    pending = {
      kind: "selection",
      anchor: target,
      conv,
      sel,
      selectionText: window.getSelection()?.toString() ?? "",
    };
  } else {
    const msgUuid = messageAncestor(target);
    if (msgUuid) {
      // The feedback schema's preview_message payload still has a
      // required `message_index` (see schemas/feedback.schema.json).
      // The renderer no longer emits `data-msg-index` — `message_uuid`
      // is the load-bearing field now. Pass 0 until the feedback
      // schema follow-up drops the index.
      pending = {
        kind: "message",
        anchor: target,
        conv,
        msgUuid,
        msgIndex: 0,
      };
    } else {
      pending = { kind: "conversation", anchor: target, conv };
    }
  }

  ev.preventDefault();
  ctxTarget.value = pending;
  ctxMenuPos.value = { x: ev.clientX, y: ev.clientY };
  ctxMenuVisible.value = true;
}

function closeCtxMenu() {
  ctxMenuVisible.value = false;
  ctxTarget.value = null;
}

const copyLabel = computed(() => {
  const t = ctxTarget.value;
  if (!t) return "Copy";
  if (t.kind === "selection") return "Copy selected text";
  if (t.kind === "message") return "Copy message ID";
  return "Copy conversation ID";
});

function copyTargetText(): string {
  const t = ctxTarget.value;
  if (!t) return "";
  if (t.kind === "selection") return t.selectionText;
  if (t.kind === "message") return t.msgUuid;
  return t.conv;
}

async function onCopy() {
  const text = copyTargetText();
  if (!text) {
    closeCtxMenu();
    return;
  }
  try {
    await navigator.clipboard.writeText(text);
  } catch {
    // Fallback for non-secure contexts.
    const ta = document.createElement("textarea");
    ta.value = text;
    document.body.appendChild(ta);
    ta.select();
    document.execCommand("copy");
    document.body.removeChild(ta);
  }
  closeCtxMenu();
}

function onFeedback() {
  const t = ctxTarget.value;
  if (!t) {
    closeCtxMenu();
    return;
  }
  if (t.kind === "selection" && t.sel) {
    feedbackContext.value = buildContext({
      surface: "preview_selection",
      anchor: t.anchor,
      targetUuids: [
        t.conv,
        t.sel.start_message_uuid,
        t.sel.end_message_uuid,
      ].filter((v, i, a) => a.indexOf(v) === i),
      payload: t.sel,
    });
    feedbackSurfaceLabel.value = "Selected text";
  } else if (t.kind === "message") {
    feedbackContext.value = buildContext({
      surface: "preview_message",
      anchor: t.anchor,
      targetUuids: [t.conv, t.msgUuid],
      payload: {
        conversation_uuid: t.conv,
        message_uuid: t.msgUuid,
        message_index: t.msgIndex,
      },
    });
    feedbackSurfaceLabel.value = "Chat message";
  } else {
    feedbackContext.value = buildContext({
      surface: "page_header",
      anchor: t.anchor,
      targetUuids: [t.conv],
      payload: { entity_kind: "conversation", entity_uuid: t.conv },
    });
    feedbackSurfaceLabel.value = "Conversation";
  }
  feedbackOpen.value = true;
  closeCtxMenu();
}

watch(
  // One UUID per rendered file — when `markdownUuid` changes, refetch
  // the file. No row-uuid disambiguation needed: provider-specific
  // sharding (beeper's per-period files) is already encoded in the
  // markdown_uuid the parent passes.
  () => props.markdownUuid,
  async (uuid) => {
    if (!uuid) {
      chat.value = null;
      return;
    }
    loading.value = true;
    error.value = null;
    try {
      chat.value = await fetchChat(uuid);
    } catch (e) {
      error.value = (e as Error).message;
    } finally {
      loading.value = false;
    }
  },
  { immediate: true },
);
</script>

<template>
  <section
    class="chat-preview"
    :class="{ 'is-hover-target': isHoverTarget }"
    :data-markdown-uuid="chat?.markdown_uuid ?? null"
    @contextmenu="onPaneContextMenu"
  >
    <p v-if="!markdownUuid" class="empty">
      Select a row to preview the conversation.
    </p>
    <p v-else-if="loading && !chat" class="empty">loading…</p>
    <p v-else-if="error" class="error">error: {{ error }}</p>
    <template v-else-if="chat">
      <header class="chat-header">
        <!-- Title block (with copy-id button and source-URL arrow) is
             rendered inline at the top of the body by the cross-provider
             `Title` helper. The header here only carries the
             non-title chrome: feedback button, "view this column alone"
             link, and timestamps. -->
        <p class="meta">
          <FeedbackButton
            :entity-uuid="chat.markdown_uuid"
            entity-kind="conversation"
            label="Conversation"
          />
          ·
          <!-- Standalone view: opens this single column on its own
               page in a new tab. The URL is a one-column stack
               (`/doc:<uuid>`), which the same MillerView renders. -->
          <RouterLink
            :to="
              encodeStack([{ kind: 'doc', md: chat.markdown_uuid }])
            "
            target="_blank"
            rel="noopener"
            >view this column alone ↗</RouterLink
          >
          <span v-if="chat.created_at"> · {{ chat.created_at }}</span>
        </p>
      </header>
      <ul v-if="docLevelOutgoing.length" class="outgoing-edges">
        <li v-for="e in docLevelOutgoing" :key="e.edge_uuid">
          <span class="edge-arrow" aria-hidden="true">→</span>
          <!-- Producers should set `label` to the human-readable
               handle they want shown in the list (e.g. "Greek" /
               "English" for perseus' cross-language edges). When
               absent, we fall back to the destination doc's title
               and finally the bare uuid. When BOTH label and title
               are set we show "label (title)" — label first, since
               that's what the producer chose to lead with, with the
               title in parens as supplementary context.

               Hover handlers mirror ChatBody's inline-span behavior:
               while the cursor sits on the link, MillerView gets a
               `hover-edge` so it can outline / span-highlight the
               destination column. -->
          <a
            class="edge-source-link"
            :href="`/#/chat/${e.dst_markdown_uuid}`"
            @click.prevent="onOpenEdge(e)"
            @mouseenter="onDocLevelHover(e)"
            @mouseleave="onDocLevelLeave"
            :title="e.dst_title ?? e.dst_markdown_uuid"
            >{{ e.label || e.dst_title || e.dst_markdown_uuid }}</a
          >
          <span
            v-if="e.label && e.dst_title && e.label !== e.dst_title"
            class="edge-dst-title"
            >({{ e.dst_title }})</span
          >
        </li>
      </ul>
      <div @click="onBodyClick">
        <ChatBody
          :body="chat.body"
          :markdown-uuid="chat.markdown_uuid"
          :selected-section-uuid="selectedSectionUuid"
          :outgoing-edges="chat.outgoing_edges"
          :hover-anchor-uuid="hoverAnchor ?? null"
          @open-edge="onOpenEdge"
          @hover-edge="onHoverEdge"
        />
      </div>
    </template>
    <FeedbackModal
      :open="feedbackOpen"
      :surface-label="feedbackSurfaceLabel"
      :context="feedbackContext"
      @close="feedbackOpen = false"
    />
    <div
      v-if="ctxMenuVisible"
      class="ctx-overlay"
      @click="closeCtxMenu"
      @contextmenu.prevent="closeCtxMenu"
    >
      <div
        class="ctx-menu"
        :style="{ top: ctxMenuPos.y + 'px', left: ctxMenuPos.x + 'px' }"
        @click.stop
      >
        <div class="ctx-item" @click="onCopy">{{ copyLabel }}</div>
        <div class="ctx-divider" />
        <div class="ctx-item" @click="onFeedback">Feedback…</div>
      </div>
    </div>
  </section>
</template>

<style scoped>
.chat-preview {
  height: 100%;
  overflow-y: auto;
  padding: 0.75rem 1rem;
  box-sizing: border-box;
  /* Reserve the border width so the hover state doesn't reflow the
     column when it gets outlined. `outline` would skip layout but
     also fail to track the rounded scroll container's edges; a
     transparent border that swaps color on `.is-hover-target` is
     visually cleaner. */
  border: 2px solid transparent;
}
.chat-preview.is-hover-target {
  border-color: rgba(99, 102, 241, 0.6);
  border-radius: 4px;
}
.chat-header h2 {
  margin: 0 0 0.25rem;
  font-size: 1.1rem;
}
.meta {
  font-size: 0.8rem;
  color: var(--fw-muted);
  margin: 0 0 0.25rem;
}
.empty,
.error {
  color: var(--fw-muted);
  padding: 1rem;
}
.error {
  color: #e35d6a;
}
.outgoing-edges {
  /* The doc-level outgoing edges list sits above the rendered body
     and below the meta line. List markers off so the leading arrow
     glyph stands in. */
  list-style: none;
  padding: 0;
  margin: 0 0 0.5rem;
  font-size: 0.85rem;
  border-top: 1px solid var(--fw-border);
  border-bottom: 1px solid var(--fw-border);
  padding: 0.4rem 0;
}
.outgoing-edges li {
  margin: 0.15rem 0;
}
.edge-arrow {
  color: var(--fw-muted, #94a3b8);
  margin-right: 0.4rem;
}
.edge-source-link {
  /* Match the inline span styling in `ChatBody.vue`: dotted muted
     underline so it reads as a link without the "external blue"
     baggage, and the same hover fill so source and destination
     (lit up via `.is-hover-target` / `.hover-dst`) share a color. */
  color: inherit;
  text-decoration: underline;
  text-decoration-style: dotted;
  text-decoration-color: var(--fw-muted, #94a3b8);
  text-underline-offset: 2px;
  border-radius: 3px;
  transition: background-color 100ms ease-in-out;
}
.edge-source-link:hover {
  background: rgba(99, 102, 241, 0.28);
}
.edge-dst-title {
  color: var(--fw-muted, #94a3b8);
  margin-left: 0.4rem;
  font-size: 0.8rem;
}
.ctx-overlay {
  position: fixed;
  inset: 0;
  z-index: 1500;
  background: transparent;
}
.ctx-menu {
  position: fixed;
  background: var(--fw-input-bg, #fff);
  color: var(--fw-fg, #000);
  border: 1px solid var(--fw-border, #ccc);
  border-radius: 4px;
  box-shadow: 0 2px 10px rgba(0, 0, 0, 0.2);
  min-width: 180px;
  padding: 4px 0;
  z-index: 1501;
  font-size: 14px;
}
.ctx-item {
  padding: 8px 16px;
  cursor: pointer;
  user-select: none;
}
.ctx-item:hover {
  background: var(--fw-accent, #eee);
}
.ctx-divider {
  height: 1px;
  background: var(--fw-border, #ccc);
  margin: 4px 0;
}
</style>

<style>
/* Global markdown styling — :deep() doesn't reach v-html descendants
   reliably across Vue versions, so scope by class instead. */
.markdown-body {
  font-size: 0.9rem;
  line-height: 1.45;
}
.markdown-body p {
  margin: 0.4rem 0;
}
.markdown-body pre {
  background: var(--fw-code-bg, #0d1117);
  color: #e6edf3;
  padding: 0.6rem 0.75rem;
  border-radius: 4px;
  overflow-x: auto;
  font-size: 0.82rem;
}
.markdown-body code {
  font-family: ui-monospace, Menlo, monospace;
  font-size: 0.85em;
}
.markdown-body :not(pre) > code {
  background: var(--fw-code-bg, #f0f0f0);
  padding: 0 0.25rem;
  border-radius: 2px;
}
.markdown-body details {
  margin: 0.4rem 0;
  padding: 0.25rem 0.5rem;
  border: 1px solid var(--fw-border);
  border-radius: 4px;
  background: var(--fw-card-bg);
}
.markdown-body details > summary {
  cursor: pointer;
  font-size: 0.85rem;
  color: var(--fw-muted);
}
.markdown-body details[open] > summary {
  margin-bottom: 0.4rem;
}
.markdown-body blockquote {
  border-left: 3px solid var(--fw-border);
  margin: 0.5rem 0;
  padding-left: 0.75rem;
  color: var(--fw-muted);
}
</style>
