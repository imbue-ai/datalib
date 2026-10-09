<script setup lang="ts">
// Document card: fetches the unified_index applet's /chat/{markdownUuid} and renders it via
// ChatBody. Fully determined by its props (which come from the card
// source, e.g. `documentView("md-uuid", "section-uuid")`):
// - navigation (edge clicks, inline /chat/<uuid> links) opens a new
//   card via ctx.host.openCards — never a bus message;
// - hovering an edge source advertises the destination on the bus
//   (`edge.hover`); every doc card subscribes and puts a transient
//   highlight on the target span when the destination is its own doc.
import { computed, onBeforeUnmount, onMounted, ref, watch } from "vue";
import {
  REMOTE_ALLOW_TABLE,
  REMOTE_FETCHED_TABLE,
  type AllowScope,
  type ChatResponse,
  type DocProblem,
  type EdgeOut,
  type RemoteAllow,
  type RemoteContext,
} from "@/api";
import { useApi } from "@/cards/cardApi";
import { copyToClipboard } from "@/clipboard";
import { oneAtATime, subscribeLive } from "@/live";
import ChatBody from "./ChatBody.ce.vue";
import { absoluteRemote, type RemoteRef } from "./remoteMedia";
import { renderDocument } from "./renderDocument";
import FeedbackButton from "@/components/FeedbackButton.ce.vue";
import FeedbackModal from "@/components/FeedbackModal.vue";
import {
  buildContext,
  capturePreviewSelection,
  messageAncestor,
  type FeedbackContext,
} from "@/feedback/context";
import { chatUuidFromHref, isBrowserClick, type ClickedLink } from "./chatLink";
import { asElement } from "./docFrame";
import { openExternal } from "@/externalLinks";
import { problemLabel } from "./problems";
import { TOPIC_EDGE_HOVER, type CardCtx, type EdgeHoverPayload } from "./types";

const { allowRemote, checkRemote, fetchChat, forgetRemoteAllow } = useApi();

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

/// A link clicked in the document frame, which never navigates itself:
/// another document opens beside this card, a page of this app in a
/// tab, anything else in the browser the person uses.
function onFrameLink(link: ClickedLink) {
  const uuid = chatUuidFromHref(link.href);
  if (uuid && !link.browserClick) {
    openDoc(uuid, null);
    return;
  }
  const url = uuid
    ? new URL(props.ctx.host.hrefFor(docSource(uuid, null)), location.href).href
    : link.resolved;
  if (new URL(url).origin === location.origin) window.open(url, "_blank", "noopener");
  else void openExternal(url);
}

/// A chip asked for everything from its person: a search card on that
/// query, beside this one.
function onOpenSearch(q: string) {
  props.ctx.host.openCards(`searchView(${JSON.stringify({ q })})`);
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
// by the sanitizer unless the server says an allow row lets them
// through (`remoteMedia.ts`): the card renders the body once without
// mounting it to learn the references, asks `/api/remote_media/check`,
// and only then shows the document, so the first paint is already the
// server's answer. The banner says how many are held and from where,
// offers to let them load — one, a host's worth, this document's,
// this source's — and names the rows that let the rest load, each
// deletable. A decision is a row in the server's store; after each
// write the card asks again and the body re-renders, so nothing here
// touches the DOM.
const remoteRefs = ref<RemoteRef[]>([]);
/// The server's answer: the row covering each reference it lets load,
/// keyed by the URL as the body wrote it.
const remoteCovered = ref<Map<string, RemoteAllow>>(new Map());
const remoteBusy = ref(false);
const remoteError = ref<string | null>(null);

function contextOf(doc: ChatResponse): RemoteContext {
  return { document: doc.markdown_uuid, source: doc.source_ref?.id ?? null };
}
const remoteContext = computed<RemoteContext>(() =>
  chat.value ? contextOf(chat.value) : { document: null, source: null },
);

/// Every remote URL a body references, as written, each once.
function remoteUrlsOf(doc: ChatResponse): string[] {
  const seen = new Set<string>();
  return renderDocument(doc.body, doc.markdown_uuid)
    .remote.map((r) => r.url)
    .filter((u) => !seen.has(u) && (seen.add(u), true));
}

/// A new function per answer, which is what makes the body re-render
/// under it.
const remoteAccept = computed(() => {
  const covered = remoteCovered.value;
  return (url: string) => covered.has(url);
});

/// One entry per distinct URL.
const remoteUnique = computed<RemoteRef[]>(() => {
  const seen = new Set<string>();
  return remoteRefs.value.filter((r) => {
    if (seen.has(r.url)) return false;
    seen.add(r.url);
    return true;
  });
});
const remotePending = computed(() => remoteUnique.value.filter((r) => !r.loaded));

/// The hosts still held, most-referenced first.
const remoteHosts = computed<{ host: string; count: number }[]>(() => {
  const counts = new Map<string, number>();
  for (const r of remotePending.value) {
    const host = r.host || r.url;
    counts.set(host, (counts.get(host) ?? 0) + 1);
  }
  return [...counts.entries()]
    .map(([host, count]) => ({ host, count }))
    .sort((a, b) => b.count - a.count || a.host.localeCompare(b.host));
});

/// The rows letting this document's references load, each once.
const remoteRules = computed<RemoteAllow[]>(() => {
  const seen = new Set<string>();
  const rules: RemoteAllow[] = [];
  for (const rule of remoteCovered.value.values()) {
    if (!seen.has(rule.allow_uuid)) {
      seen.add(rule.allow_uuid);
      rules.push(rule);
    }
  }
  return rules;
});

/// "remote images", or "remote images and media" when a video or audio
/// element is among them; singular for one.
function remoteNoun(n: number): string {
  if (remoteRefs.value.some((r) => r.kind === "media")) {
    return n === 1 ? "remote image or media file" : "remote images and media";
  }
  return n === 1 ? "remote image" : "remote images";
}

function ruleLabel(rule: RemoteAllow): string {
  switch (rule.scope) {
    case "source":
      return `everything from ${chat.value?.source_ref?.label ?? rule.key}`;
    case "document":
      return "everything in this document";
    case "host":
      return `everything on ${rule.key}`;
    default:
      return rule.key;
  }
}

async function remoteWrite(what: () => Promise<unknown>) {
  const doc = chat.value;
  if (!doc || remoteBusy.value) return;
  remoteBusy.value = true;
  remoteError.value = null;
  try {
    await what();
    remoteCovered.value = await checkRemote(contextOf(doc), remoteUrlsOf(doc));
  } catch (e) {
    remoteError.value = (e as Error).message;
  } finally {
    remoteBusy.value = false;
  }
}

function allow(scope: AllowScope, key: string | null) {
  if (!key) return;
  // A `url` row names the URL as the server will see it.
  const named = scope === "url" ? absoluteRemote(key) : key;
  void remoteWrite(() => allowRemote(scope, named));
}

function forget(rule: RemoteAllow) {
  void remoteWrite(() => forgetRemoteAllow(rule.allow_uuid));
}

function openRemoteTable(url: string, title: string) {
  props.ctx.host.openCards(`tableView(${JSON.stringify({ url, title })})`);
}

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

/// Where a right-click happened: this window, or the document frame —
/// whose selection it is, and how far its viewport sits from this one.
type ClickView = { win: Window; dx: number; dy: number };

function onPaneContextMenu(ev: MouseEvent, view: ClickView = { win: window, dx: 0, dy: 0 }) {
  if (!chat.value) return;
  const conv = chat.value.markdown_uuid;
  const target = asElement(ev.target);

  // Cascade: active selection > message under cursor > whole-page fallback.
  // Whichever path we take, we then open our custom context menu instead
  // of jumping straight into the feedback modal.
  let pending: PendingTarget;
  const sel = capturePreviewSelection(view.win);
  if (sel) {
    pending = {
      kind: "selection",
      anchor: target,
      conv,
      sel,
      selectionText: view.win.getSelection()?.toString() ?? "",
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
  ctxMenuPos.value = { x: ev.clientX + view.dx, y: ev.clientY + view.dy };
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
    remoteError.value = null;
    try {
      // The server's answer before the document shows, so the first
      // render is already under it: a placeholder that then vanished
      // would read as a flicker, and a held image that then loaded as
      // a leak.
      const doc = await fetchChat(uuid);
      remoteCovered.value = await checkRemote(contextOf(doc), remoteUrlsOf(doc));
      chat.value = doc;
    } catch (e) {
      error.value = (e as Error).message;
    } finally {
      loading.value = false;
    }
  },
  { immediate: true },
);

// The body and the problems banner are both read from the index, so an
// open card asks again when the index commits. It redraws only when the
// answer differs: every source's commit moves the index, and a redraw
// for nothing would throw away the reader's place.
const cardEl = ref<HTMLElement | null>(null);
const refresh = oneAtATime(async () => {
  const uuid = props.markdownUuid;
  if (!uuid || !chat.value || loading.value) return;
  try {
    const doc = await fetchChat(uuid);
    if (uuid !== props.markdownUuid) return;
    error.value = null;
    if (JSON.stringify(doc) === JSON.stringify(chat.value)) return;
    const covered = await checkRemote(contextOf(doc), remoteUrlsOf(doc));
    if (uuid !== props.markdownUuid) return;
    remoteCovered.value = covered;
    chat.value = doc;
  } catch (e) {
    if (uuid === props.markdownUuid) error.value = (e as Error).message;
  }
});
let unsubscribeLive: (() => void) | null = null;
onMounted(() => {
  unsubscribeLive = subscribeLive(
    {
      root: (e) => {
        if (e.kind === "index_changed") refresh();
      },
      resync: refresh,
    },
    { onScreen: cardEl.value ?? undefined },
  );
});
onBeforeUnmount(() => unsubscribeLive?.());

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
    ref="cardEl"
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
             non-title chrome: the feedback button. No date: a message
             shows its own, and a document's other stamps are in its
             frontmatter. The "open this column alone" affordance lives
             in the host's column chrome. -->
        <p class="meta">
          <FeedbackButton
            :entity-uuid="chat.markdown_uuid"
            entity-kind="conversation"
            label="Conversation"
          />
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
      <div
        v-if="remoteUnique.length"
        class="remote-banner"
        :class="{ 'remote-banner--pending': remotePending.length }"
      >
        <template v-if="remotePending.length">
          <span class="remote-banner-text">
            <strong>{{ remotePending.length }}</strong> {{ remoteNoun(remotePending.length) }} not
            loaded — loading one tells its host you opened this, once.
          </span>
          <span class="remote-banner-hosts">
            <button
              v-for="h in remoteHosts"
              :key="h.host"
              type="button"
              class="remote-host"
              :disabled="remoteBusy"
              :title="`Load everything on ${h.host}, here and in every other document`"
              @click="allow('host', h.host)"
            >
              {{ h.host }}<span v-if="h.count > 1" class="remote-host-count">×{{ h.count }}</span>
            </button>
          </span>
          <span class="remote-banner-actions">
            <button
              type="button"
              class="remote-action remote-load-all"
              :disabled="remoteBusy"
              title="Load everything in this document"
              @click="allow('document', chat.markdown_uuid)"
            >
              Load all
            </button>
            <button
              v-if="chat.source_ref"
              type="button"
              class="remote-action"
              :disabled="remoteBusy"
              :title="`Load remote images in every document from ${chat.source_ref.label} without asking`"
              @click="allow('source', chat.source_ref.id)"
            >
              Always for {{ chat.source_ref.label }}
            </button>
          </span>
        </template>
        <span v-else class="remote-banner-text">
          {{ remoteNoun(remoteUnique.length).replace(/^remote/, "Remote") }} loaded.
        </span>
        <span v-if="remoteRules.length" class="remote-banner-rules">
          <span class="remote-rules-label">Let through by</span>
          <span v-for="rule in remoteRules" :key="rule.allow_uuid" class="remote-rule">
            {{ ruleLabel(rule) }}
            <button
              type="button"
              class="remote-rule-forget"
              :disabled="remoteBusy"
              :title="`Forget this rule (allowed ${rule.created_at_utc})`"
              @click="forget(rule)"
            >
              ✕
            </button>
          </span>
        </span>
        <span class="remote-banner-links">
          <a href="#" @click.prevent="openRemoteTable(REMOTE_ALLOW_TABLE, 'Remote media rules')"
            >all rules</a
          >
          ·
          <a href="#" @click.prevent="openRemoteTable(REMOTE_FETCHED_TABLE, 'Remote media fetched')"
            >fetched</a
          >
        </span>
        <span v-if="remoteError" class="remote-banner-error">{{ remoteError }}</span>
      </div>
      <ul v-if="docLevelOutgoing.length" class="outgoing-edges">
        <li v-for="e in docLevelOutgoing" :key="e.edge_uuid">
          <span class="edge-arrow" aria-hidden="true">→</span>
          <!-- Producers should set `label` to the human-readable
               handle they want shown in the list (perseus labels a
               cross-edition edge with the counterpart edition's short
               id, e.g. "perseus-eng2"). When
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
      <div class="doc-body">
        <ChatBody
          :body="chat.body"
          :markdown-uuid="chat.markdown_uuid"
          :selected-section-uuid="jumpTo ?? sectionUuid"
          :outgoing-edges="chat.outgoing_edges"
          :hover-anchor-uuid="hoverAnchor"
          @open-edge="onOpenEdge"
          @hover-edge="onHoverEdge"
          :remote-accept="remoteAccept"
          :remote-context="remoteContext"
          :source-id="chat.source_ref?.id ?? null"
          @remote-media="remoteRefs = $event"
          @remote-load="allow('url', $event)"
          @frame-link="onFrameLink"
          @frame-contextmenu="onPaneContextMenu"
          @open-search="onOpenSearch"
          @open-card="props.ctx.host.openCards($event)"
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
/* A column: the chrome at its own height, then the body, which scrolls
   inside its frame and takes what is left — never less than most of the
   pane, so a long problems list cannot squeeze it away. */
.chat-preview {
  display: flex;
  flex-direction: column;
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
  /* Reading text: never below 14px, whatever the density. */
  font-size: max(14px, calc(var(--datalib-font-size) + 2px));
}
.chat-preview > * {
  flex: none;
}
.doc-body {
  flex: 1 1 0;
  min-height: 70%;
  /* The frame pads its own body. */
  margin: 0 -1rem -0.75rem;
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
  color: var(--datalib-error-fg);
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
   before reading it. Muted once everything is loaded. */
.remote-banner {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 0.35rem 0.6rem;
  margin: 0 0 0.5rem;
  padding: 0.4rem 0;
  font-size: 0.8rem;
  color: var(--datalib-muted);
  border-top: 1px solid var(--datalib-border);
  border-bottom: 1px solid var(--datalib-border);
}
.remote-banner--pending {
  color: inherit;
}
.remote-banner-text,
.remote-banner-rules,
.remote-banner-error {
  flex: 1 1 100%;
}
.remote-banner-hosts,
.remote-banner-actions,
.remote-banner-rules {
  display: inline-flex;
  flex-wrap: wrap;
  gap: 0.3rem;
  align-items: center;
}
.remote-banner-actions,
.remote-banner-links {
  margin-left: auto;
}
.remote-banner-links {
  font-size: 0.75rem;
  color: var(--datalib-muted);
}
.remote-banner-links a {
  color: inherit;
  text-decoration: underline dotted;
}
.remote-host,
.remote-action,
.remote-rule {
  font: inherit;
  font-size: 0.75rem;
  line-height: 1.3;
  padding: 0.1rem 0.5rem;
  color: inherit;
  background: var(--datalib-input-bg, #fff);
  border: 1px solid var(--datalib-border, #d8d8d8);
  border-radius: 999px;
}
.remote-host,
.remote-action {
  cursor: pointer;
}
.remote-host:hover,
.remote-action:hover {
  background: var(--datalib-hover, #f0f0f0);
}
.remote-host:disabled,
.remote-action:disabled,
.remote-rule-forget:disabled {
  opacity: 0.5;
  cursor: default;
}
.remote-host {
  font-family: var(--datalib-mono);
}
.remote-host-count {
  margin-left: 0.3rem;
  color: var(--datalib-muted);
}
.remote-load-all {
  border-color: var(--datalib-accent);
}
.remote-rules-label {
  font-size: 0.75rem;
}
.remote-rule {
  display: inline-flex;
  align-items: center;
  gap: 0.3rem;
}
.remote-rule-forget {
  font: inherit;
  font-size: 0.7rem;
  line-height: 1;
  padding: 0 0.15rem;
  color: var(--datalib-muted);
  background: transparent;
  border: none;
  cursor: pointer;
}
.remote-rule-forget:hover {
  color: var(--datalib-error-fg);
}
.remote-banner-error {
  color: var(--datalib-error-fg);
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
  background: color-mix(in srgb, var(--datalib-accent) 28%, transparent);
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
  border-radius: var(--datalib-radius);
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
