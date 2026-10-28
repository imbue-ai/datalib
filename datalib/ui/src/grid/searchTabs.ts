// The answers a free-text search has, one tab each: the three merged
// ("best", what a search with no tab gets), then each alone. Which have come
// back, with how many rows, and which one the grid shows. The rule that
// matters is that nothing moves under the reader: Best opens as the search
// starts, and after that only a click changes what is shown. A tab that came
// back while another was open says so until it is looked at.
import type { SearchTab } from "@/api";

export type Answer = "best" | SearchTab;

/// The `tab` a search asks the server for: none, for the merged answer.
export function apiTab(a: Answer): SearchTab | null {
  return a === "best" ? null : a;
}

export const SEARCH_TABS: { id: Answer; label: string; title: string }[] = [
  {
    id: "best",
    label: "Best",
    title: "The other three merged, best match first",
  },
  {
    id: "fields",
    label: "Fields",
    title: "The ids, people, titles and names a row answers to",
  },
  {
    id: "words",
    label: "Words",
    title: "The words anywhere in a document, best match first",
  },
  {
    id: "meaning",
    label: "Meaning",
    title: "Documents about the same thing, whatever words they use",
  },
];

export type TabAnswer =
  | { status: "pending" }
  | { status: "ready"; total: number; seen: boolean }
  | { status: "failed"; why: string };

export type TabsState = {
  q: string;
  open: Answer | null;
  answers: Record<Answer, TabAnswer>;
};

export function startTabs(q: string): TabsState {
  return {
    q,
    open: "best",
    answers: {
      best: { status: "pending" },
      fields: { status: "pending" },
      words: { status: "pending" },
      meaning: { status: "pending" },
    },
  };
}

/// `tab` came back. If the open tab failed, nothing is shown, so the first
/// tab with rows opens; once every tab has come back with none, the first
/// opens, to say so.
export function tabAnswered(s: TabsState, tab: Answer, answer: TabAnswer): TabsState {
  const answers = { ...s.answers, [tab]: answer };
  let open = s.open;
  if (open === tab && answer.status === "failed") open = null;
  if (open === null) {
    open = SEARCH_TABS.map((t) => t.id).find((id) => withRows(answers[id])) ?? null;
  }
  if (open === null && SEARCH_TABS.every((t) => answers[t.id].status !== "pending")) {
    open = SEARCH_TABS[0].id;
  }
  if (open !== null) {
    const shown = answers[open];
    if (shown.status === "ready") answers[open] = { ...shown, seen: true };
  }
  return { ...s, open, answers };
}

/// A count read again, after the index moved: it never opens a tab, and
/// it marks the tab new only when it has rows it did not have.
export function tabRecounted(s: TabsState, tab: Answer, answer: TabAnswer): TabsState {
  const was = s.answers[tab];
  if (answer.status === "ready" && tab !== s.open) {
    const grew = was.status !== "ready" || answer.total > was.total;
    answer = { ...answer, seen: was.status === "ready" && was.seen && !grew };
  }
  if (answer.status === "ready" && tab === s.open) answer = { ...answer, seen: true };
  return { ...s, answers: { ...s.answers, [tab]: answer } };
}

/// The reader picked `tab`. A tab still pending can be picked too: its
/// rows show as they come.
export function tabPicked(s: TabsState, tab: Answer): TabsState {
  const answer = s.answers[tab];
  const answers =
    answer.status === "ready" ? { ...s.answers, [tab]: { ...answer, seen: true } } : s.answers;
  return { ...s, open: tab, answers };
}

/// Whether `tab` came back with rows the reader has not looked at.
export function isNew(s: TabsState, tab: Answer): boolean {
  const answer = s.answers[tab];
  return tab !== s.open && withRows(answer) && !answer.seen;
}

function withRows(answer: TabAnswer): answer is Extract<TabAnswer, { status: "ready" }> {
  return answer.status === "ready" && answer.total > 0;
}
