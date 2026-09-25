// What a grid says when its search fails: one sentence for the card, and
// the raw error — URL, status and all — for the tooltip.
import { ApiError } from "@/apiError";

export type SearchFailure = { message: string; detail: string };

export function searchFailure(e: unknown): SearchFailure {
  const detail = e instanceof Error ? e.message : String(e);
  if (e instanceof ApiError) {
    const said = (e.detail || `the server answered ${e.status}`).replace(/\.$/, "");
    const lead = e.status === 504 ? "Search timed out" : "Search failed";
    return { message: `${lead}: ${said}.`, detail };
  }
  // `fetch` rejects with a TypeError when the request never got an answer.
  if (e instanceof TypeError) {
    return { message: "Search failed: the server could not be reached.", detail };
  }
  return { message: `Search failed: ${detail.replace(/\.$/, "")}.`, detail };
}
