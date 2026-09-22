// The link for a chain of cards from a layout the URL does not
// describe (the tree, the tiling manager): the chain alone, as a
// miller stack — what a new tab can show of it.
import { encodeColumns } from "@/router/columns";

export function chainHref(sources: string[]): string {
  return encodeColumns(sources.map((code) => ({ code, size: null, state: "" })));
}
