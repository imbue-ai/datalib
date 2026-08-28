// Catalog icon name → bundled asset URL.
//
// `ui/src/assets/` carries brand marks for eleven services; the rest of
// the catalog has none, so `iconUrl` returns null and callers render a
// per-kind glyph. Used nominatively to identify a service — don't
// restyle or recolor them.

import chatgptIconUrl from "@/assets/chatgpt.svg";
import claudeIconUrl from "@/assets/claude.svg";
import emailIconUrl from "@/assets/email.svg";
import githubIconUrl from "@/assets/github.svg";
import gitlabIconUrl from "@/assets/gitlab.svg";
import linkedinIconUrl from "@/assets/linkedin.svg";
import notionIconUrl from "@/assets/notion.svg";
import signalIconUrl from "@/assets/signal.svg";
import slackIconUrl from "@/assets/slack.svg";
import smsIconUrl from "@/assets/sms.svg";
import whatsappIconUrl from "@/assets/whatsapp.svg";

const ICONS: Record<string, string> = {
  chatgpt: chatgptIconUrl,
  claude: claudeIconUrl,
  email: emailIconUrl,
  github: githubIconUrl,
  gitlab: gitlabIconUrl,
  linkedin: linkedinIconUrl,
  notion: notionIconUrl,
  signal: signalIconUrl,
  slack: slackIconUrl,
  sms: smsIconUrl,
  whatsapp: whatsappIconUrl,
};

export function iconUrl(name: string | null | undefined): string | null {
  return name ? (ICONS[name] ?? null) : null;
}
