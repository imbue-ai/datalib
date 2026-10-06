// The line under the grid that says how much of the corpus free-text
// search reaches. qmd keeps two indexes: the keyword (BM25) one covers
// every document the index holds, the embeddings behind semantic search
// only those embedded so far, and embedding runs later and slower.

import type { QmdStateResponse } from "../api";

export type SearchCoverage = { text: string; title: string };

export function searchCoverage(
  r: Pick<QmdStateResponse, "index_present" | "summary" | "errors">,
): SearchCoverage {
  if (!r.index_present) {
    return (r.errors?.length ?? 0) > 0
      ? {
          text: "search index could not be read",
          title: "Free-text search finds nothing until the search index can be opened.",
        }
      : {
          text: "search index not built yet — sync to build it",
          title: "Free-text search finds nothing until the first sync builds the search index.",
        };
  }
  const { documents, embedded } = r.summary;
  const waiting = documents - embedded;
  return {
    text:
      `${documents.toLocaleString()} documents searchable · ` +
      `${embedded.toLocaleString()} with semantic search`,
    title:
      waiting > 0
        ? `Keyword search reaches all ${documents.toLocaleString()}. ` +
          `${waiting.toLocaleString()} are still waiting on embeddings, ` +
          `so semantic search cannot reach them yet.`
        : "Every indexed document has embeddings, so keyword and semantic search both reach all of them.",
  };
}
