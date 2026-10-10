// How a row of chips shares one grid cell: every label gets an even
// share of the width, shortened in its middle where it is longer, and
// the chips that would fall below a readable share fold into a "+N"
// chip. Pure: the cell measures, this decides.

/// What a cell is measured in: the width of a character of label text,
/// what a chip adds around its label (its lead and padding), the gap
/// between chips, and the fewest characters a label is cut to before a
/// chip is folded instead.
export type ChipMeasure = { charPx: number; chipPx: number; gapPx: number; minChars: number };

export const CELL_MEASURE: ChipMeasure = { charPx: 7, chipPx: 26, gapPx: 4, minChars: 6 };

/// `text` cut to `max` characters by taking out its middle: the start
/// and the end stay, so "Jean-Luc Picard" reads as "Jean-…icard".
export function elideMiddle(text: string, max: number): string {
  const chars = [...text];
  if (chars.length <= max) return text;
  if (max <= 1) return "…";
  const keep = max - 1;
  const head = Math.ceil(keep / 2);
  const tail = keep - head;
  return chars.slice(0, head).join("") + "…" + (tail ? chars.slice(-tail).join("") : "");
}

/// The longest share every label may take so that together they fit in
/// `budget` characters: a short label takes only what it needs and
/// leaves the rest to the long ones.
function share(lengths: number[], budget: number): number {
  const sorted = [...lengths].sort((a, b) => a - b);
  let left = budget;
  for (let i = 0; i < sorted.length; i++) {
    const even = Math.floor(left / (sorted.length - i));
    if (sorted[i] > even) return even;
    left -= sorted[i];
  }
  return Number.POSITIVE_INFINITY;
}

/// The labels to draw, in order, each cut to its share, and how many
/// trailing chips folded into the count. At least one chip is always
/// drawn, however narrow the cell.
export function fitChips(
  labels: string[],
  widthPx: number,
  m: ChipMeasure = CELL_MEASURE,
): { shown: string[]; folded: number } {
  for (let k = labels.length; k >= 1; k--) {
    const folded = labels.length - k;
    const countPx = folded ? m.chipPx + m.gapPx + `+${folded}`.length * m.charPx : 0;
    const budget = Math.floor((widthPx - k * m.chipPx - (k - 1) * m.gapPx - countPx) / m.charPx);
    const kept = labels.slice(0, k);
    const lengths = kept.map((l) => [...l].length);
    const each = share(lengths, budget);
    const readable = lengths.every((n) => Math.min(n, each) >= Math.min(n, m.minChars));
    if (readable || k === 1) {
      const cut = readable ? each : m.minChars;
      return { shown: kept.map((l) => elideMiddle(l, cut)), folded };
    }
  }
  return { shown: [], folded: 0 };
}
