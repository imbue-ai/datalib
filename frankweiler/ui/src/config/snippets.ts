// Quick-add source templates for the Sources tab. Each body is a
// pair of adjacent step entries (two-space indented, `- ` marker on
// each) appended to the `steps:` list of the DAG config: the source's
// download step plus its render step. Each step is a `command:`
// invoking `datalib-step`; the subcommand names the provider, so
// params carry no `type:` tag, and the source name comes from the
// step's first output (`slack/raw` → `slack`; see
// `frankweiler_dag::config`). Params are per-phase: the download step
// carries the provider's download config; the render step needs none
// for any of these providers (render-side knobs like beeper's
// `period` would go on it). Credentials are never here — they come
// from latchkey at runtime. Bodies are functions so date-dependent
// parts (Slack's `since:`) and the install-specific latchkey CLI hint
// are computed at click time.

// YYYY-MM-DD for `n` days before today (UTC).
function isoDaysAgo(days: number): string {
  return new Date(Date.now() - days * 86_400_000).toISOString().slice(0, 10);
}

// The standard download+render step pair for one source, preceded by a
// light divider so sources stay visually separated in the raw file.
// `sourceYaml` is the params subtree body, indented 6 spaces per line
// already; `preamble` (optional) is comment lines placed between the
// divider and the steps.
function stepPair(
  name: string,
  type: string,
  sourceYaml: string,
  preamble = "",
): string {
  const divider = `  # \u2500\u2500 ${name} ${"\u2500".repeat(Math.max(4, 66 - name.length))}`;
  // Instruction preambles get a closing divider so the guidance reads
  // as its own block, visually separate from the steps below.
  const preambleBlock = preamble
    ? `${preamble}  # ${"\u2500".repeat(70)}\n`
    : "";
  return `${divider}
${preambleBlock}  - id: ${name}.download
    command: datalib-step download ${type}
    outputs: [${name}/raw]
    params:
${sourceYaml}
  - id: ${name}.render
    command: datalib-step render ${type}
    inputs: [${name}/raw]
    outputs: [${name}/rendered_md]`;
}

export type Snippet = { label: string; body: (latchkeyCli: string) => string };

export const SNIPPETS: Snippet[] = [
  {
    label: "Claude",
    body: (lk) =>
      stepPair(
        "claude",
        "claude_api",
        "      sync: {}",
        `  # Prerequisite (one-time): register claude.ai with latchkey and
  # supply your sessionKey cookie (DevTools → Application → Cookies):
  #   ${lk} services register claude-ai --base-api-url="https://claude.ai/"
  #   ${lk} auth set claude-ai -H "Cookie: sessionKey=$(pbpaste)"
  # See docs/user/getting_your_data.md for the full walkthrough.
`,
      ),
  },
  {
    label: "ChatGPT",
    body: () => stepPair("chatgpt", "chatgpt_api", "      sync: {}"),
  },
  {
    // `since:` starts the backfill 30 days back so the first sync stays
    // small; users widen it once they've seen a sync succeed.
    label: "Slack",
    body: () =>
      stepPair(
        "slack",
        "slack_api",
        `      sync:
        media: true
        channels: ["general"]
        since: "${isoDaysAgo(30)}"`,
      ),
  },
  {
    label: "GitHub",
    body: () => stepPair("github", "github_api", "      sync: {}"),
  },
  {
    label: "GitLab",
    body: () => stepPair("gitlab", "gitlab_api", "      sync: {}"),
  },
  {
    label: "Email (JMAP)",
    body: () =>
      stepPair(
        "fastmail",
        "email",
        `      sync:
        hostname: api.fastmail.com`,
      ),
  },
  {
    // `input_path` is part of the shared per-source envelope, so it
    // lives under `common:`, not at the top of the params.
    label: "Contacts (vCard)",
    body: () =>
      stepPair(
        "contacts",
        "carddav",
        `      common:
        input_path: ~/Downloads/contacts.vcf`,
      ),
  },
  {
    // Sample public source — no latchkey needed. Bare `sync: {}` pulls
    // the default Thucydides Histories (Greek + English) from PerseusDL.
    label: "Perseus (sample)",
    body: () => stepPair("perseus", "perseus", "      sync: {}"),
  },
];
