// A document body to the HTML the card mounts: markdown-it with the
// renderers' HTML let through, relative asset references rewritten to
// the applet's routes, then the sanitizer. Its own module so the
// document card can render a body without mounting it — to ask the
// server which of its remote references may load before the first
// paint — and the chat body renders it the same way.
import MarkdownIt from "markdown-it";
import hljs from "highlight.js";
import { assetUrl, isAbsoluteOrUrl, rewriteIframeSrcs } from "./asset_urls";
import { sanitizeRenderedHtml, type Sanitized, type SanitizeOptions } from "./sanitize";

function highlight(code: string, lang: string): string {
  if (lang && hljs.getLanguage(lang)) {
    try {
      return hljs.highlight(code, { language: lang }).value;
    } catch {
      /* fall through to escape */
    }
  }
  return code.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

const md = new MarkdownIt({
  html: true,
  linkify: true,
  breaks: false,
  highlight,
});

// Rewrite relative asset references (`blobs/foo.png`, `plots/x.html`) to
// backend asset URLs. Absolute paths (`/...`) and full URLs
// (`http://...`, `data:...`, `//cdn/...`) pass through unchanged. The
// rules live in `./asset_urls` so they are unit-testable on their own.
function envUuid(env: unknown): string | null {
  return (env as { markdownUuid?: string | null } | undefined)?.markdownUuid ?? null;
}
const defaultImageRender =
  md.renderer.rules.image ||
  ((tokens, idx, options, _env, self) => self.renderToken(tokens, idx, options));
md.renderer.rules.image = (tokens, idx, options, env, self) => {
  const token = tokens[idx];
  const srcIdx = token.attrIndex("src");
  if (srcIdx >= 0 && token.attrs) {
    // markdown-it 15 types an attribute value as `string | number` (it
    // ships its own types now; @types/markdown-it 14 said `string`).
    // Anything the parser produces for `src` is a string — the number
    // arm is for tokens built programmatically — so narrow rather than
    // coerce, and leave a non-string alone.
    const raw = token.attrs[srcIdx][1];
    const src = typeof raw === "string" ? raw : null;
    const uuid = envUuid(env);
    if (uuid && src && !isAbsoluteOrUrl(src)) {
      token.attrs[srcIdx][1] = assetUrl(uuid, src);
    }
  }
  return defaultImageRender(tokens, idx, options, env, self);
};

// Same rewrite for `<iframe src>`, which arrives as raw HTML rather than
// as a parsed token — see `rewriteIframeSrcs`.
for (const rule of ["html_block", "html_inline"] as const) {
  const fallback = md.renderer.rules[rule];
  md.renderer.rules[rule] = (tokens, idx, options, env, self) => {
    const rendered = fallback ? fallback(tokens, idx, options, env, self) : tokens[idx].content;
    return rewriteIframeSrcs(rendered, envUuid(env));
  };
}

/// The body as HTML for the page: rendered, rewritten, sanitized —
/// sanitized last, after every rewrite, because the body is whatever
/// the source sent and `html: true` lets it through as HTML.
export function renderDocument(
  body: string,
  markdownUuid: string | null,
  options: SanitizeOptions = {},
): Sanitized {
  return sanitizeRenderedHtml(md.render(body, { markdownUuid }), options);
}
