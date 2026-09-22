// Catalog icon name → bundled asset URL. The README's source grid reads
// the same files by path, so a mark added here shows up there too.
//
// Brand marks are Simple Icons (CC0) in the brand's colour. The generic
// glyphs — apple_photos, contacts, fsindex, media, pdf, perseus — are
// Material Design Icons (Apache-2.0) in a mid grey that reads on both
// themes: Apple publishes no vector app icons, and the rest are not
// products.

// IQAir publishes no vector mark; this redraws the favicon
// dashboard.iqair.com serves, a white cross on red, from its 48px bitmap.
import airvisualIconUrl from "@/assets/airvisual.svg";
import appleMessagesIconUrl from "@/assets/apple_messages.svg";
import applePhotosIconUrl from "@/assets/apple_photos.svg";
// PNG: Beeper publishes no vector mark. This is beeper.com's own favicon.
import beeperIconUrl from "@/assets/beeper.png";
import chatgptIconUrl from "@/assets/chatgpt.svg";
import claudeIconUrl from "@/assets/claude.svg";
// A terminal glyph in Claude's colour, so a list that shows both the
// claude and claude_code sources tells them apart.
import claudeCodeIconUrl from "@/assets/claude_code.svg";
// The same terminal glyph in OpenAI's colour, for the same reason next
// to chatgpt.
import codexIconUrl from "@/assets/codex.svg";
import contactsIconUrl from "@/assets/contacts.svg";
// Not a service's mark: a diff group is datalib's own, and its icon is
// the three fates a diff sorts rows into.
import diffIconUrl from "@/assets/diff.svg";
import emailIconUrl from "@/assets/email.svg";
import facebookIconUrl from "@/assets/facebook.svg";
// Fastmail publishes no mark on Simple Icons; this is the vector
// favicon fastmail.com serves for itself.
import fastmailIconUrl from "@/assets/fastmail.svg";
import fsindexIconUrl from "@/assets/fsindex.svg";
import garminIconUrl from "@/assets/garmin.svg";
import githubIconUrl from "@/assets/github.svg";
import gitlabIconUrl from "@/assets/gitlab.svg";
import gmailIconUrl from "@/assets/gmail.svg";
import googleTakeoutIconUrl from "@/assets/google_takeout.svg";
import lightroomIconUrl from "@/assets/lightroom.svg";
import linkedinIconUrl from "@/assets/linkedin.svg";
import mediaIconUrl from "@/assets/media.svg";
import notionIconUrl from "@/assets/notion.svg";
import pdfIconUrl from "@/assets/pdf.svg";
import perseusIconUrl from "@/assets/perseus.svg";
import signalIconUrl from "@/assets/signal.svg";
import slackIconUrl from "@/assets/slack.svg";
import smsIconUrl from "@/assets/sms.svg";
import whatsappIconUrl from "@/assets/whatsapp.svg";
// PNG, not SVG: YoLink publishes no vector mark. This is the circle
// logo shop.yosmart.com serves as its own favicon.
import yolinkIconUrl from "@/assets/yolink.png";

const ICONS: Record<string, string> = {
  airvisual: airvisualIconUrl,
  apple_messages: appleMessagesIconUrl,
  apple_photos: applePhotosIconUrl,
  beeper: beeperIconUrl,
  chatgpt: chatgptIconUrl,
  claude: claudeIconUrl,
  claude_code: claudeCodeIconUrl,
  codex: codexIconUrl,
  contacts: contactsIconUrl,
  diff: diffIconUrl,
  email: emailIconUrl,
  facebook: facebookIconUrl,
  fastmail: fastmailIconUrl,
  fsindex: fsindexIconUrl,
  garmin: garminIconUrl,
  github: githubIconUrl,
  gitlab: gitlabIconUrl,
  gmail: gmailIconUrl,
  google_takeout: googleTakeoutIconUrl,
  lightroom: lightroomIconUrl,
  linkedin: linkedinIconUrl,
  media: mediaIconUrl,
  notion: notionIconUrl,
  pdf: pdfIconUrl,
  perseus: perseusIconUrl,
  signal: signalIconUrl,
  slack: slackIconUrl,
  sms: smsIconUrl,
  whatsapp: whatsappIconUrl,
  yolink: yolinkIconUrl,
};

export function iconUrl(name: string | null | undefined): string | null {
  return name ? (ICONS[name] ?? null) : null;
}
