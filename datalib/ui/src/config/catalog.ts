// The source catalog the "Add Data Source" picker renders, and the form
// descriptors the wizard fills in.
//
// TEMPORARY HOME. The design (docs/dev/source_wizard.md) puts each
// entry next to its provider's schema, in the `*_config` crate, served
// from `GET /api/sources/catalog`. Nothing serves that yet, so the
// table lives here — which means it can drift from the Rust structs it
// mirrors. When the endpoint lands, delete this file and fetch instead;
// the shapes below are deliberately the shapes that endpoint should
// return, so the swap is a fetch and a type import.
//
// Five types carry descriptors: `slack_api` and `claude_api` for the
// credentialed path, and `lightroom` / `signal_backup` /
// `whatsapp_backup` for the on-disk one. Every other type is listed so
// the picker shows the real breadth of what datalib supports, but is
// marked `wizard: false` — picking one sends you to the config editor
// rather than pretending a form exists.

/// A form field, mapped onto a dotted path into a step's `params` tree
/// (`sync.channels` → `[steps.params.sync] channels`).
///
/// `phase` says which of the source's two steps the value lands on.
/// It defaults to `download`; the render step carries only render-time
/// knobs, and the two params schemas are `deny_unknown_fields` on the
/// Rust side, so putting a value on the wrong step fails loudly.
export type FieldPhase = "download" | "render";

type FieldBase = {
  target: string;
  label: string;
  help?: string;
  phase?: FieldPhase;
};

export type Field =
  | ({ kind: "text" } & FieldBase & { placeholder?: string; required?: boolean })
  /// A path on the machine running the backend. Typed for now — the
  /// design's file picker needs a `GET /api/fs/browse` endpoint, since
  /// the UI has no Tauri IPC and is also served to a plain browser.
  | ({ kind: "path" } & FieldBase & { placeholder?: string; required?: boolean; picks?: "file" | "dir" })
  | ({ kind: "date" } & FieldBase)
  | ({ kind: "bool" } & FieldBase & { default?: boolean })
  | ({ kind: "int" } & FieldBase)
  | ({ kind: "string_list" } & FieldBase & { placeholder?: string });

export type CatalogEntry = {
  /// The `datalib-step download|render <type>` word.
  type: string;
  label: string;
  blurb: string;
  /// Matched by the picker's filter box alongside label and type.
  keywords: string[];
  /// Grouping in the picker.
  kind: "api" | "export" | "local";
  /// `ui/src/assets/<icon>.svg`, or null for the per-kind fallback.
  icon: string | null;
  /// Seeds the source name, and thus the step ids and artifact paths.
  defaultName: string;
  /// False → in the picker for completeness, but no form exists yet.
  wizard: boolean;
  /// False for download-only providers, which render nothing and so
  /// declare no render step (`lightroom`, `fsindex`). Defaults to true.
  renderStep?: boolean;
  /// The latchkey service name, when the source needs credentials.
  /// Used only for the credential UI: at request time latchkey picks
  /// the service by matching the URL, not by this string.
  credentialService?: string;
  fields?: Field[];
};

/// Fields shared by the two wizard-capable sources. Kept inline per
/// entry rather than factored out — the design's whole point is that a
/// descriptor is data owned by one provider, not a class hierarchy.
export const CATALOG: CatalogEntry[] = [
  {
    type: "slack_api",
    label: "Slack",
    blurb: "Mirror channels and DMs from one Slack workspace.",
    keywords: ["slack", "chat", "workspace", "channels", "messages"],
    kind: "api",
    icon: "slack",
    defaultName: "slack",
    wizard: true,
    credentialService: "slack",
    fields: [
      {
        kind: "string_list",
        target: "sync.channels",
        label: "Channels",
        placeholder: "general, engineering",
        help:
          "Channel names without the #. Leave empty for every channel you're a member of. " +
          "A live picker replaces this once the probe endpoint exists.",
      },
      {
        kind: "date",
        target: "sync.since",
        label: "Mirror messages since",
        help:
          "Oldest message to fetch (YYYY-MM-DD). This is what decides how far back the " +
          "mirror goes. Moving it earlier backfills on the next run; moving it later does nothing.",
      },
      {
        kind: "bool",
        target: "sync.media",
        label: "Download file attachments",
        default: true,
        help: "Off stores JSON metadata only.",
      },
      {
        kind: "bool",
        target: "sync.all_channels",
        label: "Include channels you're not a member of",
        default: false,
        help: "Ignored when Channels is set.",
      },
      {
        kind: "int",
        target: "sync.refresh_window_days",
        label: "Edit-catcher window (days)",
        help:
          "Re-query the trailing N days of channels that already have history, to pick up " +
          "edits and reactions. NOT a range bound — it only adds work. Leave empty for none.",
      },
    ],
  },
  {
    type: "claude_api",
    label: "Claude",
    blurb: "Mirror your claude.ai conversations and projects.",
    keywords: ["claude", "anthropic", "chat", "llm", "conversations"],
    kind: "api",
    icon: "claude",
    defaultName: "claude",
    wizard: true,
    credentialService: "claude-ai",
    fields: [
      {
        kind: "date",
        target: "sync.since",
        label: "Mirror conversations updated since",
        help: "YYYY-MM-DD. Leave empty to sync everything.",
      },
      {
        kind: "bool",
        target: "sync.projects",
        label: "Also mirror Claude Projects",
        default: true,
        help:
          "Each project's description, custom instructions and knowledge documents. " +
          "A project is often the only place some written context lives.",
      },
      {
        kind: "int",
        target: "sync.refresh_most_recent_n_chat_count",
        label: "Force-refresh the N most recent chats each run",
        help: "Leave empty to rely on updated_at alone.",
      },
      {
        kind: "string_list",
        target: "sync.conv_uuids",
        label: "Only these conversations",
        placeholder: "https://claude.ai/chat/…",
        help:
          "Bare UUIDs or paste-able chat URLs. Leave empty to walk everything — this is a " +
          "scoping tool for a first run against a large account.",
      },
    ],
  },

  // Listed for completeness; no form yet.
  { type: "chatgpt_api", label: "ChatGPT", blurb: "Mirror your ChatGPT conversations.", keywords: ["chatgpt", "openai", "gpt"], kind: "api", icon: "chatgpt", defaultName: "chatgpt", wizard: false, credentialService: "chatgpt" },
  { type: "github_api", label: "GitHub", blurb: "Mirror pull requests and their review threads.", keywords: ["github", "pr", "code", "review"], kind: "api", icon: "github", defaultName: "github", wizard: false, credentialService: "github" },
  { type: "gitlab_api", label: "GitLab", blurb: "Mirror merge requests and their discussions.", keywords: ["gitlab", "mr", "code"], kind: "api", icon: "gitlab", defaultName: "gitlab", wizard: false, credentialService: "gitlab" },
  { type: "notion_api", label: "Notion", blurb: "Mirror pages and comment threads.", keywords: ["notion", "wiki", "docs", "pages"], kind: "api", icon: "notion", defaultName: "notion", wizard: false, credentialService: "notion" },
  { type: "email", label: "Email", blurb: "Mirror mail over JMAP, the Gmail API, or a Takeout mbox.", keywords: ["email", "mail", "gmail", "fastmail", "jmap", "imap", "mbox"], kind: "api", icon: "email", defaultName: "email", wizard: false, credentialService: "fastmail" },
  { type: "carddav", label: "Contacts", blurb: "Mirror contacts from a CardDAV server or .vcf files.", keywords: ["contacts", "carddav", "vcard", "address book"], kind: "api", icon: null, defaultName: "contacts", wizard: false },
  { type: "yolink", label: "YoLink sensors", blurb: "Per-device temperature, humidity and water history.", keywords: ["yolink", "sensor", "temperature", "iot"], kind: "api", icon: null, defaultName: "yolink", wizard: false },

  { type: "claude_export", label: "Claude export", blurb: "Render an unpacked Claude data export already on disk.", keywords: ["claude", "anthropic", "export", "backup"], kind: "export", icon: "claude", defaultName: "claude-export", wizard: false },
  { type: "google_takeout", label: "Google Takeout", blurb: "Google Chat, Voice, Maps and YouTube from an export.", keywords: ["google", "takeout", "chat", "voice", "youtube"], kind: "export", icon: null, defaultName: "google-takeout", wizard: false },
  { type: "linkedin", label: "LinkedIn", blurb: "Messages and connections from a data export.", keywords: ["linkedin", "export", "connections"], kind: "export", icon: "linkedin", defaultName: "linkedin", wizard: false },
  {
    type: "signal_backup",
    label: "Signal",
    blurb: "Decrypt and mirror an Android Signal backup.",
    keywords: ["signal", "backup", "messages", "sms", "chat"],
    kind: "export",
    icon: "signal",
    defaultName: "signal",
    wizard: true,
    fields: [
      {
        kind: "path",
        picks: "dir",
        required: true,
        target: "sync.snapshot_dir",
        label: "Backup folder",
        placeholder: "~/backups/SignalBackups",
        help:
          "The folder holding your signal-backup-* snapshots, pulled off the phone. " +
          "The newest snapshot in it is the one decrypted.",
      },
      {
        kind: "text",
        target: "sync.aep_env_var",
        label: "Passphrase environment variable",
        placeholder: "SIGNAL_BACKUP_PASSPHRASE",
        help:
          "Name of the env var holding the backup passphrase — not the passphrase itself. " +
          "The backend reads it at download time. Leave empty for the default.",
      },
      {
        kind: "text",
        target: "period",
        phase: "render",
        label: "Document span",
        placeholder: "month",
        help: "How much of a conversation goes in one rendered page: day, month, year or all.",
      },
    ],
  },
  {
    type: "whatsapp_backup",
    label: "WhatsApp",
    blurb: "Decrypt and mirror an Android crypt15 backup.",
    keywords: ["whatsapp", "backup", "messages", "chat"],
    kind: "export",
    icon: "whatsapp",
    defaultName: "whatsapp",
    wizard: true,
    fields: [
      {
        kind: "path",
        picks: "dir",
        required: true,
        target: "sync.backup_dir",
        label: "WhatsApp folder",
        placeholder: "~/backups/WhatsApp",
        help:
          "The WhatsApp/ directory pulled off the phone — the one containing " +
          "Databases/msgstore.db.crypt15 and a Media/ tree.",
      },
      {
        kind: "text",
        target: "sync.key_env_var",
        label: "Decryption-key environment variable",
        placeholder: "WHATSAPP_BACKUP_DECRYPTION_KEY",
        help:
          "Name of the env var holding the hex-encoded 32-byte root key — not the key " +
          "itself. Leave empty for the default.",
      },
    ],
  },
  { type: "sms_backup_restore", label: "SMS & calls", blurb: "Android SMS Backup & Restore XML exports.", keywords: ["sms", "mms", "calls", "android", "texts"], kind: "export", icon: "sms", defaultName: "sms", wizard: false },
  { type: "beeper", label: "Beeper", blurb: "Read Beeper Texts' local store across its networks.", keywords: ["beeper", "matrix", "chat", "imessage"], kind: "export", icon: null, defaultName: "beeper", wizard: false },

  { type: "pdf", label: "PDFs", blurb: "Convert a directory tree of PDFs into searchable markdown.", keywords: ["pdf", "documents", "papers", "files"], kind: "local", icon: null, defaultName: "pdfs", wizard: false },
  { type: "fsindex", label: "File index", blurb: "Index a directory tree — paths, sizes, content hashes.", keywords: ["files", "filesystem", "index", "directory", "disk"], kind: "local", icon: null, defaultName: "fsindex", wizard: false },
  {
    type: "lightroom",
    label: "Lightroom",
    blurb: "Mirror a Lightroom Classic catalog, with full history.",
    keywords: ["lightroom", "photos", "adobe", "catalog", "sqlite", "images"],
    kind: "local",
    icon: null,
    defaultName: "lightroom",
    wizard: true,
    // Download-only: a photo catalog isn't chat-shaped, so nothing is
    // rendered and no render step is declared.
    renderStep: false,
    fields: [
      {
        kind: "path",
        picks: "file",
        required: true,
        target: "common.input_path",
        label: "Catalog file",
        placeholder: "~/Pictures/Lightroom/Lightroom Catalog-v14.lrcat",
        help:
          "A .lrcat, which is an ordinary SQLite database. Every table is mirrored, and " +
          "doltlite stores only what changed between runs — so prior states stay queryable.",
      },
      {
        kind: "bool",
        target: "skip_xmp",
        label: "Skip XMP packets and search indexes",
        default: false,
        help:
          "The bulkiest columns in a catalog, and wholly derived from columns that stay. " +
          "Off by default: a backup should be faithful unless you say otherwise.",
      },
      {
        kind: "bool",
        target: "snapshot",
        label: "Snapshot before reading",
        default: true,
        help:
          "Take a VACUUM INTO copy first, so a catalog Lightroom has open can't be read " +
          "half-written.",
      },
      {
        kind: "bool",
        target: "gc",
        label: "Collect unreachable chunks each run",
        default: false,
        help:
          "Much smaller store, history unaffected — but it rewrites the whole chunk store " +
          "every run.",
      },
    ],
  },
  { type: "perseus", label: "Perseus library", blurb: "Classical texts from the Perseus Digital Library.", keywords: ["perseus", "greek", "latin", "classics", "sample"], kind: "local", icon: null, defaultName: "perseus", wizard: false },
];

export const KIND_LABELS: Record<CatalogEntry["kind"], string> = {
  api: "Connected accounts",
  export: "Exports & backups",
  local: "On this computer",
};

export function catalogFor(type: string): CatalogEntry | undefined {
  return CATALOG.find((e) => e.type === type);
}

/// Substring match over label, type and keywords. Deliberately not
/// fuzzy: with ~20 entries a fuzzy matcher mostly adds surprise.
export function filterCatalog(query: string): CatalogEntry[] {
  const q = query.trim().toLowerCase();
  if (!q) return CATALOG;
  return CATALOG.filter((e) =>
    [e.label, e.type, ...e.keywords].some((s) => s.toLowerCase().includes(q)),
  );
}
