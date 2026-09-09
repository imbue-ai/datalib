// DOM decoration for a rendered chat body: the per-section copy button,
// and the handling that makes a very long message navigable.
//
// Plain JavaScript, and deliberately so. `ChatBody.ce.vue` imports it,
// and `tools/chat_preview.mjs` inlines this same file into the preview
// page it generates — so the page you review a layout change in behaves
// like the app instead of like a hand-written imitation of it. Anything
// here must therefore stay dependency-free and touch nothing but the
// DOM node it is handed.

/** A message taller than this (CSS px) is clamped and gets jump controls. */
export const LONG_MESSAGE_PX = 360;

/**
 * Scroll `el` to the top of its scrollport.
 *
 * Sets scrollTop directly rather than calling `scrollIntoView`, which
 * silently no-ops in Chromium when the pane is re-rendering — that bug
 * is why "nothing happens when I click around inside one thread" was
 * reported once already.
 * @param {HTMLElement} el
 */
export function scrollSectionToTop(el) {
  const pane = el.closest(".chat-preview");
  if (!pane) return;
  pane.scrollTop += el.getBoundingClientRect().top - pane.getBoundingClientRect().top;
}

/**
 * Open every `<details>` enclosing `el`. A tool step lives inside a
 * collapsed `.tool-group`, so "scroll to it" has nothing to scroll to
 * until the group is open. Walks the whole chain: groups nest.
 * @param {HTMLElement} el
 */
export function openEnclosingDetails(el) {
  for (
    let d = el.closest("details");
    d;
    d = d.parentElement?.closest("details") ?? null
  ) {
    d.open = true;
  }
}

/**
 * Where a section's copy button goes: the chat-common message header
 * (the `## ` line whose parts are tagged `.msg-author` / `.msg-ts`),
 * else an explicit `.msg-meta` div, else the first `<p><em>…</em></p>`
 * some renderers emit as an italic meta line. Block sections have none
 * of the three and get the button at the top of the section.
 * @param {HTMLElement} el
 * @returns {HTMLElement | null}
 */
export function metaHost(el) {
  const header = el.querySelector(":scope > h2 > .msg-author");
  if (header?.parentElement) return header.parentElement;
  const meta = el.querySelector(":scope > .msg-meta");
  if (meta) return meta;
  for (const p of el.querySelectorAll(":scope > p")) {
    if (p.firstElementChild?.tagName === "EM" && p.children.length === 1) return p;
  }
  return null;
}

/**
 * Give every `[data-section-uuid]` a button that copies its uuid, and
 * the page title one that copies the document's.
 * @param {HTMLElement} root
 */
export function injectCopyUuidButtons(root) {
  const button = (uuid, label) => {
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "copy-uuid";
    btn.dataset.uuid = uuid;
    btn.title = `${label} (${uuid})`;
    btn.setAttribute("aria-label", label);
    btn.textContent = "🆔";
    return btn;
  };

  for (const el of root.querySelectorAll(
    "div[data-section-uuid], section[data-section-uuid]",
  )) {
    if (
      el.querySelector(
        ":scope > h2 .copy-uuid, :scope > .msg-meta .copy-uuid, :scope > p .copy-uuid, :scope > .copy-uuid",
      )
    )
      continue;
    const uuid = el.getAttribute("data-section-uuid") ?? "";
    if (!uuid) continue;
    const btn = button(uuid, "Copy section ID");
    const host = metaHost(el);
    if (host) host.append(document.createTextNode(" · "), btn);
    else el.prepend(btn);
  }

  for (const el of root.querySelectorAll("[data-page-title-uuid]")) {
    if (el.querySelector(":scope > button.copy-uuid")) continue;
    const uuid = el.getAttribute("data-page-title-uuid") ?? "";
    if (!uuid) continue;
    el.append(document.createTextNode(" "), button(uuid, "Copy page ID"));
  }
}

/** The next top-level item after `el`, or null at the end of the doc. */
function nextItem(el) {
  for (let n = el.nextElementSibling; n; n = n.nextElementSibling) {
    if (n.matches(".msg, details.tool-group")) return /** @type {HTMLElement} */ (n);
  }
  return null;
}

/**
 * Clamp messages that are too tall to scroll past comfortably, and give
 * them a sticky header carrying "jump to the start of this message" and
 * "jump to the next one" — so being deep inside a wall of text is never
 * a place you have to scroll your way out of.
 *
 * Idempotent: re-running over an already-decorated body does nothing.
 * @param {HTMLElement} root
 * @param {{ clampPx?: number }} [opts]
 */
export function decorateLongMessages(root, opts = {}) {
  const clampPx = opts.clampPx ?? LONG_MESSAGE_PX;
  // v-html replaces the body wholesale, so observers from the previous
  // render are watching detached nodes. `root` survives; hang them off
  // it and drop them here rather than leaking one set per document.
  for (const io of root.__chatStickyObservers ?? []) io.disconnect();
  root.__chatStickyObservers = [];
  root.__chatWidthObserver?.disconnect();
  root.__chatWidthObserver = null;

  // Height is meaningless until the pane has a width. In a column that
  // has not been laid out yet — a hidden tab, the frame before first
  // paint — every line of text wraps into a zero-width box and a
  // one-line message measures taller than the clamp, so every message
  // gets a "Show more" that does nothing. Wait for a real width.
  if (!root.clientWidth) {
    const ro = new ResizeObserver(() => {
      if (!root.clientWidth) return;
      decorateLongMessages(root, opts);
    });
    ro.observe(root);
    root.__chatWidthObserver = ro;
    return;
  }

  for (const el of root.querySelectorAll(":scope > .msg[data-section-uuid]")) {
    if (el.dataset.longChecked) continue;
    el.dataset.longChecked = "1";
    // Measured before any clamp class lands, so this is the content's
    // own height rather than the clamped box's.
    if (el.scrollHeight <= clampPx) continue;
    el.classList.add("msg--long", "msg--clamped");

    const more = document.createElement("button");
    more.type = "button";
    more.className = "msg-expand";
    more.textContent = "Show more";
    more.addEventListener("click", () => {
      const clamped = el.classList.toggle("msg--clamped");
      more.textContent = clamped ? "Show more" : "Show less";
      // Re-collapsing from far down the message would otherwise leave
      // the viewport somewhere below the message entirely.
      if (clamped) scrollSectionToTop(el);
    });
    el.append(more);

    const host = metaHost(el);
    if (!host) continue;
    const nav = document.createElement("span");
    nav.className = "msg-nav";
    const jump = (glyph, label, target) => {
      const b = document.createElement("button");
      b.type = "button";
      b.className = "msg-jump";
      b.textContent = glyph;
      b.title = label;
      b.setAttribute("aria-label", label);
      b.addEventListener("click", target);
      return b;
    };
    nav.append(
      jump("▲", "Jump to the start of this message", () => scrollSectionToTop(el)),
      jump("▼", "Jump to the next message", () => {
        const next = nextItem(el);
        if (next) scrollSectionToTop(next);
        else el.closest(".chat-preview")?.scrollTo({ top: 1e9 });
      }),
    );
    host.append(nav);

    // A pinned header unavoidably covers the line of text passing under
    // it — that is what `position: sticky` does, and GitHub's file
    // headers and VS Code's sticky scroll both live with it. What is
    // avoidable is the header looking identical whether it is pinned or
    // not, which makes the covered line read as content that vanished.
    // `.is-stuck` lets the CSS give it a shadow and an edge for exactly
    // as long as it is floating over something.
    //
    // The threshold/rootMargin pair is the standard detection: with the
    // root's top edge pulled in by 1px, a header sitting at `top: 0`
    // can no longer be fully visible, so its ratio drops below 1.
    const pane = el.closest(".chat-preview");
    if (!pane) continue;
    const io = new IntersectionObserver(
      ([entry]) => host.classList.toggle("is-stuck", entry.intersectionRatio < 1),
      { root: pane, threshold: [1], rootMargin: "-1px 0px 0px 0px" },
    );
    io.observe(host);
    root.__chatStickyObservers.push(io);
  }
}
