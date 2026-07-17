<script setup lang="ts">
// Document card: fetches /api/chat/{markdownUuid} and renders it via
// ChatBody. Fully determined by its props (which come from the card
// source, e.g. `documentView("md-uuid", "section-uuid")`):
// - navigation (edge clicks, inline /chat/<uuid> links) opens a new
//   card via ctx.host.openCards — never a bus message;
// - hovering an edge source advertises the destination on the bus
//   (`edge.hover`); every doc card subscribes and puts a transient
//   highlight on the target span when the destination is its own doc.
import { computed, onBeforeUnmount, ref, watch } from "vue";
import { fetchChat, type ChatResponse, type EdgeOut } from "@/api";
import ChatBody from "./ChatBody.ce.vue";
import FeedbackButton from "@/components/FeedbackButton.ce.vue";
import FeedbackModal from "@/components/FeedbackModal.vue";
import {
  buildContext,
  capturePreviewSelection,
  messageAncestor,
  type FeedbackContext,
} from "@/feedback/context";
import { chatHrefFromClick } from "./chatLink";
import {
  TOPIC_EDGE_HOVER,
  type CardCtx,
  type EdgeHoverPayload,
} from "./types";

const props = defineProps<{
  ctx: CardCtx;
  // Addresses one rendered `.md` file — the file this card fetches
  // and displays. Same value `/api/chat/{markdown_uuid}` takes.
  markdownUuid: string | null;
  // Section uuid inside the doc to highlight and scroll to. Matches
  // the renderer-emitted `data-section-uuid` attributes.
  sectionUuid: string | null;
}>();

function docSource(md: string, anchor: string | null): string {
  const args = [md, anchor].map((a) => JSON.stringify(a)).join(", ");
  return `documentView(${args})`;
}

function openDoc(md: string, anchor: string | null) {
  props.ctx.host.openCards(docSource(md, anchor));
}

function onBodyClick(ev: MouseEvent) {
  const uuid = chatHrefFromClick(ev);
  if (!uuid) return;
  ev.preventDefault();
  openDoc(uuid, null);
}

function onOpenEdge(edge: EdgeOut) {
  // Falsy → "whole-doc destination", don't seed a highlight target.
  openDoc(edge.dst_markdown_uuid, edge.dst_anchor_uuid || null);
}

function publishHover(target: { md: string; anchor: string | null } | null) {
  const payload: EdgeHoverPayload = target
    ? { markdownUuid: target.md, sectionUuid: target.anchor }
    : null;
  props.ctx.bus.publish(TOPIC_EDGE_HOVER, payload, {
    from: props.ctx.cardId,
  });
}

function onHoverEdge(target: { md: string; anchor: string | null } | null) {
  publishHover(target);
}

function onDocLevelHover(edge: EdgeOut) {
  publishHover({
    md: edge.dst_markdown_uuid,
    anchor: edge.dst_anchor_uuid || null,
  });
}

function onDocLevelLeave() {
  publishHover(null);
}

// Incoming edge-hover highlight: any doc card (including this one)
// may advertise a hovered edge; when its destination anchor lives in
// our doc, light the span up. Whole-doc destinations (null anchor)
// are deliberately not surfaced.
const hoverAnchor = ref<string | null>(null);

function isEdgeHoverTarget(p: unknown): p is NonNullable<EdgeHoverPayload> {
  if (!p || typeof p !== "object") return false;
  const o = p as Record<string, unknown>;
  return typeof o.markdownUuid === "string" && "sectionUuid" in o;
}

const unsubHover = props.ctx.bus.subscribe(TOPIC_EDGE_HOVER, (payload) => {
  const t = isEdgeHoverTarget(payload) ? payload : null;
  hoverAnchor.value =
    t && props.markdownUuid && t.markdownUuid === props.markdownUuid
      ? t.sectionUuid
      : null;
});
onBeforeUnmount(unsubHover);

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

// Chrome title: generic while nothing is loaded (the uuid means
// nothing to a human), the document's own name once the fetch lands.
watch(
  () => chat.value?.name,
  (name) => props.ctx.setTitle(name || "Document"),
  { immediate: true },
);
</script>

<template>
  <section
    class="chat-preview"
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
             non-title chrome: feedback button and timestamps. The
             "open this column alone" affordance lives in the host's
             column chrome. -->
        <p class="meta">
          <FeedbackButton
            :entity-uuid="chat.markdown_uuid"
            entity-kind="conversation"
            label="Conversation"
          />
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
               title in parens as supplementary context. -->
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
          :selected-section-uuid="sectionUuid"
          :outgoing-edges="chat.outgoing_edges"
          :hover-anchor-uuid="hoverAnchor"
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
  /* Match the inline span styling in `ChatBody.ce.vue`: dotted muted
     underline so it reads as a link without the "external blue"
     baggage, and the same hover fill so source and destination
     (lit up via `.hover-dst`) share a color. */
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
/* Markdown styling for the v-html body. Unscoped so the rules reach
   inside `v-html`; still shadow-local since this lands in the card's
   shadow root. */
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
