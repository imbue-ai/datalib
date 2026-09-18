// Catalog icon name → bundled asset URL.

import appleMessagesIconUrl from "@/assets/apple_messages.svg";
import chatgptIconUrl from "@/assets/chatgpt.svg";
import claudeIconUrl from "@/assets/claude.svg";
// Not a service's mark: a diff group is datalib's own, and its icon is
// the three fates a diff sorts rows into.
import diffIconUrl from "@/assets/diff.svg";
import emailIconUrl from "@/assets/email.svg";
import facebookIconUrl from "@/assets/facebook.svg";
// Fastmail publishes no mark on Simple Icons; this is the vector
// favicon fastmail.com serves for itself.
import fastmailIconUrl from "@/assets/fastmail.svg";
import garminIconUrl from "@/assets/garmin.svg";
import githubIconUrl from "@/assets/github.svg";
import gmailIconUrl from "@/assets/gmail.svg";
import gitlabIconUrl from "@/assets/gitlab.svg";
import linkedinIconUrl from "@/assets/linkedin.svg";
import notionIconUrl from "@/assets/notion.svg";
import signalIconUrl from "@/assets/signal.svg";
import slackIconUrl from "@/assets/slack.svg";
import smsIconUrl from "@/assets/sms.svg";
import whatsappIconUrl from "@/assets/whatsapp.svg";
// PNG, not SVG: YoLink publishes no vector mark. This is the circle
// logo shop.yosmart.com serves as its own favicon.
import yolinkIconUrl from "@/assets/yolink.png";

const ICONS: Record<string, string> = {
  apple_messages: appleMessagesIconUrl,
  chatgpt: chatgptIconUrl,
  claude: claudeIconUrl,
  diff: diffIconUrl,
  email: emailIconUrl,
  facebook: facebookIconUrl,
  fastmail: fastmailIconUrl,
  garmin: garminIconUrl,
  github: githubIconUrl,
  gmail: gmailIconUrl,
  gitlab: gitlabIconUrl,
  linkedin: linkedinIconUrl,
  notion: notionIconUrl,
  signal: signalIconUrl,
  slack: slackIconUrl,
  sms: smsIconUrl,
  whatsapp: whatsappIconUrl,
  yolink: yolinkIconUrl,
};

export function iconUrl(name: string | null | undefined): string | null {
  return name ? (ICONS[name] ?? null) : null;
}
