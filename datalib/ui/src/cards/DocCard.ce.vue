<script setup lang="ts">
// Document card: fetches the unified_index applet's /chat/{markdownUuid} and renders it via
// ChatBody. Fully determined by its props (which come from the card
// source, e.g. `documentView("md-uuid", "section-uuid")`):
// - navigation (edge clicks, inline /chat/<uuid> links) opens a new
//   card via ctx.host.openCards — never a bus message;
// - hovering an edge source advertises the destination on the bus
//   (`edge.hover`); every doc card subscribes and puts a transient
//   highlight on the target span when the destination is its own doc.
import { computed, onBeforeUnmount, ref, watch } from "vue";
import { fetchChat, type ChatResponse, type DocProblem, type EdgeOut } from "@/api";
import { copyToClipboard } from "@/clipboard";
import ChatBody from "./ChatBody.ce.vue";
import type { RemoteRef } from "./remoteMedia";
import FeedbackButton from "@/components/FeedbackButton.ce.vue";
import FeedbackModal from "@/components/FeedbackModal.vue";
import {
  buildContext,
  capturePreviewSelection,
  messageAncestor,
  type FeedbackContext,
} from "@/feedback/context";
import { chatHrefFromClick, isBrowserClick } from "./chatLink";
import { problemLabel } from "./problems";
import { TOPIC_EDGE_HOVER, type CardCtx, type EdgeHoverPayload } from "./types";

const props = defineProps<{
  ctx: CardCtx;
  // Addresses one rendered `.md` file — the file this card fetches
  // and displays. Same value the applet's `/chat/{markdown_uuid}` takes.
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

// Falsy anchor → "whole-doc destination", don't seed a highlight target.
function edgeSource(edge: EdgeOut): string {
  return docSource(edge.dst_markdown_uuid, edge.dst_anchor_uuid || null);
}

function onOpenEdge(edge: EdgeOut) {
  props.ctx.host.openCards(edgeSource(edge));
}

// The doc-level edge list draws real links: a plain click opens the
// destination beside this card, anything else is the browser's.
function onEdgeLinkClick(ev: MouseEvent, edge: EdgeOut) {
  if (isBrowserClick(ev)) return;
  ev.preventDefault();
  onOpenEdge(edge);
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
    t && props.markdownUuid && t.markdownUuid === props.markdownUuid ? t.sectionUuid : null;
});
onBeforeUnmount(unsubHover);

// Doc-level outgoing edges (whole-doc source) drive the
// "destinations" list at the top of the preview. Span-level edges
// (truthy `src_anchor_uuid`) drive inline clickable highlights
// inside the body and are NOT listed here — they appear in context
// where the user can read what they're navigating from.
const docLevelOutgoing = computed<EdgeOut[]>(() => {
  if (!chat.value) return [];
  return (chat.value.outgoing_edges ?? []).filter((e) => !e.src_anchor_uuid);
});

const chat = ref<ChatResponse | null>(null);
const loading = ref(false);
const error = ref<string | null>(null);

// ── The problems banner: what render could not fully do to this
// document, above the body, errors first. A line about a record that
// survived as a section jumps to it; a dropped record has no section,
// and its line says so instead.
const problems = computed<DocProblem[]>(() => chat.value?.problems ?? []);
const problemErrors = computed<string[]>(() => chat.value?.errors ?? []);
/// The section a banner line was clicked for: overrides the card's own
/// target so the body scrolls and highlights in place rather than
/// opening a second card.
const jumpTo = ref<string | null>(null);
watch(
  () => props.markdownUuid,
  () => {
    jumpTo.value = null;
  },
);

function onProblemJump(p: DocProblem) {
  if (!p.item_uuid) return;
  // Re-set through null so clicking the same line twice scrolls again.
  jumpTo.value = null;
  void Promise.resolve().then(() => {
    jumpTo.value = p.item_uuid;
  });
}

// ── Remote images. The body's references to other hosts are held back
// by the sanitizer (`remoteMedia.ts`); the banner above the body says
// how many and from where, so what the document would have fetched is
// known before it is read. Loading them is issue #648's second half.
const remoteRefs = ref<RemoteRef[]>([]);

/// One entry per distinct URL.
const remoteUnique = computed<RemoteRef[]>(() => {
  const seen = new Set<string>();
  return remoteRefs.value.filter((r) => {
    if (seen.has(r.url)) return false;
    seen.add(r.url);
    return true;
  });
});

/// The hosts referenced, most-referenced first.
const remoteHosts = computed<{ host: string; count: number }[]>(() => {
  const counts = new Map<string, number>();
  for (const r of remoteUnique.value) {
    const host = r.host || r.url;
    counts.set(host, (counts.get(host) ?? 0) + 1);
  }
  return [...counts.entries()]
    .map(([host, count]) => ({ host, count }))
    .sort((a, b) => b.count - a.count || a.host.localeCompare(b.host));
});

/// "remote images", or "remote images and media" when a video or audio
/// element is among them; singular for one.
const remoteNoun = computed(() => {
  const n = remoteUnique.value.length;
  if (remoteRefs.value.some((r) => r.kind === "media")) {
    return n === 1 ? "remote image or media file" : "remote images and media";
  }
  return n === 1 ? "remote image" : "remote images";
});

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
  await copyToClipboard(text);
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
      targetUuids: [t.conv, t.sel.start_message_uuid, t.sel.end_message_uuid].filter(
        (v, i, a) => a.indexOf(v) === i,
      ),
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
    remoteRefs.value = [];
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
    <p v-if="!markdownUuid" class="empty">Select a row to preview the conversation.</p>
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
      <ul v-if="problems.length || problemErrors.length" class="problems">
        <li v-for="e in problemErrors" :key="e" class="problem problem-error">
          <span class="problem-severity">error</span>
          <span class="problem-text">{{ e }}</span>
        </li>
        <li
          v-for="p in problems"
          :key="p.problem_uuid"
          :class="['problem', `problem-${p.severity}`]"
          :data-problem-uuid="p.problem_uuid"
          :title="`${p.stage}: ${p.reason}, first seen ${p.first_seen_at_utc}`"
        >
          <span class="problem-severity">{{ p.severity }}</span>
          <span class="problem-text">
            <a v-if="p.item_uuid" class="problem-jump" href="#" @click.prevent="onProblemJump(p)">{{
              problemLabel(p)
            }}</a>
            <template v-else>{{ problemLabel(p) }}</template>
            <code v-if="p.sample" class="problem-sample">{{ p.sample }}</code>
          </span>
        </li>
      </ul>
      <div v-if="remoteUnique.length" class="remote-banner">
        <span class="remote-banner-text">
          <strong>{{ remoteUnique.length }}</strong> {{ remoteNoun }} not loaded — loading one would
          tell its host you opened this.
        </span>
        <span class="remote-banner-hosts">
          <span v-for="h in remoteHosts" :key="h.host" class="remote-host">
            {{ h.host }}<span v-if="h.count > 1" class="remote-host-count">×{{ h.count }}</span>
          </span>
        </span>
      </div>
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
            :href="ctx.host.hrefFor(edgeSource(e))"
            @click="onEdgeLinkClick($event, e)"
            @mouseenter="onDocLevelHover(e)"
            @mouseleave="onDocLevelLeave"
            :title="e.dst_title ?? e.dst_markdown_uuid"
            >{{ e.label || e.dst_title || e.dst_markdown_uuid }}</a
          >
          <span v-if="e.label && e.dst_title && e.label !== e.dst_title" class="edge-dst-title"
            >({{ e.dst_title }})</span
          >
        </li>
      </ul>
      <div @click="onBodyClick">
        <ChatBody
          :body="chat.body"
          :markdown-uuid="chat.markdown_uuid"
          :selected-section-uuid="jumpTo ?? sectionUuid"
          :outgoing-edges="chat.outgoing_edges"
          :hover-anchor-uuid="hoverAnchor"
          @open-edge="onOpenEdge"
          @hover-edge="onHoverEdge"
          @remote-media="remoteRefs = $event"
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
  /* No padding at the *top*. A sticky message header stops at the
     scrollport's padding edge, while the content behind it keeps
     scrolling up through that padding and stays visible — so a top
     padding shows a sliver of text floating above the pinned bar. The
     gap comes back below as padding on the first child, where it
     scrolls away with the content instead. */
  padding: 0 1rem 0.75rem;
  box-sizing: border-box;
}
.chat-preview > .empty,
.chat-preview > .error,
.chat-header {
  padding-top: 0.75rem;
}
.chat-header h2 {
  margin: 0 0 0.25rem;
  font-size: 1.1rem;
}
.meta {
  font-size: 0.8rem;
  color: var(--datalib-muted);
  margin: 0 0 0.25rem;
}
.empty,
.error {
  color: var(--datalib-muted);
  padding: 1rem;
}
.error {
  color: #e35d6a;
}
.problems {
  /* Above the body and the edges: what render could not do to this
     document is the first thing a reader should see. */
  list-style: none;
  padding: 0;
  margin: 0 0 0.5rem;
  font-size: 0.85rem;
  border-top: 1px solid var(--datalib-border);
  border-bottom: 1px solid var(--datalib-border);
}
.problem {
  display: flex;
  gap: 0.5rem;
  align-items: baseline;
  padding: 0.25rem 0;
}
.problem-severity {
  flex: 0 0 auto;
  font-size: 0.7rem;
  line-height: 1rem;
  padding: 0 0.4rem;
  border-radius: 0.5rem;
  text-transform: uppercase;
  background: color-mix(in srgb, currentColor 12%, transparent);
}
.problem-error .problem-severity {
  color: var(--datalib-log-error);
}
.problem-warning .problem-severity {
  color: var(--datalib-log-warn);
}
.problem-info .problem-severity {
  color: var(--datalib-muted);
}
.problem-text {
  flex: 1 1 auto;
  min-width: 0;
}
.problem-jump {
  color: inherit;
  text-decoration: underline dotted;
}
.problem-sample {
  display: inline-block;
  margin-left: 0.4rem;
  font-size: 0.75rem;
  opacity: 0.8;
  max-width: 100%;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  vertical-align: bottom;
}
/* The remote-images banner: above the body with the problems, since
   what the document would fetch from elsewhere is something to know
   before reading it. */
.remote-banner {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.35rem 0.6rem;
  margin: 0 0 0.5rem;
  padding: 0.4rem 0;
  font-size: 0.8rem;
  border-top: 1px solid var(--datalib-border);
  border-bottom: 1px solid var(--datalib-border);
}
.remote-banner-text {
  flex: 1 1 100%;
}
.remote-banner-hosts {
  display: inline-flex;
  flex-wrap: wrap;
  gap: 0.3rem;
  align-items: center;
}
.remote-host {
  font-family: ui-monospace, Menlo, monospace;
  font-size: 0.75rem;
  line-height: 1.3;
  padding: 0.1rem 0.5rem;
  background: var(--datalib-input-bg, #fff);
  border: 1px solid var(--datalib-border, #d8d8d8);
  border-radius: 999px;
}
.remote-host-count {
  margin-left: 0.3rem;
  color: var(--datalib-muted);
}
.outgoing-edges {
  /* The doc-level outgoing edges list sits above the rendered body
     and below the meta line. List markers off so the leading arrow
     glyph stands in. */
  list-style: none;
  padding: 0;
  margin: 0 0 0.5rem;
  font-size: 0.85rem;
  border-top: 1px solid var(--datalib-border);
  border-bottom: 1px solid var(--datalib-border);
  padding: 0.4rem 0;
}
.outgoing-edges li {
  margin: 0.15rem 0;
}
.edge-arrow {
  color: var(--datalib-muted, #94a3b8);
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
  text-decoration-color: var(--datalib-muted, #94a3b8);
  text-underline-offset: 2px;
  border-radius: 3px;
  transition: background-color 100ms ease-in-out;
}
.edge-source-link:hover {
  background: rgba(99, 102, 241, 0.28);
}
.edge-dst-title {
  color: var(--datalib-muted, #94a3b8);
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
  background: var(--datalib-input-bg, #fff);
  color: var(--datalib-fg, #000);
  border: 1px solid var(--datalib-border, #ccc);
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
  background: var(--datalib-accent, #eee);
}
.ctx-divider {
  height: 1px;
  background: var(--datalib-border, #ccc);
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
  /* Fixed dark, not `--datalib-code-bg`: the highlight theme we inject
     is github-*dark* and does not switch with the app's, so painting a
     code block on the light-mode token left dark-theme token colors on
     a near-white background. Inline `code` below is unhighlighted and
     does follow the theme. */
  background: #0d1117;
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
  background: var(--datalib-code-bg, #f0f0f0);
  padding: 0 0.25rem;
  border-radius: 2px;
}
.markdown-body details {
  margin: 0.4rem 0;
  padding: 0.25rem 0.5rem;
  border: 1px solid var(--datalib-border);
  border-radius: 4px;
  background: var(--datalib-card-bg);
}
.markdown-body details > summary {
  cursor: pointer;
  font-size: 0.85rem;
  color: var(--datalib-muted);
}
.markdown-body details[open] > summary {
  margin-bottom: 0.4rem;
}
.markdown-body img {
  max-width: 100%;
  max-height: 60vh;
  width: auto;
  height: auto;
}
.markdown-body blockquote {
  border-left: 3px solid var(--datalib-border);
  margin: 0.5rem 0;
  padding-left: 0.75rem;
  color: var(--datalib-muted);
}
</style>
