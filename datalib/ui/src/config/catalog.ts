// The source catalog the "Add Data Source" picker renders, and the form
// descriptors the wizard fills in.

/// A form field, mapped onto a dotted path into a step's `params` tree
/// (`api.channels` → `[steps.params.api] channels`).
export type FieldPhase = "download" | "render";

type FieldBase = {
  target: string;
  label: string;
  help?: string;
  phase?: FieldPhase;
  /// Only shown, and only written, while the `bool` field at this
  /// target is on.
  requires?: string;
};

export type Field =
  | ({ kind: "text" } & FieldBase & {
      placeholder?: string;
      required?: boolean;
      /// Renders as the latchkey-account control rather than a bare
      /// text box: a dropdown of the accounts latchkey has stored for
      /// the entry's `credentialService`, a "Connect via latchkey"
      /// button, and — still — somewhere to type.
      ///
      /// Typing matters. latchkey may hold an account this server
      /// can't enumerate (no keyring access, latchkey not installed),
      /// and a dropdown that came back empty must not be the only way
      /// in. The value written is the account string either way.
      latchkey?: boolean;
    })
  /// A path on the machine running the backend.
  ///
  /// **A path field must offer a native OS picker** — the rule, and
  /// the checklist for adding one, are in
  /// `docs/dev/wizard_file_pickers.md`. In the desktop app the wizard
  /// opens a real dialog (`ui/src/desktop.ts::pickPath`); in a plain
  /// browser it can't, and the typed input is all there is until a
  /// `GET /api/fs/browse` endpoint exists — `<input type=file>` is no
  /// substitute, since a browser never yields a filesystem path.
  | ({ kind: "path" } & FieldBase & {
      placeholder?: string;
      required?: boolean;
      picks?: "file" | "dir";
      /// Dialog title. Name the thing being chosen ("Choose your
      /// WhatsApp backup folder"), not the widget ("Select folder").
      pickTitle?: string;
      /// `picks: "file"` only — extensions to filter on, no dot. The
      /// typed input stays the escape hatch for anything the filter
      /// wrongly excludes.
      extensions?: string[];
    })
  /// A closed set of values — one Rust enum, one dropdown. Prefer this
  /// over `text` whenever the backend parses the string against a fixed
  /// list: a typo becomes unreachable rather than a sync-time error,
  /// and the options themselves carry the documentation the help text
  /// would otherwise have to spell out.
  ///
  /// `default` must be one of `options` and is what the form starts on,
  /// so the value is always written explicitly — there is no "unset"
  /// choice. Keep it equal to the backend's own default.
  | ({ kind: "select" } & FieldBase & {
      options: { value: string; label: string }[];
      default: string;
    })
  | ({ kind: "date" } & FieldBase)
  | ({ kind: "bool" } & FieldBase & { default?: boolean })
  /// `default` pre-fills the box on a **new** source only, and is
  /// deliberately not applied when editing an existing one.
  | ({ kind: "int" } & FieldBase & { default?: number })
  | ({ kind: "string_list" } & FieldBase & {
      placeholder?: string;
      /// Offer a picker built from `POST /api/probe`, alongside the
      /// comma-separated box. Names *which* of the probe's items this
      /// field takes: every label, only the ones a render filter can
      /// match, or an account's conversations.
      probe?: "labels" | "mailboxes" | "conversations";
    });

export type CatalogEntry = {
  /// The group's `type`: the thing mirrored (`slack`, `email`, …).
  /// Which of a step's params tables reach a live origin and which read
  /// files on disk is not recorded here: `ingestMethods.ts` answers that
  /// from the provider's own declaration, for this type and the params
  /// a form would write.
  type: string;
  /// The params table that selects this entry's ingest method when no
  /// field or preset names it — `api` for a live service whose knobs
  /// are all optional, so that a form with nothing filled in still
  /// writes `api = {}`. A file-backed method has a required `path`
  /// field and needs none.
  method?: string;
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
  /// The latchkey service name, when the source needs credentials. The
  /// wizard shows its Connection section only while the params the form
  /// would write reach an origin (`ingestReach`): an import has nothing
  /// to log in to.
  credentialService?: string;
  /// How to register `credentialService` with latchkey when latchkey
  /// has never heard of it, so that a browser login exists at all. The
  /// login flow belongs to the *service* and is fixed when it is
  /// registered, so this is the only moment it can be chosen; a name
  /// latchkey already holds is left exactly as its owner set it up.
  credentialRegister?: import("@/api").ServiceRegistration;
  /// Shown beside the Connect button, when connecting this way costs
  /// something the person should decide about before clicking.
  credentialConnectWarning?: string;
  /// Dotted params path whose presence identifies this entry among the
  /// several that share one `type`. Undefined on a type with only one
  /// entry, which is nearly all of them.
  ///
  /// Order matters: [`catalogForStep`] takes the first entry whose key
  /// is present, so a more specific key must come first in `CATALOG`.
  variantKey?: string;
  /// Params this entry always writes, with no field to edit them.
  preset?: Preset[];
  /// Offer "Test connection", and populate any `probe:` field from
  /// what comes back. Requires a `datalib-step probe <type>` on the
  /// backend side; see `datalib/backend/datalib_step/src/probe.rs`.
  canProbe?: boolean;
  fields?: Field[];
};

/// A fixed params value, written without being asked about. See
/// [`CatalogEntry.preset`].
export type Preset = {
  target: string;
  value: string | number | boolean;
  /// Which step it lands on. Defaults to `download`, like a field.
  phase?: FieldPhase;
};

/// Fields shared by the two wizard-capable sources. Kept inline per
/// entry rather than factored out — the design's whole point is that a
/// descriptor is data owned by one provider, not a class hierarchy.
export const CATALOG: CatalogEntry[] = [
  {
    type: "slack",
    method: "api",
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
        target: "api.channels",
        label: "Channels",
        placeholder: "general, engineering",
        help:
          "Channel names without the #. Leave empty for every channel you're a member of. " +
          "A live picker replaces this once the probe endpoint exists.",
      },
      {
        kind: "date",
        target: "api.since",
        label: "Mirror messages since",
        help:
          "Oldest message to fetch (YYYY-MM-DD). This is what decides how far back the " +
          "mirror goes. Moving it earlier backfills on the next run; moving it later does nothing.",
      },
      {
        kind: "bool",
        target: "api.media",
        label: "Download file attachments",
        default: true,
        help: "Off stores JSON metadata only.",
      },
      {
        kind: "int",
        target: "common.blob_size_limit_bytes",
        requires: "api.media",
        label: "Skip attachments larger than (bytes)",
        default: 5_000_000,
        help:
          "5 MB by default. A workspace's few largest uploads — screen recordings, design " +
          "files, CI artifacts — are usually most of its bytes on disk and least of its " +
          "text, so a cap here costs little and saves a lot. Raising it later backfills: " +
          "Slack re-walks from the start date when the limit is relaxed. Clear it for no " +
          "limit.",
      },
      {
        kind: "bool",
        target: "api.all_channels",
        label: "Include channels you're not a member of",
        default: false,
        help: "Ignored when Channels is set.",
      },
      {
        kind: "bool",
        target: "api.dms",
        label: "Download direct messages",
        default: false,
        help:
          "Your 1:1 and group DMs, alongside the channels above. Off by default — DMs are " +
          "the most private thing in a workspace, so mirroring them is opt-in.",
      },
      {
        kind: "string_list",
        target: "api.dm_users",
        requires: "api.dms",
        label: "Only DMs with these people",
        placeholder: "@riker, Jean-Luc Picard, U024BE7LH",
        help:
          "Names a person, not a conversation — a Slack handle, display name, real name or " +
          "user id, with or without the @. A group DM counts as a conversation with everyone " +
          "in it. Leave empty for every DM.",
      },
      {
        kind: "int",
        target: "api.refresh_window_days",
        label: "Edit-catcher window (days)",
        help:
          "Re-query the trailing N days of channels that already have history, to pick up " +
          "edits and reactions. NOT a range bound — it only adds work. Leave empty for none.",
      },
    ],
  },
  {
    type: "claude",
    variantKey: "api",
    method: "api",
    label: "Claude",
    blurb: "Mirror your claude.ai conversations and projects.",
    keywords: ["claude", "anthropic", "chat", "llm", "conversations"],
    kind: "api",
    icon: "claude",
    defaultName: "claude",
    wizard: true,
    credentialService: "claude-ai",
    // The whole claude.ai credential is the `sessionKey` cookie, so
    // cookie-capture is the flow that fits.
    credentialRegister: {
      base_api_url: "https://claude.ai/",
      login_url: "https://claude.ai/login",
      login_flow: "cookie-capture",
      login_flow_params: { cookieKeys: ["sessionKey"] },
    },
    credentialConnectWarning:
      "This signs in a second time, and claude.ai appears to evict the older session when it " +
      "does — observed 2026-08-31, the captured cookie and the browser you normally use kept " +
      "logging each other out. Pasting the sessionKey avoids that.",
    canProbe: true,
    fields: [
      {
        kind: "text",
        latchkey: true,
        target: "latchkey_settings.account",
        label: "Claude account",
        placeholder: "you@example.com",
        help:
          "Which stored claude.ai login to mirror. Leave it empty if latchkey holds only " +
          "one — naming the wrong one mirrors someone else's conversations.",
      },
      {
        kind: "date",
        target: "api.since",
        label: "Mirror conversations updated since",
        help: "YYYY-MM-DD. Leave empty to sync everything.",
      },
      {
        kind: "bool",
        target: "api.projects",
        label: "Also mirror Claude Projects",
        default: true,
        help:
          "Each project's description, custom instructions and knowledge documents. " +
          "A project is often the only place some written context lives.",
      },
      {
        kind: "int",
        target: "api.refresh_most_recent_n_chat_count",
        label: "Force-refresh the N most recent chats each run",
        help: "Leave empty to rely on updated_at alone.",
      },
      {
        kind: "string_list",
        probe: "conversations",
        target: "api.conv_uuids",
        label: "Only these conversations",
        placeholder: "https://claude.ai/chat/…",
        help:
          "Bare UUIDs or paste-able chat URLs. Leave empty to walk everything — this is a " +
          "scoping tool for a first run against a large account.",
      },
    ],
  },

  // Listed for completeness; no form yet.
  { type: "chatgpt", method: "api", label: "ChatGPT", blurb: "Mirror your ChatGPT conversations.", keywords: ["chatgpt", "openai", "gpt"], kind: "api", icon: "chatgpt", defaultName: "chatgpt", wizard: false, credentialService: "chatgpt" },
  { type: "github", method: "api", label: "GitHub", blurb: "Mirror pull requests and their review threads.", keywords: ["github", "pr", "code", "review"], kind: "api", icon: "github", defaultName: "github", wizard: false, credentialService: "github" },
  { type: "gitlab", method: "api", label: "GitLab", blurb: "Mirror merge requests and their discussions.", keywords: ["gitlab", "mr", "code"], kind: "api", icon: "gitlab", defaultName: "gitlab", wizard: false, credentialService: "gitlab" },
  { type: "notion", method: "api", label: "Notion", blurb: "Mirror pages and comment threads.", keywords: ["notion", "wiki", "docs", "pages"], kind: "api", icon: "notion", defaultName: "notion", wizard: false, credentialService: "notion" },

  // ── the two `email` variants ──────────────────────────────────────
  //
  // Gmail must come before the JMAP entry: `variantKey` matching takes
  // the first hit, and a Gmail step has no `jmap` table to confuse it
  // — but a future entry keyed on something broader would.
  {
    type: "email",
    variantKey: "gmail",
    label: "Gmail",
    blurb: "Mirror a Gmail account through Google's API.",
    keywords: ["gmail", "google", "email", "mail", "inbox", "labels"],
    kind: "api",
    icon: "email",
    defaultName: "gmail",
    wizard: true,
    canProbe: true,
    credentialService: "google-gmail",
    preset: [
      // The presence of a `gmail` table is what selects this mode,
      // and a table needs a key. `user_id` is the one to spend: `me`
      // is both Gmail's meaning of "the authenticated user" and the
      // backend's own default, so writing it changes nothing except
      // making the mode explicit in the file.
      { target: "gmail.user_id", value: "me" },
      { target: "outlink_format", value: "gmail", phase: "render" },
    ],
    fields: [
      {
        kind: "text",
        latchkey: true,
        target: "latchkey_settings.account",
        label: "Google account",
        placeholder: "you@example.com",
        help:
          "Which stored Google login to mirror. Leave it empty if latchkey holds only one — " +
          "it is required only when the google-gmail service has more than one account, " +
          "and naming the wrong one mirrors the wrong mailbox.",
      },
      {
        kind: "string_list",
        probe: "labels",
        target: "only_extract_labels",
        label: "Download only these labels",
        placeholder: "Inbox, Work/Projects",
        help:
          "Exact label paths — a nested label must be listed in full, and listing a parent " +
          "does not include its children. Empty downloads the whole account, which is the " +
          "point of a mirror; narrow it for a first run against a large mailbox. Widening " +
          "it later backfills the labels you added.",
      },
      {
        kind: "int",
        target: "gmail.message_budget",
        label: "Stop after this many messages each run",
        help:
          "Gmail's quota allows about 300 messages a minute, so a 100k-message account is " +
          "roughly six hours of downloading. A budget makes that a series of runs that each " +
          "finish successfully and resume where they stopped, instead of one long run that " +
          "fails and poisons everything downstream. Leave empty for no limit.",
      },
      {
        kind: "int",
        target: "common.blob_size_limit_bytes",
        label: "Skip attachments larger than (bytes)",
        help:
          "Attachments are most of a mailbox's bytes and almost none of its text. Leave " +
          "empty for no limit.",
      },
      {
        kind: "string_list",
        probe: "mailboxes",
        phase: "render",
        target: "only_render_labels",
        label: "Render only these labels",
        placeholder: "Inbox, Work/Projects",
        help:
          "A second, narrower filter applied when markdown is written — so a whole account " +
          "can be downloaded once and only part of it turned into searchable pages. Empty " +
          "renders everything downloaded. Changing it re-renders; it never re-downloads.",
      },
    ],
  },
  {
    type: "email",
    variantKey: "jmap",
    label: "Fastmail",
    blurb: "Mirror a Fastmail mailbox over JMAP.",
    keywords: ["fastmail", "jmap", "email", "mail", "inbox", "folders"],
    kind: "api",
    icon: "email",
    defaultName: "fastmail",
    wizard: true,
    canProbe: true,
    credentialService: "fastmail",
    preset: [
      // The JMAP server. A preset rather than a field because this
      // entry *is* Fastmail — a different host is a different service
      // and wants its own entry (the downloader hardcodes nothing:
      // everything after discovery comes off the session document).
      { target: "jmap.hostname", value: "api.fastmail.com" },
      { target: "outlink_format", value: "fastmail", phase: "render" },
    ],
    fields: [
      {
        kind: "text",
        latchkey: true,
        target: "latchkey_settings.account",
        label: "Fastmail account",
        placeholder: "you@fastmail.com",
        help:
          "Which stored Fastmail login to mirror. Leave it empty if latchkey holds only one.",
      },
      {
        kind: "string_list",
        probe: "labels",
        target: "only_extract_labels",
        label: "Download only these folders",
        placeholder: "Inbox, travel/portugal",
        help:
          "Exact folder paths, parent first — `travel/portugal` is the folder inside " +
          "`travel`, and listing `travel` alone does not include it. Empty downloads the " +
          "whole mailbox.",
      },
      {
        kind: "int",
        target: "common.blob_size_limit_bytes",
        label: "Skip attachments larger than (bytes)",
        help:
          "Attachments are most of a mailbox's bytes and almost none of its text. Leave " +
          "empty for no limit.",
      },
      {
        kind: "int",
        target: "jmap.blob_download_concurrency",
        label: "Message downloads in flight",
        help:
          "JMAP has no bulk download — each message body is its own request — so this is " +
          "the only lever on how fast a first backfill goes. Leave empty for the default; " +
          "1 makes it strictly one at a time.",
      },
      {
        kind: "string_list",
        probe: "mailboxes",
        phase: "render",
        target: "only_render_labels",
        label: "Render only these folders",
        placeholder: "Inbox, travel/portugal",
        help:
          "A second, narrower filter applied when markdown is written — so a whole mailbox " +
          "can be downloaded once and only part of it turned into searchable pages. Empty " +
          "renders everything downloaded. Changing it re-renders; it never re-downloads.",
      },
    ],
  },
  // The catch-all `email` entry, and deliberately last: it has no
  // `variantKey`, so it is what an email step matches when neither of
  // the two above does — an mbox source, or a JMAP server that is not
  // Fastmail. No form, because the thing it stands for is "some other
  // way of getting mail", which is not one form.
  { type: "email", label: "Email (mbox or other server)", blurb: "A Google Takeout .mbox, or a JMAP server other than Fastmail.", keywords: ["email", "mail", "jmap", "imap", "mbox", "takeout"], kind: "api", icon: "email", defaultName: "email", wizard: false },
  { type: "contacts", label: "Contacts", blurb: "Mirror contacts from a CardDAV server or .vcf files.", keywords: ["contacts", "carddav", "vcard", "address book"], kind: "api", icon: null, defaultName: "contacts", wizard: false },
  { type: "yolink", method: "api", label: "YoLink", blurb: "Per-device temperature, humidity and water history.", keywords: ["yolink", "sensor", "temperature", "iot", "yosmart"], kind: "api", icon: "yolink", defaultName: "yolink", wizard: false },

  {
    type: "claude",
    variantKey: "export",
    label: "Claude export",
    blurb: "Ingest an unpacked Claude data export already on disk.",
    keywords: ["claude", "anthropic", "export", "backup"],
    kind: "export",
    icon: "claude",
    defaultName: "claude-export",
    wizard: true,
    fields: [
      {
        kind: "path",
        picks: "dir",
        pickTitle: "Choose the unpacked Claude export",
        required: true,
        target: "export.path",
        label: "Export folder",
        placeholder: "~/Downloads/claude-export",
        help:
          "The directory you unpacked the export into — the one holding conversations.json. " +
          "The export is a complete snapshot: a conversation it no longer mentions is " +
          "dropped from the mirror.",
      },
    ],
  },
  { type: "google_takeout", label: "Google Takeout", blurb: "Google Chat, Voice, Maps and YouTube from an export.", keywords: ["google", "takeout", "chat", "voice", "youtube"], kind: "export", icon: null, defaultName: "google-takeout", wizard: false },
  { type: "linkedin", label: "LinkedIn", blurb: "Messages and connections from a data export.", keywords: ["linkedin", "export", "connections"], kind: "export", icon: "linkedin", defaultName: "linkedin", wizard: false },
  {
    type: "signal",
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
        pickTitle: "Choose your Signal backups folder",
        required: true,
        target: "backup.path",
        label: "Backup folder",
        placeholder: "~/backups/SignalBackups",
        help:
          "The folder holding your signal-backup-* snapshots, pulled off the phone. " +
          "The newest snapshot in it is the one decrypted.",
      },
      {
        kind: "text",
        target: "backup.aep_env_var",
        label: "Passphrase environment variable",
        placeholder: "SIGNAL_BACKUP_PASSPHRASE",
        help:
          "Name of the env var holding the backup passphrase — not the passphrase itself. " +
          "The backend reads it at download time. Leave empty for the default.",
      },
      {
        kind: "select",
        target: "period",
        phase: "render",
        label: "Document span",
        default: "month",
        options: [
          { value: "day", label: "A day" },
          { value: "month", label: "A month" },
          { value: "year", label: "A year" },
          { value: "all", label: "The whole conversation" },
        ],
        help: "How much of a conversation goes in one rendered page.",
      },
    ],
  },
  {
    type: "whatsapp",
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
        pickTitle: "Choose your WhatsApp backup folder",
        required: true,
        target: "backup.path",
        label: "WhatsApp folder",
        placeholder: "~/backups/WhatsApp",
        help:
          "The WhatsApp/ directory pulled off the phone — the one containing " +
          "Databases/msgstore.db.crypt15 and a Media/ tree.",
      },
      {
        kind: "text",
        target: "backup.key_env_var",
        label: "Decryption-key environment variable",
        placeholder: "WHATSAPP_BACKUP_DECRYPTION_KEY",
        help:
          "Name of the env var holding the hex-encoded 32-byte root key — not the key " +
          "itself. Leave empty for the default.",
      },
    ],
  },
  { type: "sms_backup_restore", label: "SMS & calls", blurb: "Android SMS Backup & Restore XML exports.", keywords: ["sms", "mms", "calls", "android", "texts"], kind: "export", icon: "sms", defaultName: "sms", wizard: false },
  { type: "beeper", label: "Beeper", blurb: "Read Beeper Texts' local store across its networks. Poorly supported — expect rough edges.", keywords: ["beeper", "matrix", "chat", "imessage"], kind: "export", icon: null, defaultName: "beeper", wizard: false },

  {
    type: "pdf",
    label: "PDFs",
    blurb: "Convert a directory tree of PDFs into searchable markdown.",
    keywords: ["pdf", "documents", "papers", "files"],
    kind: "local",
    icon: null,
    defaultName: "pdfs",
    wizard: true,
    fields: [
      {
        kind: "path",
        picks: "dir",
        pickTitle: "Choose the folder of PDFs to index",
        required: true,
        target: "fswalk.path",
        label: "PDF folder",
        placeholder: "~/Documents",
        help:
          "Scanned recursively for PDFs. Documents are identified by their bytes, so the " +
          "same file in two places is one document; a PDF with no extractable text is " +
          "recorded as scanned and left unconverted rather than indexed as empty.",
      },
      {
        kind: "string_list",
        target: "ignore",
        label: "Ignore patterns",
        placeholder: "drafts/**, **/scans/**",
        help:
          "Gitignore-shaped patterns pruned from the scan, on top of any .gitignore files " +
          "found in the tree. Leave empty to walk everything.",
      },
      {
        kind: "int",
        target: "max_bytes",
        label: "Skip files larger than (bytes)",
        help:
          "A multi-gigabyte PDF is nearly always a scanned book, and either way one " +
          "document shouldn't stall a whole scan. Leave empty for the 512 MiB default.",
      },
    ],
  },
  { type: "fsindex", label: "File index", blurb: "Index a directory tree — paths, sizes, content hashes.", keywords: ["files", "filesystem", "index", "directory", "disk"], kind: "local", icon: null, defaultName: "fsindex", wizard: false },
  {
    type: "media",
    label: "Music, photos & video",
    blurb: "Index a media tree — tags, EXIF, playlists, and a metadata-free content hash.",
    keywords: ["music", "photos", "video", "mp3", "jpeg", "raw", "dng", "playlists", "media"],
    kind: "local",
    icon: null,
    defaultName: "media",
    wizard: true,
    // Download-only: media has no text to convert, so nothing is
    // rendered and no render step is declared.
    renderStep: false,
    fields: [
      {
        kind: "path",
        picks: "dir",
        pickTitle: "Choose your media folder",
        required: true,
        target: "fswalk.path",
        label: "Media folder",
        placeholder: "~/Music",
        help:
          "Scanned for audio, images, video and .m3u playlists. Files are identified by " +
          "their bytes rather than their extension, and each one also gets a hash over " +
          "just its audio or picture data — so retagging a track, or re-rendering a RAW " +
          "preview, doesn't make it look like a new file.",
      },
      {
        kind: "bool",
        target: "playlists",
        label: "Index .m3u playlists",
        default: true,
        help:
          "Records each playlist's entries in order, including the ones pointing at " +
          "tracks you no longer have. Streaming manifests that share the .m3u8 " +
          "extension are recognized and skipped.",
      },
      {
        kind: "bool",
        target: "skip_dataless",
        label: "Skip cloud placeholders",
        default: true,
        help:
          "Leave Dropbox online-only and iCloud evicted files alone rather than pulling " +
          "them down. Turn this off only if your filesystem reports no block counts, " +
          "which makes every file look evicted.",
      },
    ],
  },
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
        pickTitle: "Choose your Lightroom catalog",
        extensions: ["lrcat"],
        required: true,
        target: "catalog.path",
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
  { type: "perseus", method: "github", label: "Perseus library", blurb: "Classical texts from the Perseus Digital Library.", keywords: ["perseus", "greek", "latin", "classics", "sample"], kind: "local", icon: null, defaultName: "perseus", wizard: false },
];

export const KIND_LABELS: Record<CatalogEntry["kind"], string> = {
  api: "Connected accounts",
  export: "Exports & backups",
  local: "On this computer",
};

/// A stable, unique key for an entry — what a `v-for` keys on and what
/// the picker's cursor compares.
export function entryKey(entry: CatalogEntry): string {
  return entry.variantKey ? `${entry.type}:${entry.variantKey}` : entry.type;
}

/// The first entry for a step type, ignoring variants.
export function catalogFor(type: string): CatalogEntry | undefined {
  return CATALOG.find((e) => e.type === type);
}

/// The entry describing a step that already exists: its type, narrowed
/// by which variant its params say it is.
export function catalogForStep(
  type: string | null,
  params: Record<string, unknown>,
): CatalogEntry | undefined {
  if (!type) return undefined;
  const candidates = CATALOG.filter((e) => e.type === type);
  return (
    candidates.find((e) => e.variantKey !== undefined && hasPath(params, e.variantKey)) ??
    candidates.find((e) => e.variantKey === undefined)
  );
}

/// Does a dotted path exist in a params tree? Presence, not truthiness:
/// `gmail = {}` selects the Gmail mode, and an empty table is a
/// perfectly ordinary way to write it by hand.
function hasPath(params: Record<string, unknown>, path: string): boolean {
  let cur: unknown = params;
  for (const seg of path.split(".")) {
    if (cur === null || typeof cur !== "object" || Array.isArray(cur)) return false;
    if (!(seg in (cur as Record<string, unknown>))) return false;
    cur = (cur as Record<string, unknown>)[seg];
  }
  return true;
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
