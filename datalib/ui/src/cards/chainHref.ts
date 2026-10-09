// The link for a chain of cards: the chain alone, as `/code/code…` —
// what a new window opens as a tab (router/columns.ts has the format).
import { encodeColumns } from "@/router/columns";

export function chainHref(sources: string[]): string {
  return encodeColumns(sources.map((code) => ({ code, size: null, state: "" })));
}
