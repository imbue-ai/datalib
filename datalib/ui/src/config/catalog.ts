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
// Seven descriptors carry a form: `slack_api`, `claude_api` and the two
// `email` variants (Gmail, Fastmail) for the credentialed path, and
// `lightroom` / `signal_backup` / `whatsapp_backup` / `pdf` / `media`
// for the on-disk one. Every other type is listed so the picker shows
// the real breadth of what datalib supports, but is marked
// `wizard: false` — picking one sends you to the config editor rather
// than pretending a form exists.
//
// ### One step type, more than one entry
//
// Gmail and Fastmail are both `datalib-step download email`. They are
// *not* one form: they authenticate against different latchkey
// services, select their download mode with different params, and want
// different words on screen. So `email` has three entries here, and the
// thing that separates them is `variantKey` — the params path whose
// presence says which one an existing step is. `type` stops being a
// unique key at that point; `entryKey` is the unique one.

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
  /// Only shown, and only written, while the `bool` field at this
  /// target is on.
  ///
  /// This is not cosmetic. A provider may reject a combination its
  /// struct can express — `slack_api` errors on `dm_users` set with
  /// `dms = false`, because both silent readings of that are wrong —
  /// and a form that can produce a config the backend refuses is a
  /// form that fails at sync time instead of at fill-in time.
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
  ///
  /// `picks` is what decides which dialog opens, so a file/dir mismatch
  /// here is the one error a picker can still let through. Both it and
  /// `pickTitle` are required in practice; they are optional in the
  /// type only because this table predates the picker.
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
  ///
  /// The asymmetry is the point. A `bool`/`select` default matches the
  /// backend's own default, so seeding it while editing changes
  /// nothing. An `int` default here is a *policy* the wizard imposes
  /// where the backend has none — `blob_size_limit_bytes` means "no
  /// limit" when absent — so seeding it on edit would silently cap a
  /// source that was deliberately uncapped, the next time someone
  /// opened the form to change something unrelated. Absent stays
  /// absent; only a value already in the config is shown back.
  | ({ kind: "int" } & FieldBase & { default?: number })
  | ({ kind: "string_list" } & FieldBase & {
      placeholder?: string;
      /// Offer a checklist built from `POST /api/probe`, alongside the
      /// comma-separated box. Names *which* of the probe's lists:
      ///
      ///   `labels`     everything the account has. What a **download**
      ///                filter may name — for Gmail that includes
      ///                `Starred` and `Unread`, which the service
      ///                resolves server-side.
      ///   `mailboxes`  only the entries emails are actually filed in.
      ///                What a **render** filter may name: it matches
      ///                stored mailbox paths, so offering `Starred`
      ///                there would offer a filter that silently
      ///                matches nothing.
      ///
      /// The typed box stays either way — a probe needs credentials
      /// that may not exist yet, and a form should not be unusable
      /// until a network call succeeds.
      probe?: "labels" | "mailboxes";
    });

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
  ///
  /// The wizard uses it to list stored accounts and to run
  /// `latchkey auth browser <service>`. At *request* time latchkey
  /// picks the service by matching the URL rather than by this string,
  /// so a wrong value here misleads the setup screen without breaking
  /// a sync — which is exactly the kind of wrong that survives.
  credentialService?: string;
  /// Dotted params path whose presence identifies this entry among the
  /// several that share one `type`. Undefined on a type with only one
  /// entry, which is nearly all of them.
  ///
  /// Order matters: [`catalogForStep`] takes the first entry whose key
  /// is present, so a more specific key must come first in `CATALOG`.
  variantKey?: string;
  /// Params this entry always writes, with no field to edit them.
  ///
  /// Two jobs, both about identity rather than preference:
  ///
  ///   * **selecting a mode.** An `email` step is a Gmail step because
  ///     it has a `gmail_api` table, and a JMAP step because it has a
  ///     `sync.hostname`. Neither is something to ask about — the
  ///     person picked "Gmail" off the tile grid already.
  ///   * **following from the choice.** A Gmail source's webmail
  ///     outlinks are Gmail's. There is no second answer.
  ///
  /// A preset is written on every save and is counted as *known* by
  /// `paramsAreRepresentable`, so a step carrying one stays editable.
  /// If a value is a real choice, make it a field — a preset the user
  /// can't see is a value they can't change without the config editor.
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
        kind: "int",
        target: "common.blob_size_limit_bytes",
        requires: "sync.media",
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
        target: "sync.all_channels",
        label: "Include channels you're not a member of",
        default: false,
        help: "Ignored when Channels is set.",
      },
      {
        kind: "bool",
        target: "sync.dms",
        label: "Download direct messages",
        default: false,
        help:
          "Your 1:1 and group DMs, alongside the channels above. Off by default — DMs are " +
          "the most private thing in a workspace, so mirroring them is opt-in.",
      },
      {
        kind: "string_list",
        target: "sync.dm_users",
        requires: "sync.dms",
        label: "Only DMs with these people",
        placeholder: "@riker, Jean-Luc Picard, U024BE7LH",
        help:
          "Names a person, not a conversation — a Slack handle, display name, real name or " +
          "user id, with or without the @. A group DM counts as a conversation with everyone " +
          "in it. Leave empty for every DM.",
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

  // ── the two `email` variants ──────────────────────────────────────
  //
  // Same step type, same raw schema, same render path: a mailbox
  // mirrored from Gmail and one mirrored over JMAP dedupe against each
  // other rather than doubling (docs/dev/email_download_modes.md).
  // What differs is how you reach the account, and that is all these
  // two entries encode.
  //
  // Gmail must come before the JMAP entry: `variantKey` matching takes
  // the first hit, and a Gmail step has no `sync` table to confuse it
  // — but a future entry keyed on something broader would.
  {
    type: "email",
    variantKey: "gmail_api",
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
      // The presence of a `gmail_api` table is what selects this mode,
      // and a table needs a key. `user_id` is the one to spend: `me`
      // is both Gmail's meaning of "the authenticated user" and the
      // backend's own default, so writing it changes nothing except
      // making the mode explicit in the file.
      { target: "gmail_api.user_id", value: "me" },
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
        target: "gmail_api.message_budget",
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
    variantKey: "sync",
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
      { target: "sync.hostname", value: "api.fastmail.com" },
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
        target: "sync.blob_download_concurrency",
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
  { type: "carddav", label: "Contacts", blurb: "Mirror contacts from a CardDAV server or .vcf files.", keywords: ["contacts", "carddav", "vcard", "address book"], kind: "api", icon: null, defaultName: "contacts", wizard: false },
  { type: "yolink", label: "YoLink", blurb: "Per-device temperature, humidity and water history.", keywords: ["yolink", "sensor", "temperature", "iot", "yosmart"], kind: "api", icon: "yolink", defaultName: "yolink", wizard: false },

  { type: "claude_export", label: "Claude export", blurb: "Ingest an unpacked Claude data export already on disk.", keywords: ["claude", "anthropic", "export", "backup"], kind: "export", icon: "claude", defaultName: "claude-export", wizard: false },
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
        pickTitle: "Choose your Signal backups folder",
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
        pickTitle: "Choose your WhatsApp backup folder",
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
        target: "common.input_path",
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
        target: "common.input_path",
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

/// A stable, unique key for an entry — what a `v-for` keys on and what
/// the picker's cursor compares.
///
/// `type` alone stopped being unique when `email` grew a Gmail entry and
/// a Fastmail entry beside its catch-all. Rather than inventing a
/// second id to keep in step with the type, the key *is* the pair that
/// already distinguishes them.
export function entryKey(entry: CatalogEntry): string {
  return entry.variantKey ? `${entry.type}:${entry.variantKey}` : entry.type;
}

/// The first entry for a step type, ignoring variants.
///
/// Right for a caller that only has a type string and wants a label —
/// but wrong for anything that will *write* a step, since the variants
/// of one type write different params. Those callers want
/// [`catalogForStep`].
export function catalogFor(type: string): CatalogEntry | undefined {
  return CATALOG.find((e) => e.type === type);
}

/// The entry describing a step that already exists: its type, narrowed
/// by which variant its params say it is.
///
/// Among the entries for one type, the first whose `variantKey` is
/// present in the params wins; an entry with no `variantKey` matches
/// anything and so acts as the fallback. That ordering is why the
/// catch-all `email` entry sits last in `CATALOG`.
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
/// `gmail_api = {}` selects the Gmail mode, and an empty table is a
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
