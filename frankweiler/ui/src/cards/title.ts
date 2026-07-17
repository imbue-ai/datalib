// Human-readable card titles. When dev mode is off (see devMode.ts)
// the layouts show a card's title in the chrome bar instead of its
// source. A card sets (and updates) its title via ctx.setTitle — see
// CardCtx in types.ts; a card that never does gets a best-effort name
// derived from its source.

// The title the chrome shows for a card: the declared one when set,
// else a name derived from the source — the bare factory/alias name
// for the common `name(...)` shape (an alias name is already the
// friendliest thing we have for a user component), a generic label
// otherwise.
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
