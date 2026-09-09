// Turn rendered chat markdown into one interactive HTML page, so a
// layout change can be reviewed — clicked, expanded, re-themed —
// without a data root.
//
// The fidelity comes from reading the app's own sources rather than
// re-stating them: markdown-it with the same options as
// `src/cards/ChatBody.ce.vue`, the CSS variables lifted out of
// `src/App.vue`, every `<style>` block of the two card components
// verbatim, and `src/cards/chatSections.js` — the very module the
// component imports — inlined into the page. Nothing about the layout
// is re-implemented here; only the preview's own chrome (the toolbar,
// and the light/dark switch below) is new.
//
// Usage: node chat_preview.mjs --md <dir-of-md> --out <file.html> [--ui <ui-root>]

import { readFileSync, readdirSync, writeFileSync, mkdirSync } from "node:fs";
import { join, dirname, resolve } from "node:path";
import MarkdownIt from "markdown-it";
import hljs from "highlight.js";

function arg(name, fallback) {
  const i = process.argv.indexOf(`--${name}`);
  if (i >= 0 && process.argv[i + 1]) return process.argv[i + 1];
  if (fallback !== undefined) return fallback;
  throw new Error(`missing --${name}`);
}

const mdDir = resolve(arg("md"));
const outFile = resolve(arg("out"));
// Two roots, deliberately. `--ui` is the *source* tree, so editing a
// .vue and re-running shows the new CSS with no rebuild. Packages come
// from wherever this script actually sits, which under `bazel run` is
// the rules_js-linked node_modules rather than a host `pnpm install`.
const uiRoot = resolve(arg("ui", join(import.meta.dirname, "..")));
const pkgRoot = resolve(import.meta.dirname, "..");

/** Every `<style>` block of a .vue file, concatenated. */
function vueStyles(relPath) {
  const src = readFileSync(join(uiRoot, relPath), "utf8");
  return [...src.matchAll(/<style[^>]*>([\s\S]*?)<\/style>/g)]
    .map((m) => m[1])
    .join("\n");
}

/**
 * App.vue defines its dark tokens only inside
 * `@media (prefers-color-scheme: dark)`, which no button can flip. Move
 * that block's declarations onto an explicit `:root.preview-dark` rule
 * and drop the media query, so the toolbar's switch actually decides —
 * and a reviewer on a dark OS can still see the light theme. The tokens
 * themselves are still App.vue's; only the selector changes.
 */
function themeSwitchable(appCss) {
  const media = /@media \(prefers-color-scheme: dark\)\s*\{\s*:root\s*\{([\s\S]*?)\}\s*\}/;
  const m = appCss.match(media);
  if (!m) throw new Error("App.vue no longer has a prefers-color-scheme block");
  return `${appCss.replace(media, "")}\n:root.preview-dark {${m[1]}}`;
}

const css = [
  themeSwitchable(vueStyles("src/App.vue")),
  readFileSync(
    join(pkgRoot, "node_modules/highlight.js/styles/github-dark.css"),
    "utf8",
  ),
  vueStyles("src/cards/DocCard.ce.vue"),
  vueStyles("src/cards/ChatBody.ce.vue"),
  // Preview-only chrome. The real pane is a resizable Miller column;
  // pin a plausible width and let the page grow to its content.
  `
  body { margin: 0; background: var(--datalib-bg); color: var(--datalib-fg);
         font-family: system-ui, sans-serif; }
  .preview-bar { position: sticky; top: 0; z-index: 2; display: flex; gap: 1rem;
    align-items: center; padding: .5rem 1rem; font-size: .85rem;
    background: var(--datalib-card-bg); border-bottom: 1px solid var(--datalib-border); }
  .preview-bar button { font: inherit; cursor: pointer; padding: .2rem .6rem;
    border: 1px solid var(--datalib-border); border-radius: 4px;
    background: var(--datalib-input-bg); color: inherit; }
  .preview-doc { max-width: 760px; margin: 0 auto 2rem; }
  .preview-doc > h2.preview-name { font-size: .75rem; font-weight: 600; margin: 1.5rem 0 .25rem;
    text-transform: uppercase; letter-spacing: .06em; color: var(--datalib-muted); }
  /* ONE scrollport for the whole page, not one per document. The pane
     has to really scroll or the sticky headers and jump controls have
     nothing to work against — but a scroll box per document would stack
     scrollbars inside a scrollbar, which the app never does and nobody
     wants to read. */
  html, body { height: 100%; }
  body { display: flex; flex-direction: column; }
  .preview-bar { flex: none; }
  .chat-preview { flex: 1; min-height: 0; }
  `,
].join("\n");

// The app's own decoration module, inlined verbatim. Read from
// `--ui` so an edit shows up on the next run without a rebuild.
const sectionsModule = readFileSync(
  join(uiRoot, "src/cards/chatSections.js"),
  "utf8",
);

// Preview-only wiring: the app calls these from ChatBody's lifecycle
// hooks and drives selection from a grid-row click; here a plain click
// stands in, so the selected styling is part of what gets reviewed.
const PREVIEW_SCRIPT = String.raw`
for (const body of document.querySelectorAll(".chat-body")) {
  injectCopyUuidButtons(body);
  decorateLongMessages(body);
}
document.addEventListener("click", (ev) => {
  const el = ev.target.closest?.("[data-section-uuid]");
  if (!el || ev.target.closest("button, a, summary")) return;
  for (const s of document.querySelectorAll(".selected")) s.classList.remove("selected");
  el.classList.add("selected");
  openEnclosingDetails(el);
});
const root = document.documentElement;
const themeBtn = document.getElementById("theme");
const setDark = (dark) => {
  root.classList.toggle("preview-dark", dark);
  root.style.colorScheme = dark ? "dark" : "light";
  themeBtn.textContent = dark ? "Light theme" : "Dark theme";
};
setDark(window.matchMedia("(prefers-color-scheme: dark)").matches);
themeBtn.addEventListener("click", () => setDark(!root.classList.contains("preview-dark")));
document.getElementById("expand").addEventListener("click", () => {
  const anyClosed = [...document.querySelectorAll("details")].some((d) => !d.open);
  for (const d of document.querySelectorAll("details")) d.open = anyClosed;
});
`;

const md = new MarkdownIt({
  html: true,
  linkify: true,
  breaks: false,
  highlight: (code, lang) => {
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
  },
});

/** Drop the YAML frontmatter the backend strips before serving. */
function stripFrontmatter(text) {
  if (!text.startsWith("---\n")) return text;
  const end = text.indexOf("\n---\n", 4);
  return end < 0 ? text : text.slice(end + 5);
}

const mdFiles = readdirSync(mdDir, { recursive: true })
  .filter((f) => typeof f === "string" && f.endsWith(".md"))
  .sort();
if (mdFiles.length === 0) throw new Error(`no .md files under ${mdDir}`);

const docs = mdFiles
  .map(
    (rel) => `<div class="preview-doc">
<h2 class="preview-name">${rel}</h2>
<div class="chat-body markdown-body">${md.render(
      stripFrontmatter(readFileSync(join(mdDir, rel), "utf8")),
    )}</div>
</div>`,
  )
  .join("\n");

const html = `<!doctype html>
<html><head><meta charset="utf-8"><title>Chat markdown preview</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>${css}</style></head>
<body>
<div class="preview-bar">
  <strong>Chat markdown preview</strong>
  <button id="theme" type="button">Dark theme</button>
  <button id="expand" type="button">Expand / collapse all</button>
  <span>Click a message to select it.</span>
</div>
<section class="chat-preview">
${docs}
</section>
<script type="module">${sectionsModule}\n${PREVIEW_SCRIPT}</script>
</body></html>`;

mkdirSync(dirname(outFile), { recursive: true });
writeFileSync(outFile, html);
process.stdout.write(`${outFile}\n`);
