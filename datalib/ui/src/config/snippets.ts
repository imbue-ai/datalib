// Quick-add source templates for the Sources tab. Each body is a pair
// of adjacent `[[steps]]` tables appended to the DAG config: the
// source's download step plus its render step. Each step is a
// `command` invoking `datalib-step`; the subcommand names the
// provider, so params carry no `type` tag, and the source name comes
// from the step's first output ("slack/raw" → slack; see
// `datalib_dag::config`). Params are per-phase: the download step
// carries the provider's download config; the render step needs none
// for any of these providers (render-side knobs like beeper's
// `period` would go on it). Credentials are never here — they come
// from latchkey at runtime. Bodies are functions so date-dependent
// parts (Slack's `since`) and the install-specific latchkey CLI hint
// are computed at click time.
//
// Note these are appended to the *end* of the file, which is the only
// place a `[[steps]]` table can safely go: in TOML every key after a
// table header belongs to that table, so inserting mid-file would
// silently reparent whatever followed.

// YYYY-MM-DD for `n` days before today (UTC).
function isoDaysAgo(days: number): string {
  return new Date(Date.now() - days * 86_400_000).toISOString().slice(0, 10);
}

// The standard download+render step pair for one source, preceded by a
// light divider so sources stay visually separated in the raw file.
// `params` is the download step's `[steps.params]` body — written as
// TOML sub-table headers, so it must come last within its step.
// `preamble` (optional) is comment lines placed between the divider
// and the steps.
function stepPair(
  name: string,
  type: string,
  params: string,
  preamble = "",
): string {
  const divider = `# ── ${name} ${"─".repeat(Math.max(4, 66 - name.length))}`;
  // Instruction preambles get a closing divider so the guidance reads
  // as its own block, visually separate from the steps below.
  const preambleBlock = preamble ? `${preamble}# ${"─".repeat(70)}\n` : "";
  return `${divider}
${preambleBlock}[[steps]]
id = "${name}/raw"
command = "datalib-step download ${type}"
${params}

[[steps]]
id = "${name}/rendered_md"
command = "datalib-step render ${type}"
inputs = ["${name}/raw"]`;
}

export type Snippet = { label: string; body: (latchkeyCli: string) => string };

export const SNIPPETS: Snippet[] = [
  {
    label: "Claude",
    body: (lk) =>
      stepPair(
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
    body: () => stepPair("chatgpt", "chatgpt_api", "[steps.params]\nsync = {}"),
  },
  {
    // `since` starts the backfill 30 days back so the first sync stays
    // small; users widen it once they've seen a sync succeed.
    label: "Slack",
    body: () =>
      stepPair(
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
    body: () => stepPair("github", "github_api", "[steps.params]\nsync = {}"),
  },
  {
    label: "GitLab",
    body: () => stepPair("gitlab", "gitlab_api", "[steps.params]\nsync = {}"),
  },
  {
    label: "Email (JMAP)",
    body: () =>
      stepPair(
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
      stepPair(
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
    body: () => stepPair("perseus", "perseus", "[steps.params]\nsync = {}"),
  },
];
