// The icon a card's metadata names, resolved to something drawable. A
// token is one of three things: a glyph below — monochrome 24×24 path
// data, drawn in `currentColor`; the name of a source mark in
// `src/assets/` (`slack`, `gmail`, …); or an image of its own, as a
// `data:` URL (`data:image/png;base64,…`). Builtin cards name theirs
// in cards/catalog.ts; a custom component names its in the `icon` of
// its `<name>.json`.
import { iconUrl } from "@/config/icons";
import { STEP_GLYPHS } from "@/config/glyphs";

/// Material Design icons (Apache-2.0), filled.
export const CARD_GLYPHS: Record<string, string> = {
  // social/person
  person:
    "M12 12c2.21 0 4-1.79 4-4s-1.79-4-4-4-4 1.79-4 4 1.79 4 4 4zm0 2c-2.67 0-8 1.34-8 4v2h16v-2c0-2.66-5.33-4-8-4z",
  // action/home
  home: "M10 20v-6h4v6h5v-8h3L12 3 2 12h3v8z",
  // device/storage
  sources:
    "M2 20h20v-4H2v4zm2-3h2v2H4v-2zM2 4v4h20V4H2zm4 3H4V5h2v2zm-4 7h20v-4H2v4zm2-3h2v2H4v-2z",
  // action/search
  search:
    "M15.5 14h-.79l-.28-.27C15.41 12.59 16 11.11 16 9.5 16 5.91 13.09 3 9.5 3S3 5.91 3 9.5 5.91 16 9.5 16c1.61 0 3.09-.59 4.23-1.57l.27.28v.79l5 4.99L20.49 19l-4.99-5zm-6 0C7.01 14 5 11.99 5 9.5S7.01 5 9.5 5 14 7.01 14 9.5 11.99 14 9.5 14z",
  // image/grid_on
  table:
    "M20 2H4c-1.1 0-2 .9-2 2v16c0 1.1.9 2 2 2h16c1.1 0 2-.9 2-2V4c0-1.1-.9-2-2-2zM8 20H4v-4h4v4zm0-6H4v-4h4v4zm0-6H4V4h4v4zm6 12h-4v-4h4v4zm0-6h-4v-4h4v4zm0-6h-4V4h4v4zm6 12h-4v-4h4v4zm0-6h-4v-4h4v4zm0-6h-4V4h4v4z",
  // editor/bubble_chart
  map: "M7.2 11.2a3.2 3.2 0 1 0 0 6.4 3.2 3.2 0 1 0 0-6.4zM14.8 16a2 2 0 1 0 0 4 2 2 0 1 0 0-4zM15.2 4a4.8 4.8 0 1 0 0 9.6 4.8 4.8 0 1 0 0-9.6z",
  // action/subject
  log: "M14 17H4v2h10v-2zm6-8H4v2h16V9zM4 15h16v-2H4v2zM4 5v2h16V5H4z",
  // action/code
  code: "M9.4 16.6L4.8 12l4.6-4.6L8 6l-6 6 6 6 1.4-1.4zm5.2 0l4.6-4.6-4.6-4.6L16 6l6 6-6 6-1.4-1.4z",
  // action/description
  document:
    "M14 2H6c-1.1 0-1.99.9-1.99 2L4 20c0 1.1.89 2 1.99 2H18c1.1 0 2-.9 2-2V8l-6-6zm2 16H8v-2h8v2zm0-4H8v-2h8v2zm-3-5V3.5L18.5 9H13z",
  // action/history
  history:
    "M13 3c-4.97 0-9 4.03-9 9H1l3.89 3.89.07.14L9 12H6c0-3.87 3.13-7 7-7s7 3.13 7 7-3.13 7-7 7c-1.93 0-3.68-.79-4.94-2.06l-1.42 1.42C8.27 19.99 10.51 21 13 21c4.97 0 9-4.03 9-9s-4.03-9-9-9zm-1 5v5l4.28 2.54.72-1.21-3.5-2.08V8H12z",
  // action/account_tree
  dag: "M22 11V3h-7v3H9V3H2v8h7V8h2v10h4v3h7v-8h-7v3h-2V8h2v3z",
  // action/dashboard
  dashboard: "M3 13h8V3H3v10zm0 8h8v-6H3v6zm10 0h8V11h-8v10zm0-18v6h8V3h-8z",
  // content/add
  add: "M19 13h-6v6h-2v-6H5v-2h6V5h2v6h6v2z",
  // alert/warning
  problem: "M1 21h22L12 2 1 21zm12-3h-2v-2h2v2zm0-4h-2v-4h2v4z",
  // communication/forum
  chat: "M21 6h-2v9H6v2c0 .55.45 1 1 1h11l4 4V7c0-.55-.45-1-1-1zm-4 6V3c0-.55-.45-1-1-1H3c-.55 0-1 .45-1 1v14l4-4h10c.55 0 1-.45 1-1z",
  book: STEP_GLYPHS.render,
  component: STEP_GLYPHS.applet,
};

/// What a card without an icon of its own draws.
export const DEFAULT_CARD_ICON = "component";

export type DrawnIcon = { kind: "glyph"; path: string } | { kind: "image"; url: string };

/// The image types an icon may carry inline. Drawn through `<img>`, so
/// an SVG's scripts never run; the app's CSP already allows `data:`
/// images (backend/http/src/embed.rs).
const DATA_IMAGE = /^data:image\/(png|jpeg|gif|webp|svg\+xml)[;,]/;

/// A glyph, then a source mark, then an inline image; an unknown token
/// draws the generic component glyph rather than nothing, so a misspelt
/// icon shows up as a plain card instead of a gap.
export function resolveIcon(token: string | null | undefined): DrawnIcon {
  if (token && token in CARD_GLYPHS) return { kind: "glyph", path: CARD_GLYPHS[token] };
  const url = iconUrl(token);
  if (url) return { kind: "image", url };
  if (token && DATA_IMAGE.test(token)) return { kind: "image", url: token };
  return { kind: "glyph", path: CARD_GLYPHS[DEFAULT_CARD_ICON] };
}
