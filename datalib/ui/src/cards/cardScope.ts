// Which card a request is for. A fetch started synchronously inside
// `inCard` carries the card's id and type (the wrapper in telemetry.ts),
// and the server's request log records both. Like the frame chain in
// live.ts, the scope ends at the first await, so an api.ts function
// must call fetch before it awaits anything (tests/card_api.test.ts).

export const CARD_HEADER = "X-Datalib-Card";
export const CARD_TYPE_HEADER = "X-Datalib-Card-Type";

export type CardTag = { id: string; type: string };

let serving: CardTag | null = null;

export function cardBeingServed(): CardTag | null {
  return serving;
}

export function inCard<T>(tag: CardTag, fn: () => T): T {
  const outer = serving;
  serving = tag;
  try {
    return fn();
  } finally {
    serving = outer;
  }
}
