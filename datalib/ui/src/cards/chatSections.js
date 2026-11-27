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
 * The pane `el` scrolls in. In the document frame that is the frame's
 * own root, which scrolls as the viewport does: its top is the
 * viewport's, whatever it has scrolled by.
 * @param {Element} el
 * @returns {{ pane: Element, top: number, isViewport: boolean } | null}
 */
function scrollPane(el) {
  const pane = el.closest(".chat-preview");
  if (!pane) return null;
  const isViewport = pane === el.ownerDocument.scrollingElement;
  return { pane, top: isViewport ? 0 : pane.getBoundingClientRect().top, isViewport };
}

/**
 * Scroll `el` to the top of its scrollport.
 *
 * Sets scrollTop directly rather than calling `scrollIntoView`, which
 * silently does nothing in Chromium while the pane is re-rendering, so
 * a click inside a thread would move nothing.
 * @param {HTMLElement} el
 */
export function scrollSectionToTop(el) {
  const p = scrollPane(el);
  if (!p) return;
  p.pane.scrollTop += el.getBoundingClientRect().top - p.top;
}

/**
 * The element that answers to `uuid`: the outermost one naming it. A
 * message's box is drawn by the app around what the message says, so a
 * copy of another message's `data-section-uuid` that a message carries
 * sits inside a box and never answers in its place. An id nothing outside
 * a box names (a reaction, a sentence) answers where it first appears.
 * @param {ParentNode} root
 * @param {string} uuid
 * @returns {HTMLElement | null}
 */
export function sectionEl(root, uuid) {
  const all = root.querySelectorAll(`[data-section-uuid="${uuid.replace(/["\\]/g, "\\$&")}"]`);
  for (const el of all) {
    if (!el.parentElement?.closest("[data-section-uuid]")) return /** @type {HTMLElement} */ (el);
  }
  return /** @type {HTMLElement | null} */ (all[0] ?? null);
}

/**
 * Whether `el` is the element that answers to the id it names.
 * @param {ParentNode} root
 * @param {Element} el
 */
export function answers(root, el) {
  const uuid = el.getAttribute("data-section-uuid");
  return uuid !== null && sectionEl(root, uuid) === el;
}

/**
 * The uuid of the message around `el`: the closest `id="m-{uuid}"` that
 * answers to its id, so a copy of another message's wrapper inside a
 * message is walked past.
 * @param {Element | null | undefined} el
 * @returns {string | null}
 */
export function messageAncestor(el) {
  let cur = el ?? null;
  while (cur) {
    if (cur.id.startsWith("m-") && answers(cur.ownerDocument, cur)) return cur.id.slice(2);
    cur = cur.parentElement;
  }
  return null;
}

/**
 * Open every `<details>` enclosing `el`. A tool step lives inside a
 * collapsed `.tool-group`, so "scroll to it" has nothing to scroll to
 * until the group is open. Walks the whole chain: groups nest.
 * @param {HTMLElement} el
 */
export function openEnclosingDetails(el) {
  for (let d = el.closest("details"); d; d = d.parentElement?.closest("details") ?? null) {
    d.open = true;
  }
}

/**
 * A section's header, where its controls go: the `## ` line every
 * renderer writes for a message with an author and a time
 * (`MessageHeader`), which always carries the `.msg-ts` time. A section
 * with none, a page or a system note, gets its controls at its top.
 * @param {HTMLElement} el
 * @returns {HTMLElement | null}
 */
export function metaHost(el) {
  const h2 = el.querySelector(":scope > h2");
  return h2?.querySelector(":scope > .msg-ts") ? /** @type {HTMLElement} */ (h2) : null;
}

/**
 * Add a control to a header's actions: the controls a reader points at
 * the message to see, kept together at the header's end
 * (`span.msg-actions`). The source-link arrow the renderer wrote is moved
 * in as the last of them — `↗` reads as "and here is the thing itself",
 * so it stays last however many controls accumulate before it.
 * @param {HTMLElement} host
 * @param {HTMLElement} control
 */
function addHeaderControl(host, control) {
  const doc = host.ownerDocument;
  let actions = host.querySelector(":scope > .msg-actions");
  if (!actions) {
    actions = doc.createElement("span");
    actions.className = "msg-actions";
    const link = host.querySelector(":scope > .source-link");
    if (link) actions.append(link);
    host.append(actions);
  }
  const link = actions.querySelector(":scope > .source-link");
  if (link) link.before(control);
  else actions.append(control);
}

/**
 * Give every `[data-section-uuid]` a 🔗 button that copies a link to it,
 * `datalib:doc/<page>#<section>`, and the page title one that copies the
 * document's — unless the page is shown under a title bar that has one
 * (`pageTitle: false`). The button keeps the uuid it names; the copy is
 * written when it is clicked (`linkOfButton`).
 * @param {HTMLElement} root
 * @param {{ pageTitle?: boolean }} [opts]
 */
export function injectCopyLinkButtons(root, { pageTitle = true } = {}) {
  const button = (uuid, label) => {
    const btn = root.ownerDocument.createElement("button");
    btn.type = "button";
    btn.className = "copy-link";
    btn.dataset.uuid = uuid;
    btn.title = `${label} (${uuid})`;
    btn.setAttribute("aria-label", label);
    btn.textContent = "🔗";
    return btn;
  };

  for (const el of root.querySelectorAll("div[data-section-uuid], section[data-section-uuid]")) {
    if (el.querySelector(":scope > h2 .copy-link, :scope > .copy-link")) continue;
    const uuid = el.getAttribute("data-section-uuid") ?? "";
    if (!uuid || !answers(root, el)) continue;
    const btn = button(uuid, "Copy link to this message");
    const host = metaHost(el);
    if (host) addHeaderControl(host, btn);
    else el.prepend(btn);
  }

  if (!pageTitle) return;
  for (const el of root.querySelectorAll("[data-page-title-uuid]")) {
    // The id sits on the heading itself, or on a block whose heading is
    // a markdown `#` (a contact page's head, whose heading is a chip).
    const host = el.matches("h1") ? el : (el.querySelector("h1") ?? el);
    if (host.querySelector(":scope > .msg-actions > button.copy-link")) continue;
    const uuid = el.getAttribute("data-page-title-uuid") ?? "";
    if (!uuid) continue;
    addHeaderControl(host, button(uuid, "Copy link to this page"));
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
 * Fold a recipients line longer than two lines to two, with a button
 * after it that says how many people it names.
 * @param {HTMLElement} line
 */
function foldRecipients(line) {
  if (line.dataset.foldChecked) return;
  line.dataset.foldChecked = "1";
  line.classList.add("msg-recipients--folded");
  // Chips drawn later are a little wider than the links measured here,
  // so a line that just fits may wrap to a third; it then shows whole.
  if (line.scrollHeight <= line.clientHeight + 1) {
    line.classList.remove("msg-recipients--folded");
    return;
  }
  const count = line.querySelectorAll(".msg-recipient, a.chip").length;
  const toggle = line.ownerDocument.createElement("button");
  toggle.type = "button";
  toggle.className = "msg-recipients-toggle";
  toggle.textContent = `Show all ${count}`;
  toggle.addEventListener("click", () => {
    const folded = line.classList.toggle("msg-recipients--folded");
    line.classList.toggle("msg-recipients--open", !folded);
    toggle.textContent = folded ? `Show all ${count}` : "Show fewer";
  });
  line.after(toggle);
}

/**
 * Clamp messages that are too tall to scroll past comfortably, and give
 * them a sticky header carrying "jump to the start of this message" and
 * "jump to the next one" — so being deep inside a wall of text is never
 * a place you have to scroll your way out of. A recipients line too
 * long for its header folds first.
 *
 * Idempotent: re-running over an already-decorated body does nothing.
 * @param {HTMLElement} root
 * @param {{ clampPx?: number }} [opts]
 */
export function decorateLongMessages(root, opts = {}) {
  const clampPx = opts.clampPx ?? LONG_MESSAGE_PX;
  // A new body replaces the old one's nodes, so observers from the previous
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

  // Before the messages are measured, so a folded list is not what
  // makes one long.
  for (const line of root.querySelectorAll(".msg > p > .msg-recipients:first-child")) {
    foldRecipients(line);
  }

  // A part of a page (`msg--page`) is read whole, as the page is.
  for (const el of root.querySelectorAll(":scope > .msg[data-section-uuid]:not(.msg--page)")) {
    if (el.dataset.longChecked) continue;
    el.dataset.longChecked = "1";
    // Measured before any clamp class lands, so this is the content's
    // own height rather than the clamped box's.
    if (el.scrollHeight <= clampPx) continue;
    el.classList.add("msg--long", "msg--clamped");

    const doc = root.ownerDocument;
    const more = doc.createElement("button");
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
    const nav = doc.createElement("span");
    nav.className = "msg-nav";
    const jump = (glyph, label, target) => {
      const b = doc.createElement("button");
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
    addHeaderControl(host, nav);

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
    const p = scrollPane(el);
    if (!p) continue;
    // The frame's viewport is observed through its document; an
    // implicit root would be the app's window, not the frame's.
    const io = new IntersectionObserver(
      ([entry]) => host.classList.toggle("is-stuck", entry.intersectionRatio < 1),
      {
        root: p.isViewport ? el.ownerDocument : p.pane,
        threshold: [1],
        rootMargin: "-1px 0px 0px 0px",
      },
    );
    io.observe(host);
    root.__chatStickyObservers.push(io);
  }
}
