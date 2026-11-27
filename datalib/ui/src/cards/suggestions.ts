// What agents suggested for the contacts that waits for the person
// (`GET /applet/datalib_contacts/suggestions`), and how a suggestion is
// put to them: the contacts card's suggestions and a person's page say
// it the same way. None of it is a contact, a link or a label anything
// else reads until Yes. docs/dev/contacts.md § "Agents propose, a person
// decides".

import { CONTACTS_APPLET } from "./contacts";

/** `datalib_contacts::REAL`, hand-kept: the label agents curate, in when
 *  someone or something is not spam. */
export const REAL = "real";

export type SuggestedLabel = { label: string; applies: boolean };

/** One suggestion, as the applet serves it. */
export type Suggestion = {
  /** What Yes (`/review/confirm`) and No (`/review/reject`) are sent for. */
  subject: string;
  contact_id: string | null;
  name: string | null;
  /** The agent made the contact, and none of it is the person's yet. */
  new_contact: boolean;
  /** The handle the labels are about, for a handle's own labels. */
  handle: string | null;
  /** The handles the agent says are this contact's. */
  handles: string[];
  /** What the contact has already. */
  held: string[];
  labels: SuggestedLabel[];
  agents: string[];
  runs: string[];
  reasons: string[];
  confidence: number | null;
};

export async function loadSuggestions(): Promise<Suggestion[]> {
  const r = await fetch(`${CONTACTS_APPLET}/suggestions`);
  if (!r.ok) throw new Error(`suggestions → ${r.status}: ${await r.text()}`);
  return ((await r.json()) as { suggestions: Suggestion[] }).suggestions;
}

/** A suggestion in words: a sentence around the name it is about, why,
 *  and who said it. "These two are **Will Riker**", "This is also
 *  **Deanna Troi**", "**Q** is not real (spam)", "This is not real
 *  (spam)" beside a handle's chip. */
export type Said = {
  before: string;
  name: string | null;
  after: string;
  because: string | null;
  by: string;
};

const COUNT = ["", "This", "These two", "These three", "These four"];

function these(n: number): string {
  return n === 1 ? "This is" : `${COUNT[n] ?? `These ${n}`} are`;
}

export function labelPhrase(l: SuggestedLabel): string {
  if (l.label === REAL) return l.applies ? "real" : "not real (spam)";
  return l.applies ? `in “${l.label}”` : `not in “${l.label}”`;
}

export function said(s: Suggestion): Said {
  const n = s.handles.length;
  const labels = s.labels.map(labelPhrase).join(", ");
  const runs = s.runs.length ? ` (${s.runs.join(", ")})` : "";
  const why = {
    because: s.reasons.length ? s.reasons.map((r) => r.replace(/[.\s]+$/, "")).join("; ") : null,
    by: `${s.agents.join(" and ") || "an agent"}${runs}`,
  };
  if (n > 0) {
    return {
      before: s.new_contact ? these(n) : `${these(n)} also`,
      name: s.name,
      after: labels ? `, who is ${labels}` : "",
      ...why,
    };
  }
  if (s.handle === null && s.name)
    return { before: "", name: s.name, after: ` is ${labels}`, ...why };
  return { before: `This is ${labels}`, name: null, after: "", ...why };
}

/** The suggestions a page about `handles` (and the contact `contactId`,
 *  where it is one) should put to the person: any naming one of them, or
 *  about that contact. */
export function suggestionsAbout(
  all: Suggestion[],
  handles: Iterable<string>,
  contactId: string | null,
): Suggestion[] {
  const mine = new Set(handles);
  return all.filter(
    (s) =>
      (contactId !== null && s.contact_id === contactId) ||
      [...s.handles, ...s.held, ...(s.handle ? [s.handle] : [])].some((h) => mine.has(h)),
  );
}

/** One run of one agent, and the subjects it suggested something for:
 *  what *Yes to all* agrees with. A suggestion more than one run made is
 *  in each. */
export type Run = { agent: string; run: string | null; subjects: string[] };

export function runsOf(all: Suggestion[]): Run[] {
  const out: Run[] = [];
  for (const s of all) {
    const agents = s.agents.length ? s.agents : ["an agent"];
    const runs = s.runs.length ? s.runs : [null];
    for (const agent of agents) {
      for (const run of runs) {
        let r = out.find((o) => o.agent === agent && o.run === run);
        if (!r) out.push((r = { agent, run, subjects: [] }));
        if (!r.subjects.includes(s.subject)) r.subjects.push(s.subject);
      }
    }
  }
  return out;
}
