// The lines above a search's results: what it could not reach, from what its
// answer says.

export type SearchEcho = { partial?: string | null };

export type SearchNotice = { text: string };

export function searchNotices(echo: SearchEcho | null): SearchNotice[] {
  return echo?.partial ? [{ text: echo.partial }] : [];
}
