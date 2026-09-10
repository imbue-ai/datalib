// Quick-add source templates for the Sources tab. Each body is one
// source appended to the DAG config — the `[[groups]]` entry, its
// ingest step and its render step — written through the same writers
// the wizard uses (`buildGroup`, `stepToml`), so the shape of a source
// is spelled out in `sourceSteps.ts` and nowhere else. What a snippet
// adds is a hand-written params body for the ingest step: several of
// these providers have no wizard form, and some carry params the
// catalog does not model (`carddav`'s `common.input_path`), which is
// why they go through `stepToml` rather than `buildSource`. Credentials
// are never here — they come from latchkey at runtime. Bodies are
// functions so date-dependent parts (Slack's `since`) and the
// install-specific latchkey CLI hint are computed at click time.

import { buildGroup, stepIdFor, stepToml } from "./sourceSteps";

// YYYY-MM-DD for `n` days before today (UTC).
function isoDaysAgo(days: number): string {
  return new Date(Date.now() - days * 86_400_000).toISOString().slice(0, 10);
}

// One source: its group, then the ingest+render step pair. `params` is
// the ingest step's `[steps.params]` body — written as TOML sub-table
// headers, so it must come last within its step. `preamble` (optional)
// is comment lines placed above the group's divider.
function source(id: string, type: string, params: string, preamble = ""): string {
  const group = buildGroup({ id, name: "", type });
  const ingest = stepToml({ group: id, phase: "download", params });
  const render = stepToml({ group: id, phase: "render", inputs: [stepIdFor(id, "download")] });
  return `${preamble}${group}\n\n${ingest}\n\n${render}`;
}

export type Snippet = { label: string; body: (latchkeyCli: string) => string };

export const SNIPPETS: Snippet[] = [
  {
    label: "Claude",
    body: (lk) =>
      source(
        "claude",
        "claude_api",
        "[steps.params]\nsync = {}",
        `# Prerequisite (one-time): register claude.ai with latchkey and
# supply your sessionKey cookie (DevTools → Application → Cookies):
#   ${lk} services register claude-ai --base-api-url="https://claude.ai/"
#   ${lk} auth set claude-ai -H "Cookie: sessionKey=$(pbpaste)"
# See docs/user/getting_your_data.md for the full walkthrough.
`,
      ),
  },
  {
    label: "ChatGPT",
    body: () => source("chatgpt", "chatgpt_api", "[steps.params]\nsync = {}"),
  },
  {
    // `since` starts the backfill 30 days back so the first sync stays
    // small; users widen it once they've seen a sync succeed.
    label: "Slack",
    body: () =>
      source(
        "slack",
        "slack_api",
        `[steps.params.sync]
media = true
channels = ["general"]
since = "${isoDaysAgo(30)}"`,
      ),
  },
  {
    label: "GitHub",
    body: () => source("github", "github_api", "[steps.params]\nsync = {}"),
  },
  {
    label: "GitLab",
    body: () => source("gitlab", "gitlab_api", "[steps.params]\nsync = {}"),
  },
  {
    label: "Email (JMAP)",
    body: () =>
      source(
        "fastmail",
        "email",
        `[steps.params.sync]
hostname = "api.fastmail.com"`,
      ),
  },
  {
    // `input_path` is part of the shared per-source envelope, so it
    // lives under `common`, not at the top of the params.
    label: "Contacts (vCard)",
    body: () =>
      source(
        "contacts",
        "carddav",
        `[steps.params.common]
input_path = "~/Downloads/contacts.vcf"`,
      ),
  },
  {
    // Sample public source — no latchkey needed. Bare `sync = {}` pulls
    // the default Thucydides Histories (Greek + English) from PerseusDL.
    label: "Perseus (sample)",
    body: () => source("perseus", "perseus", "[steps.params]\nsync = {}"),
  },
];
