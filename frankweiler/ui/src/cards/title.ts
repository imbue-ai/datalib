// Human-readable card titles. When dev mode is off (see devMode.ts)
// the layouts show a card's title in the chrome bar instead of its
// source. A factory declares its title by wrapping the render it
// returns in `titled(...)`; a card that doesn't gets a best-effort
// name derived from its source.
import type { CardRender } from "./types";

// Attach a human-readable title to a render. The title is computed at
// factory-call time, so it can reflect the arguments — e.g.
// `gridView({ q: "kraken" })` titles itself `Search: kraken`.
export function titled(title: string, render: CardRender): CardRender {
  render.cardTitle = title;
  return render;
}

// The title the chrome shows for a card: the declared one when the
// render carries it, else a name derived from the source — the bare
// factory/alias name for the common `name(...)` shape (an alias name
// is already the friendliest thing we have for a user component), a
// generic label otherwise.
export function displayTitle(
  source: string,
  declared: string | null | undefined,
): string {
  if (declared) return declared;
  const trimmed = source.trim();
  if (trimmed === "") return "new card";
  const call = trimmed.match(/^([A-Za-z_$][\w$]*)\s*\(/);
  return call ? call[1] : "custom card";
}
