// Which rendered documents the grid's Indexed / Embedded columns still
// need an answer for. The columns ask only about the rows on screen, and
// a margin around them, so each request is about a screenful however many
// rows the grid holds: every answer costs the applet a file read and a hash.

// Enough that a short scroll lands on answers already in hand.
export const ASK_MARGIN = 50;

export type RowRange = { top: number; bottom: number };

export function widen(range: RowRange, rowCount: number, margin = ASK_MARGIN): RowRange {
  return {
    top: Math.max(0, range.top - margin),
    bottom: Math.min(rowCount - 1, range.bottom + margin),
  };
}

export function markdownsToAsk(
  rows: Iterable<{ markdown_uuid?: string | null } | null | undefined>,
  answered: ReadonlyMap<string, unknown>,
  asked: ReadonlySet<string>,
): string[] {
  // A set: a thread's messages share one document.
  const out = new Set<string>();
  for (const row of rows) {
    const uuid = row?.markdown_uuid;
    if (uuid && !answered.has(uuid) && !asked.has(uuid)) out.add(uuid);
  }
  return [...out];
}
