// The lines above a search's results: what it could not reach, from what its
// answer says. A part that could not answer, and how many of the documents
// it could have found have no vectors for search by meaning.

export type SearchEcho = {
  partial?: string | null;
  without_vectors?: { total: number; by_source: [string, number][] } | null;
};

export type SearchNotice = { text: string; title?: string };

export function searchNotices(echo: SearchEcho | null): SearchNotice[] {
  const notices: SearchNotice[] = [];
  if (echo?.partial) notices.push({ text: echo.partial });
  const missing = echo?.without_vectors;
  if (missing && missing.total > 0) {
    const count = missing.total.toLocaleString();
    const sources = missing.by_source.map(([id, n]) => `${id} ${n.toLocaleString()}`).join(", ");
    notices.push({
      text:
        missing.total === 1
          ? "1 candidate lacks semantic embedding vectors"
          : `${count} candidates lack semantic embedding vectors`,
      title: `Search by meaning cannot reach these yet: ${sources}`,
    });
  }
  return notices;
}
