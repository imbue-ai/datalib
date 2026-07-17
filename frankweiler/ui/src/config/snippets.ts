// Quick-add source templates for the Sources tab. Each body is a
// pair of adjacent step entries (two-space indented, `- ` marker on
// each) appended to the `steps:` list of the DAG config: the source's
// `<type>.download` step plus its `<type>.render` step, sharing one
// params block via a YAML anchor. The step type names the provider,
// so params carry no `type:` tag — `source:` is the provider's own
// config subtree (see `frankweiler_dag::config`). Credentials are
// never here — they come from latchkey at runtime. Bodies are
// functions so date-dependent parts (Slack's `since:`) and the
// install-specific latchkey CLI hint are computed at click time.

// YYYY-MM-DD for `n` days before today (UTC).
function isoDaysAgo(days: number): string {
  return new Date(Date.now() - days * 86_400_000).toISOString().slice(0, 10);
}

// The standard download+render step pair for one source. `sourceYaml`
// is the `source:` subtree body, indented 8 spaces per line already.
function stepPair(name: string, type: string, sourceYaml: string): string {
  return `  - id: ${name}.download
    outputs: [${name}/raw]
    step: ${type}.download
    params: &${name}
      name: ${name}
      source:
${sourceYaml}
  - id: ${name}.render
    inputs: [${name}/raw]
    outputs: [${name}/rendered_md]
    step: ${type}.render
    params: *${name}`;
}

export type Snippet = { label: string; body: (latchkeyCli: string) => string };

export const SNIPPETS: Snippet[] = [
  {
    label: "Claude",
    body: (lk) => `  # Prerequisite (one-time): register claude.ai with latchkey and
  # supply your sessionKey cookie (DevTools → Application → Cookies):
  #   ${lk} services register claude-ai --base-api-url="https://claude.ai/"
  #   ${lk} auth set claude-ai -H "Cookie: sessionKey=$(pbpaste)"
  # See docs/user/getting_your_data.md for the full walkthrough.
${stepPair("claude", "claude_api", "        sync: {}")}`,
  },
  {
    label: "ChatGPT",
    body: () => stepPair("chatgpt", "chatgpt_api", "        sync: {}"),
  },
  {
    // `since:` starts the backfill 30 days back so the first sync stays
    // small; users widen it once they've seen a sync succeed.
    label: "Slack",
    body: () =>
      stepPair(
        "slack",
        "slack_api",
        `        sync:
          media: true
          channels: ["general"]
          since: "${isoDaysAgo(30)}"`,
      ),
  },
  {
    label: "GitHub",
    body: () => stepPair("github", "github_api", "        sync: {}"),
  },
  {
    label: "GitLab",
    body: () => stepPair("gitlab", "gitlab_api", "        sync: {}"),
  },
  {
    label: "Email (JMAP)",
    body: () =>
      stepPair(
        "fastmail",
        "email",
        `        sync:
          hostname: api.fastmail.com`,
      ),
  },
  {
    // `input_path` is part of the shared per-source envelope, so it
    // lives under `common:`, not at the top of `source:`.
    label: "Contacts (vCard)",
    body: () =>
      stepPair(
        "contacts",
        "carddav",
        `        common:
          input_path: ~/Downloads/contacts.vcf`,
      ),
  },
  {
    // Sample public source — no latchkey needed. Bare `sync: {}` pulls
    // the default Thucydides Histories (Greek + English) from PerseusDL.
    label: "Perseus (sample)",
    body: () => stepPair("perseus", "perseus", "        sync: {}"),
  },
];
