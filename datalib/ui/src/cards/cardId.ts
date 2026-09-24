// What identifies one open card: an id minted when it opens, and a type
// read from its source. Both ride on the requests the card makes
// (cardScope.ts), so the server's request log can say which card, and
// which kind of card, is doing the asking.

// A UUIDv7 (RFC 9562): the first 48 bits are the Unix time in ms, so
// ids sort by when their cards were opened.
export function uuidv7(
  nowMs: number = Date.now(),
  random: Uint8Array = crypto.getRandomValues(new Uint8Array(10)),
): string {
  const b = new Uint8Array(16);
  let t = nowMs;
  for (let i = 5; i >= 0; i--) {
    b[i] = t % 256;
    t = Math.floor(t / 256);
  }
  b[6] = 0x70 | (random[0] & 0x0f);
  b[7] = random[1];
  b[8] = 0x80 | (random[2] & 0x3f);
  b.set(random.subarray(3, 10), 9);
  const hex = Array.from(b, (x) => x.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

export const newCardId = (): string => uuidv7();

// The factory or component a card's source calls: `gridView` for
// `gridView({ q: "…" })`, `comp.user.tetris` for a custom component.
// Source that is not a call is "custom"; no source at all is "blank".
export function cardType(source: string): string {
  const trimmed = source.trim();
  if (trimmed === "") return "blank";
  const call = trimmed.match(/^([A-Za-z_$][\w$]*(?:\s*\.\s*[A-Za-z_$][\w$]*)*)\s*\(/);
  return call ? call[1].replace(/\s+/g, "") : "custom";
}
