// Quick-add source templates for the Sources tab. Each body is a
// self-contained `sources:` list item (two-space indented, `- ` marker
// on the first line) that gets appended to the raw config text. The
// orchestrator owns only `name`/`enabled`; everything provider-owned
// (including `type:`) nests under `source:` — see
// `frankweiler/backend/ingest_config`. Credentials are never here —
// they come from latchkey at runtime. Bodies are functions so
// date-dependent parts (Slack's `since:`) and the install-specific
// latchkey CLI hint are computed at click time.

// YYYY-MM-DD for `n` days before today (UTC).
function isoDaysAgo(days: number): string {
  return new Date(Date.now() - days * 86_400_000).toISOString().slice(0, 10);
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
  - name: claude
    source:
      type: claude_api
      sync: {}`,
  },
  {
    label: "ChatGPT",
    body: () => `  - name: chatgpt
    source:
      type: chatgpt_api
      sync: {}`,
  },
  {
    // `since:` starts the backfill 30 days back so the first sync stays
    // small; users widen it once they've seen a sync succeed.
    label: "Slack",
    body: () => `  - name: slack
    source:
      type: slack_api
      sync:
        media: true
        channels: ["general"]
        since: "${isoDaysAgo(30)}"`,
  },
  {
    label: "GitHub",
    body: () => `  - name: github
    source:
      type: github_api
      sync: {}`,
  },
  {
    label: "GitLab",
    body: () => `  - name: gitlab
    source:
      type: gitlab_api
      sync: {}`,
  },
  {
    label: "Email (JMAP)",
    body: () => `  - name: fastmail
    source:
      type: email
      sync:
        hostname: api.fastmail.com`,
  },
  {
    // `input_path` is part of the shared per-source envelope, so it
    // lives under `common:`, not at the top of `source:`.
    label: "Contacts (vCard)",
    body: () => `  - name: contacts
    source:
      type: carddav
      common:
        input_path: ~/Downloads/contacts.vcf`,
  },
  {
    // Sample public source — no latchkey needed. Bare `sync: {}` pulls
    // the default Thucydides Histories (Greek + English) from PerseusDL.
    label: "Perseus (sample)",
    body: () => `  - name: perseus
    source:
      type: perseus
      sync: {}`,
  },
];
