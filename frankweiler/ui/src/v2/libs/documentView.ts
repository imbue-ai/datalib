// Prebuilt "document" view factory — `documentView("md-uuid",
// "section-uuid")` in card source returns a CardRender that fetches
// /api/chat/{md} and renders the markdown body inside the shadow
// root, highlighting and scrolling to the given section. The card is
// fully determined by its source: a different selection is a
// different card (the grid opens a fresh column), so there is nothing
// to subscribe to.
//
// Trimmed relative to v1 DocColumn.vue: no feedback modal, no
// right-click menu, no copy-uuid buttons, no edge decoration. Just
// enough to validate markdown-it inside a shadow root and the
// source-driven column flow.
import MarkdownIt from "markdown-it";
import hljs from "highlight.js";
// The .css imports are inlined at build time by Vite's CSS handling
// — but inside a shadow root we have to drop the text into a
// <style> manually, since the global stylesheet doesn't pierce the
// shadow boundary. The `?inline` query asks Vite to give us the
// stylesheet as a string instead of injecting it into <head>.
import hljsCss from "highlight.js/styles/github-dark.css?inline";
import { fetchChat, type ChatResponse } from "@/api";
import type { CardRender, Teardown } from "../types";

const md = new MarkdownIt({
  html: true,
  linkify: true,
  breaks: false,
  highlight: (code: string, lang: string) => {
    if (lang && hljs.getLanguage(lang)) {
      try {
        return hljs.highlight(code, { language: lang }).value;
      } catch {
        /* fall through */
      }
    }
    return code
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;");
  },
});

// Rewrite relative image srcs to backend asset URLs the same way
// ChatBody.vue does — without it, a doc that references local image
// assets renders broken icons.
function isAbsoluteOrUrl(src: string): boolean {
  return /^([a-z][a-z0-9+.-]*:|\/\/|\/|#)/i.test(src);
}
const defaultImageRender =
  md.renderer.rules.image ||
  ((tokens, idx, options, _env, self) =>
    self.renderToken(tokens, idx, options));
md.renderer.rules.image = (tokens, idx, options, env, self) => {
  const token = tokens[idx];
  const srcIdx = token.attrIndex("src");
  if (srcIdx >= 0 && token.attrs) {
    const src = token.attrs[srcIdx][1];
    const uuid = (env as { markdownUuid?: string | null } | undefined)
      ?.markdownUuid;
    if (uuid && src && !isAbsoluteOrUrl(src)) {
      token.attrs[srcIdx][1] = `/api/asset/${encodeURIComponent(uuid)}/${src
        .split("/")
        .map(encodeURIComponent)
        .join("/")}`;
    }
  }
  return defaultImageRender(tokens, idx, options, env, self);
};

export function documentView(
  markdownUuid?: string | null,
  sectionUuid?: string | null,
): CardRender {
  return (root) => {
    const style = document.createElement("style");
    style.textContent = SHADOW_CSS + "\n" + hljsCss;
    root.appendChild(style);

    const wrap = document.createElement("div");
    wrap.className = "doc-card";
    root.appendChild(wrap);

    const header = document.createElement("header");
    header.className = "doc-header";
    wrap.appendChild(header);

    const body = document.createElement("div");
    body.className = "doc-body markdown-body";
    wrap.appendChild(body);

    let inflight: AbortController | null = null;
    let alive = true;

    function setEmpty(msg: string) {
      header.textContent = "";
      body.innerHTML = `<p class="empty">${msg}</p>`;
    }

    function renderChat(chat: ChatResponse) {
      header.textContent = chat.name ?? chat.markdown_uuid;
      body.innerHTML = md.render(chat.body || "", {
        markdownUuid: chat.markdown_uuid,
      });
      applySectionHighlight();
    }

    function applySectionHighlight() {
      if (!sectionUuid) return;
      const target = body.querySelector<HTMLElement>(
        `[data-section-uuid="${sectionUuid.replace(/"/g, '\\"')}"]`,
      );
      if (!target) return;
      target.classList.add("selected");
      // The scrollable container is the .doc-body. Direct scrollTop
      // adjust avoids the scrollIntoView-no-op-on-prop-change issue
      // ChatBody.vue documents.
      const top =
        target.getBoundingClientRect().top - body.getBoundingClientRect().top;
      body.scrollTop += top - 20;
    }

    async function loadDoc() {
      if (!markdownUuid) {
        setEmpty("Select a row to preview.");
        return;
      }
      inflight = new AbortController();
      setEmpty("loading…");
      try {
        const chat = await fetchChat(markdownUuid, inflight.signal);
        if (!alive) return;
        renderChat(chat);
      } catch (e) {
        if ((e as { name?: string }).name === "AbortError") return;
        if (!alive) return;
        setEmpty(`error: ${(e as Error).message}`);
      }
    }

    loadDoc();

    const teardown: Teardown = () => {
      alive = false;
      inflight?.abort();
    };
    return teardown;
  };
}

const SHADOW_CSS = `
:host { display: block; height: 100%; }
.doc-card {
  display: flex;
  flex-direction: column;
  height: 100%;
  box-sizing: border-box;
  font: 14px system-ui, sans-serif;
  color: inherit;
}
.doc-header {
  flex: 0 0 auto;
  padding: 0.5rem 1rem;
  border-bottom: 1px solid #888;
  font-weight: 600;
}
.doc-body {
  flex: 1 1 auto;
  overflow-y: auto;
  padding: 0.75rem 1rem;
}
.empty { opacity: 0.6; }
.markdown-body p { margin: 0.4rem 0; }
.markdown-body pre {
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
  background: rgba(128,128,128,0.2);
  padding: 0 0.25rem;
  border-radius: 2px;
}
.markdown-body blockquote {
  border-left: 3px solid #888;
  margin: 0.5rem 0;
  padding-left: 0.75rem;
  opacity: 0.85;
}
.msg {
  padding: 0.5rem 0.75rem;
  border-left: 3px solid transparent;
  margin: 0.5rem 0;
}
.msg.selected {
  background: rgba(99, 102, 241, 0.15);
  outline: 2px solid #6366f1;
}
[data-section-uuid].selected {
  background: rgba(99, 102, 241, 0.15);
}
`;
