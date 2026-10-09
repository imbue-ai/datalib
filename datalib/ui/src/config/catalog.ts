// The source catalog the "+ Data Source" picker renders, and the form
// descriptors the wizard fills in. Before adding a source here, read
// docs/dev/wizard_design.md (how a form is laid out and worded),
// docs/dev/config_model.md (what the form writes) and
// docs/dev/wizard_file_pickers.md (a path field offers a native picker).

import { NAME_IT_HELP } from "./accountNaming";

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
        required?: boolean;
        /// The field names an environment variable holding a secret.
        /// Drawn as the default variable's name with a copy button, and
        /// the box only for someone who uses another name.
        envVar?: { default: string; holds: string };
        /// Renders as the latchkey-account control rather than a bare
        /// text box: a box that lists the accounts latchkey has stored
        /// for the entry's `credentialService` and still takes typing,
        /// with the sign-in tabs under it.
        ///
        /// Typing matters. latchkey may hold an account this server
        /// can't enumerate (no keyring access, latchkey not installed),
        /// and a list that came back empty must not be the only way in.
        /// The value written is the account string either way.
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
        required?: boolean;
        picks?: "file" | "dir";
        /// Dialog title. Name the thing being chosen ("Choose your
        /// WhatsApp backup folder"), not the widget ("Select folder").
        pickTitle?: string;
        /// `picks: "file"` only — extensions to filter on, no dot. The
        /// typed input stays the escape hatch for anything the filter
        /// wrongly excludes.
        extensions?: string[];
        /// The folder the thing is almost always in, where the app that
        /// owns it keeps it (`~/Library/Messages`). The picker opens
        /// there while the field is empty, and the help text, which must
        /// name it, shows it with a copy button beside it.
        startIn?: string;
        /// macOS keeps this path private, and choosing it in the picker
        /// is what grants access. Drawn as a card that says so, under
        /// this title.
        guarded?: string;
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
  /// A byte count. Stored and written as plain bytes, like an `int`,
  /// but drawn as a number beside a B/KB/MB/GB unit so nobody has to
  /// count zeros. The label should therefore not say "(bytes)".
  | ({ kind: "bytes" } & FieldBase & { default?: number })
  | ({ kind: "string_list" } & FieldBase & {
        /// Offer a picker, alongside the comma-separated box, that a
        /// "Load" button fills from `POST /api/probe` with this list:
        /// every label, only the ones a render filter can match, an
        /// account's conversations (a Claude chat, a Slack DM), or a
        /// workspace's channels.
        probe?: ProbeNoun;
      });

/// What a `probe:` field is a picker *of*: the list its "Load" asks the
/// provider for. Mirrors `ProbeList` in datalib/backend/probe/src/lib.rs.
/// The wizard says `labels` and `mailboxes` in the source's own word for
/// them (`CatalogEntry.mailboxNoun`), the rest as written.
export type ProbeNoun =
  "labels" | "mailboxes" | "conversations" | "channels" | "calendars" | "addressbooks";

/// One answer to a section's question. Choosing it writes `sets` and
/// shows `fields` under it; the fields of the answers not chosen are
/// emptied, so an answer is the whole of what it says.
export type Answer = {
  label: string;
  help?: string;
  sets?: Record<string, boolean>;
  fields?: string[];
};

/// A heading and the controls under it: either a question with
/// `answers`, or `fields` drawn as they are. `advanced` puts it inside
/// Advanced options. docs/dev/wizard_design.md says what goes where.
export type Section = {
  heading: string;
  help?: string;
  answers?: Answer[];
  fields?: string[];
  advanced?: boolean;
};

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
  /// The greyed-out example in the wizard's Name box. Write what a
  /// person would actually call this source — "WhatsApp old phone", not
  /// "whatsapp" — since the name is display text and telling someone
  /// their choices are wider than the id is the whole job of the hint.
  /// Where somebody plausibly has two, say which one this is: that is
  /// the case the name exists for, and the id cannot carry it.
  /// Nothing is pre-filled from it: a blank name still falls back to
  /// the id.
  nameHint: string;
  /// What this source calls a mailbox on its own screens — Gmail says
  /// "labels", every other mail client "folders" — used in the
  /// sentences around a `labels`/`mailboxes` picker. Defaults to
  /// "folders".
  mailboxNoun?: "labels" | "folders";
  /// False → in the picker for completeness, but no form exists yet.
  wizard: boolean;
  /// False for a provider that declares no render step at all.
  /// Defaults to true, and rendering no *documents* is not a reason to
  /// set it false: the render step is also what emits the storage
  /// report, which for a download-only source is the only thing that
  /// puts it in the grid.
  renderStep?: boolean;
  /// The latchkey service name, when the source needs credentials. The
  /// wizard shows its account row only while the params the form
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
  /// The "Paste a key" tab. `help` says where the credential
  /// comes from; `headers` replaces the shape latchkey's own example
  /// gives, for a service whose example is wrong — `{secret}` marks
  /// where the pasted value goes (see `credentialShape.ts`).
  /// `accountSuffix` follows the username in the name the credential is
  /// offered under, for entries that share one service but want
  /// different credentials: Fastmail Contacts and Calendar both use
  /// `fastmail-dav`, and a read-only app password covers only one.
  credentialPaste?: { help?: string; headers?: string[]; accountSuffix?: string };
  /// Dotted params path whose presence identifies this entry among the
  /// several that share one `type`. Undefined on a type with only one
  /// entry, which is nearly all of them.
  ///
  /// Order matters: [`catalogForStep`] takes the first entry whose key
  /// is present, so a more specific key must come first in `CATALOG`.
  variantKey?: string;
  /// Field targets of which at least one must be filled in, for a type
  /// whose methods combine (lightroom's catalog and backups folder), so
  /// no one of them is `required` alone.
  requiresOneOf?: string[];
  /// Params this entry always writes, with no field to edit them.
  preset?: Preset[];
  /// Offer "Check connection", and a "Load" on every `probe:` field.
  /// Requires a `datalib-step probe <type>` on the backend side; see
  /// `datalib/backend/datalib_step/src/probe.rs`.
  canProbe?: boolean;
  /// One sentence above the form on what this source copies, where
  /// the tile's blurb does not say enough.
  intro?: string;
  /// "Before you start": what the person has to have in hand, for a
  /// source that cannot be set up from this dialog alone. A `needs`
  /// item may hold `code` in backticks.
  before?: { text: string; needs: string[] };
  /// The form, top to bottom. A field no section names is an advanced
  /// one, drawn under its own label in Advanced options.
  sections?: Section[];
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

/// Fields are kept inline per entry rather than factored out, even
/// where two entries agree — the design's whole point is that a
/// descriptor is data owned by one provider, not a class hierarchy.
export const CATALOG: CatalogEntry[] = [
  {
    type: "slack",
    method: "api",
    label: "Slack",
    blurb: "Copy channels and DMs from one Slack workspace.",
    keywords: ["slack", "chat", "workspace", "channels", "messages"],
    kind: "api",
    icon: "slack",
    defaultName: "slack",
    nameHint: "Work Slack",
    wizard: true,
    credentialService: "slack",
    canProbe: true,
    sections: [
      {
        heading: "Which channels?",
        answers: [
          { label: "Every channel I'm in", sets: { "api.all_channels": false } },
          {
            label: "Every channel in the workspace",
            help: "Including ones you haven't joined.",
            sets: { "api.all_channels": true },
          },
          // No `sets`: a channel list wins over `all_channels` in the
          // downloader, so a config holding both is this answer.
          { label: "Only the channels I choose", fields: ["api.channels"] },
        ],
      },
      {
        heading: "Direct messages?",
        answers: [
          {
            label: "Leave them out",
            help: "DMs are the most private part of a workspace.",
            sets: { "api.dms": false },
          },
          {
            label: "All my direct messages",
            help: "One-to-one and group conversations.",
            sets: { "api.dms": true },
          },
          {
            label: "Only the conversations I choose",
            sets: { "api.dms": true },
            fields: ["api.dm_conversations"],
          },
        ],
      },
      {
        heading: "How far back?",
        answers: [
          { label: "Everything" },
          { label: "Only messages since a date I choose", fields: ["api.since"] },
        ],
      },
      { heading: "Files people shared", fields: ["api.media", "common.blob_size_limit_bytes"] },
    ],
    fields: [
      {
        kind: "string_list",
        probe: "channels",
        target: "api.channels",
        label: "Channels",
        help: "A channel named here is copied whether or not you're a member of it.",
      },
      {
        kind: "date",
        target: "api.since",
        label: "Start date",
        help: "Choosing an earlier date later brings in the older messages on the next sync.",
      },
      {
        kind: "date",
        target: "api.until",
        label: "Copy until",
        help: "Last day to copy. Empty keeps up with today; a past day fixes the window.",
      },
      {
        kind: "bool",
        target: "api.media",
        label: "Download them",
        default: true,
        help: "Off keeps each file's name and details, without the file.",
      },
      {
        kind: "bytes",
        target: "common.blob_size_limit_bytes",
        requires: "api.media",
        label: "Skip files bigger than",
        default: 5_000_000,
        help:
          "A few very large uploads usually take most of the disk space and hold little text. " +
          "Clear the number for no limit; raising it later brings in the files it skipped.",
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
          "the most private thing in a workspace, so copying them is opt-in.",
      },
      {
        kind: "string_list",
        probe: "conversations",
        target: "api.dm_conversations",
        requires: "api.dms",
        label: "Conversations",
        help:
          "A link to a conversation works too: right-click it in Slack's sidebar and choose " +
          "Copy link.",
      },
      {
        kind: "int",
        target: "api.refresh_window_days",
        label: "Edit-catcher window (days)",
        help:
          "Re-queries the trailing N days of channels that already have history, for edits, " +
          "reactions and deletions. Not a range bound. Empty is 30; 0 turns it off.",
      },
    ],
  },
  {
    type: "claude",
    variantKey: "api",
    method: "api",
    label: "Claude",
    blurb: "Copy your claude.ai conversations and projects.",
    keywords: ["claude", "anthropic", "chat", "llm", "conversations"],
    kind: "api",
    icon: "claude",
    defaultName: "claude",
    nameHint: "Claude Account 1",
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
    // Kept to one line on purpose: the point is that clicking has a
    // cost, not the history of how we found out (2026-08-31, the
    // captured cookie and the everyday browser evicting each other).
    credentialConnectWarning: "Signing in again may log out your other claude.ai session.",
    // latchkey offers a service it did not ship the generic Bearer
    // example; claude.ai's credential is the cookie.
    credentialPaste: {
      headers: ["Cookie: sessionKey={secret}"],
      help:
        "The sessionKey cookie from a signed-in claude.ai tab: DevTools → Application → " +
        "Cookies → https://claude.ai → sessionKey → Value.",
    },
    canProbe: true,
    sections: [
      {
        heading: "Which conversations?",
        answers: [
          { label: "All of them" },
          {
            label: "Only the conversations I choose",
            help: "Useful for a first sync of a large account.",
            fields: ["api.conv_uuids"],
          },
        ],
      },
      {
        heading: "How far back?",
        answers: [
          { label: "Everything" },
          { label: "Only conversations updated since a date I choose", fields: ["api.since"] },
        ],
      },
      { heading: "Projects", fields: ["api.projects"] },
    ],
    fields: [
      {
        kind: "text",
        latchkey: true,
        target: "latchkey_settings.account",
        label: "Claude account",
        help: NAME_IT_HELP,
      },
      {
        kind: "date",
        target: "api.since",
        label: "Start date",
      },
      {
        kind: "bool",
        target: "api.projects",
        label: "Also copy Claude Projects",
        default: true,
        help: "Each project's description, custom instructions and knowledge documents.",
      },
      {
        kind: "int",
        target: "api.refresh_most_recent_n_chat_count",
        label: "Force-refresh the N most recent chats each sync",
        help: "Empty relies on updated_at alone.",
      },
      {
        kind: "string_list",
        probe: "conversations",
        target: "api.conv_uuids",
        label: "Conversations",
        help: "Chat links work too: paste one in the box.",
      },
    ],
  },

  {
    type: "chatgpt",
    method: "api",
    label: "ChatGPT",
    blurb: "Copy your ChatGPT conversations.",
    keywords: ["chatgpt", "openai", "gpt", "chat", "llm", "conversations"],
    kind: "api",
    icon: "chatgpt",
    defaultName: "chatgpt",
    nameHint: "ChatGPT Account 1",
    wizard: true,
    credentialService: "chatgpt",
    // The credential is the bearer token chatgpt.com's own session
    // endpoint hands the page, so token-capture is the flow that fits.
    // Same registration `docs/user/getting_your_data.md` gives by hand.
    credentialRegister: {
      base_api_url: "https://chatgpt.com/",
      login_url: "https://chatgpt.com/auth/login",
      login_flow: "token-capture",
      login_flow_params: {
        tokenUrl: "https://chatgpt.com/api/auth/session",
        tokenField: "accessToken",
      },
    },
    canProbe: true,
    sections: [
      {
        heading: "Which conversations?",
        answers: [
          { label: "All of them" },
          {
            label: "Only the conversations I choose",
            help: "Useful for a first sync of a large account.",
            fields: ["api.conv_uuids"],
          },
        ],
      },
      {
        heading: "How far back?",
        answers: [
          { label: "Everything" },
          { label: "Only conversations updated since a date I choose", fields: ["api.since"] },
        ],
      },
    ],
    fields: [
      {
        kind: "text",
        latchkey: true,
        target: "latchkey_settings.account",
        label: "ChatGPT account",
        help: NAME_IT_HELP,
      },
      {
        kind: "date",
        target: "api.since",
        label: "Start date",
      },
      {
        kind: "string_list",
        probe: "conversations",
        target: "api.conv_uuids",
        label: "Conversations",
        help: "Chat links work too: paste one in the box.",
      },
    ],
  },

  // Listed for completeness; no form yet.
  {
    type: "github",
    method: "api",
    label: "GitHub",
    blurb: "Copy pull requests and their review threads.",
    keywords: ["github", "pr", "code", "review"],
    kind: "api",
    icon: "github",
    defaultName: "github",
    nameHint: "Work GitHub",
    wizard: false,
    credentialService: "github",
  },
  {
    type: "gitlab",
    method: "api",
    label: "GitLab",
    blurb: "Copy merge requests and their discussions.",
    keywords: ["gitlab", "mr", "code"],
    kind: "api",
    icon: "gitlab",
    defaultName: "gitlab",
    nameHint: "Work GitLab",
    wizard: false,
    credentialService: "gitlab",
  },
  {
    type: "notion",
    method: "api",
    label: "Notion",
    blurb: "Copy pages and comment threads.",
    keywords: ["notion", "wiki", "docs", "pages"],
    kind: "api",
    icon: "notion",
    defaultName: "notion",
    nameHint: "Team Notion",
    wizard: false,
    credentialService: "notion",
  },

  // ── the two `email` variants ──────────────────────────────────────
  //
  // Gmail must come before the JMAP entry: `variantKey` matching takes
  // the first hit, and a Gmail step has no `jmap` table to confuse it
  // — but a future entry keyed on something broader would.
  {
    type: "email",
    variantKey: "gmail",
    label: "Gmail",
    blurb: "Copy a Gmail account through Google's API.",
    keywords: ["gmail", "google", "email", "mail", "inbox", "labels"],
    kind: "api",
    icon: "gmail",
    defaultName: "gmail",
    nameHint: "Work Gmail",
    mailboxNoun: "labels",
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
    sections: [
      {
        heading: "Which mail?",
        answers: [
          { label: "The whole account" },
          { label: "Only the labels I choose", fields: ["only_extract_labels"] },
        ],
      },
      { heading: "Large messages", fields: ["common.blob_size_limit_bytes"] },
    ],
    fields: [
      {
        kind: "text",
        latchkey: true,
        target: "latchkey_settings.account",
        label: "Google account",
        help:
          "Which stored Google login to copy. Leave it empty if only one is stored — " +
          "it is required only when the google-gmail service has more than one account, " +
          "and naming the wrong one copies the wrong mailbox.",
      },
      {
        kind: "string_list",
        probe: "labels",
        target: "only_extract_labels",
        label: "Labels",
        help:
          "A nested label is named in full (Work/Projects), and choosing a parent does not " +
          "include its children. A label added later brings in its mail on the next sync.",
      },
      {
        kind: "bytes",
        target: "common.blob_size_limit_bytes",
        label: "Skip messages bigger than",
        help:
          "A skipped message keeps its headers and loses its body. Decide before the first " +
          "sync: raising the limit later does not go back for messages already skipped. Empty " +
          "means no limit.",
      },
      {
        kind: "string_list",
        probe: "mailboxes",
        phase: "render",
        target: "only_render_labels",
        label: "Render only these labels",
        help:
          "A narrower filter applied at render. Empty renders everything downloaded. Changing " +
          "it re-renders; it never re-downloads.",
      },
    ],
  },
  {
    type: "email",
    variantKey: "jmap",
    label: "Fastmail",
    blurb: "Copy a Fastmail mailbox over JMAP.",
    keywords: ["fastmail", "jmap", "email", "mail", "inbox", "folders"],
    kind: "api",
    icon: "fastmail",
    defaultName: "fastmail",
    nameHint: "Personal Fastmail",
    wizard: true,
    canProbe: true,
    credentialService: "fastmail",
    // Fastmail has no read-only OAuth scope, so the browser login can
    // read, change and send mail; a hand-made token is the way to less.
    credentialPaste: {
      help:
        "For read-only access, make an API token at app.fastmail.com → Settings → Privacy & " +
        "Security → Integrations → API tokens, with Read-only access ticked, and paste it " +
        "here. Web login signs in with full read and write access instead.",
    },
    preset: [
      // The JMAP server. A preset rather than a field because this
      // entry *is* Fastmail — a different host is a different service
      // and wants its own entry (the downloader hardcodes nothing:
      // everything after discovery comes off the session document).
      { target: "jmap.hostname", value: "api.fastmail.com" },
      { target: "outlink_format", value: "fastmail", phase: "render" },
    ],
    sections: [
      {
        heading: "Which mail?",
        answers: [
          { label: "The whole mailbox" },
          { label: "Only the folders I choose", fields: ["only_extract_labels"] },
        ],
      },
      { heading: "Large attachments", fields: ["common.blob_size_limit_bytes"] },
    ],
    fields: [
      {
        kind: "text",
        latchkey: true,
        target: "latchkey_settings.account",
        label: "Fastmail account",
        help: "Which stored Fastmail login to copy. Leave it empty if only one is stored.",
      },
      {
        kind: "string_list",
        probe: "labels",
        target: "only_extract_labels",
        label: "Folders",
        help:
          "A folder inside another is named in full (travel/portugal), and choosing a parent " +
          "does not include its children.",
      },
      {
        kind: "bytes",
        target: "common.blob_size_limit_bytes",
        label: "Skip attachments bigger than",
        help:
          "Attachments take most of a mailbox's disk space and hold almost none of its text. " +
          "Empty means no limit.",
      },
      {
        kind: "int",
        target: "jmap.blob_download_concurrency",
        label: "Message downloads in flight",
        help:
          "JMAP fetches each message body in its own request. Empty is the default; 1 is " +
          "strictly one at a time.",
      },
      {
        kind: "string_list",
        probe: "mailboxes",
        phase: "render",
        target: "only_render_labels",
        label: "Render only these folders",
        help:
          "A narrower filter applied at render. Empty renders everything downloaded. Changing " +
          "it re-renders; it never re-downloads.",
      },
    ],
  },
  // The catch-all `email` entry, and deliberately last: it has no
  // `variantKey`, so it is what an email step matches when neither of
  // the two above does — an mbox source, or a JMAP server that is not
  // Fastmail. No form, because the thing it stands for is "some other
  // way of getting mail", which is not one form.
  {
    type: "email",
    label: "Email (mbox or other server)",
    blurb: "A Google Takeout .mbox, or a JMAP server other than Fastmail.",
    keywords: ["email", "mail", "jmap", "imap", "mbox", "takeout"],
    kind: "api",
    icon: "email",
    defaultName: "email",
    nameHint: "Old mail archive",
    wizard: false,
  },
  // ── the `calendar` variants ───────────────────────────────────────
  //
  // Like `email`: one type, and an entry per way in, each keyed on its
  // own method table. Every method has a form, so there is no catch-all.
  {
    type: "calendar",
    variantKey: "google",
    method: "google",
    label: "Google Calendar",
    blurb: "Copy the calendars on a Google account through Google's API.",
    keywords: ["google", "calendar", "gcal", "events", "meetings", "schedule"],
    kind: "api",
    icon: "google_calendar",
    defaultName: "google_calendar",
    nameHint: "Work calendar",
    wizard: true,
    credentialService: "google-calendar",
    canProbe: true,
    sections: [
      {
        heading: "Which calendars?",
        answers: [
          { label: "Every calendar on the account", help: "Subscribed ones included." },
          { label: "Only the calendars I choose", fields: ["google.calendars"] },
        ],
      },
      {
        heading: "Which events?",
        answers: [
          { label: "All events" },
          { label: "Only events in a range of dates", fields: ["google.since", "google.until"] },
        ],
      },
    ],
    fields: [
      {
        kind: "text",
        latchkey: true,
        target: "latchkey_settings.account",
        label: "Google account",
        help:
          "Which stored Google login to copy. Leave it empty if only one is stored " +
          "for google-calendar.",
      },
      {
        kind: "string_list",
        probe: "calendars",
        target: "google.calendars",
        label: "Calendars",
        help: "A calendar added later is downloaded whole.",
      },
      {
        kind: "date",
        target: "google.since",
        label: "First day",
        help:
          "Only events with some part in the range are copied, and a series keeps only its " +
          "changed dates inside it.",
      },
      {
        kind: "date",
        target: "google.until",
        label: "Last day",
        help: "The last day, included. Leave empty for no end.",
      },
    ],
  },
  {
    type: "calendar",
    variantKey: "fastmail",
    method: "fastmail",
    label: "Fastmail Calendar",
    blurb: "Copy a Fastmail account's calendars over CalDAV.",
    keywords: ["fastmail", "calendar", "caldav", "events", "meetings", "schedule"],
    kind: "api",
    icon: "fastmail",
    defaultName: "fastmail_calendar",
    nameHint: "Personal calendar",
    wizard: true,
    // CalDAV takes an app password, which is its own latchkey service,
    // not the OAuth login the `fastmail` mail entry uses.
    credentialService: "fastmail-dav",
    credentialPaste: {
      help:
        "Your Fastmail address and an app password from app.fastmail.com → Settings → " +
        "Privacy & Security → Integrations → App passwords: Access “Calendars (CalDAV)”, " +
        "with Read-only access ticked. An API token won’t do: CalDAV refuses them.",
      accountSuffix: "calendar",
    },
    canProbe: true,
    sections: [
      {
        heading: "Which calendars?",
        answers: [
          { label: "Every calendar" },
          { label: "Only the calendars I choose", fields: ["fastmail.calendars"] },
        ],
      },
      {
        heading: "Which events?",
        answers: [
          { label: "All events" },
          {
            label: "Only events in a range of dates",
            fields: ["fastmail.since", "fastmail.until"],
          },
        ],
      },
    ],
    fields: [
      {
        kind: "text",
        latchkey: true,
        target: "latchkey_settings.account",
        label: "Fastmail account",
        help:
          "The name the app password is stored under in latchkey — “you@fastmail.com calendar”, " +
          "say. Leave it empty if only one is stored.",
      },
      {
        kind: "string_list",
        probe: "calendars",
        target: "fastmail.calendars",
        label: "Calendars",
        help: "A calendar added later is downloaded whole.",
      },
      {
        kind: "date",
        target: "fastmail.since",
        label: "First day",
        help:
          "Only events with some part in the range are copied, and a series keeps only its " +
          "changed dates inside it.",
      },
      {
        kind: "date",
        target: "fastmail.until",
        label: "Last day",
        help: "The last day, included. Leave empty for no end.",
      },
    ],
  },
  {
    type: "calendar",
    variantKey: "caldav",
    label: "CalDAV",
    blurb: "Copy the calendars on any CalDAV server: iCloud, Nextcloud, Radicale, ….",
    keywords: ["calendar", "caldav", "icloud", "nextcloud", "radicale", "events", "schedule"],
    kind: "api",
    icon: "calendar",
    defaultName: "caldav_calendar",
    nameHint: "iCloud calendar",
    wizard: true,
    // No `credentialService`: latchkey keys a CalDAV login by the
    // server's host, and a host it does not ship needs registering
    // first, with an app password rather than a browser login — which
    // the Connect flow cannot do. The help text says how.
    canProbe: true,
    before: {
      text: "The sign-in for this server is set up in a terminal, with latchkey:",
      needs: [
        "`latchkey services register` a service for this host.",
        '`latchkey auth set <service> -u "you@example.com:<app password>"`',
      ],
    },
    sections: [
      { heading: "Server address", fields: ["caldav.server_url"] },
      {
        heading: "Which calendars?",
        answers: [
          { label: "Every calendar" },
          { label: "Only the calendars I choose", fields: ["caldav.calendars"] },
        ],
      },
      {
        heading: "Which events?",
        answers: [
          { label: "All events" },
          { label: "Only events in a range of dates", fields: ["caldav.since", "caldav.until"] },
        ],
      },
    ],
    fields: [
      {
        kind: "text",
        required: true,
        target: "caldav.server_url",
        label: "Server address",
        help:
          "Where the server's CalDAV starts, e.g. https://caldav.icloud.com/. The host alone is" +
          " usually enough.",
      },
      {
        kind: "text",
        target: "latchkey_settings.account",
        label: "Latchkey account",
        help: "Which stored login to use, when more than one is stored for this host.",
      },
      {
        kind: "string_list",
        probe: "calendars",
        target: "caldav.calendars",
        label: "Calendars",
        help: "A calendar added later is downloaded whole.",
      },
      {
        kind: "date",
        target: "caldav.since",
        label: "First day",
        help:
          "Only events with some part in the range are copied, and a series keeps only its " +
          "changed dates inside it.",
      },
      {
        kind: "date",
        target: "caldav.until",
        label: "Last day",
        help: "The last day, included. Leave empty for no end.",
      },
    ],
  },
  {
    type: "calendar",
    variantKey: "ics",
    label: "Calendar files (.ics)",
    blurb: "A folder of .ics exports, such as Google Takeout's Calendar folder.",
    keywords: ["calendar", "ics", "ical", "icalendar", "takeout", "export", "events"],
    kind: "export",
    icon: "calendar",
    defaultName: "ics_calendar",
    nameHint: "Old calendar export",
    wizard: true,
    sections: [{ heading: "Calendar folder", fields: ["ics.path"] }],
    fields: [
      {
        kind: "path",
        picks: "dir",
        pickTitle: "Choose the folder of .ics files",
        required: true,
        target: "ics.path",
        label: "Calendar folder",
        help:
          "A folder of .ics files, one per calendar: the Calendar folder from a Google Takeout," +
          " say. Each file is the whole of its calendar: an event a re-read file no longer " +
          "holds is dropped from the copy.",
      },
    ],
  },
  // ── the `contacts` variants ───────────────────────────────────────
  //
  // Like `calendar`: one type, an entry per way in, keyed on its method
  // table, and a form for each, so no catch-all.
  {
    type: "contacts",
    variantKey: "fastmail",
    method: "fastmail",
    label: "Fastmail Contacts",
    blurb: "Copy a Fastmail account's address books over CardDAV.",
    keywords: ["fastmail", "contacts", "carddav", "vcard", "address book"],
    kind: "api",
    icon: "fastmail",
    defaultName: "fastmail_contacts",
    nameHint: "Personal contacts",
    wizard: true,
    // The app password Fastmail Calendar uses too: DAV refuses the OAuth
    // login the `fastmail` mail entry holds.
    credentialService: "fastmail-dav",
    credentialPaste: {
      help:
        "Your Fastmail address and an app password from app.fastmail.com → Settings → " +
        "Privacy & Security → Integrations → App passwords: Access “Contacts (CardDAV)”, " +
        "with Read-only access ticked. An API token won’t do: CardDAV refuses them.",
      accountSuffix: "contacts",
    },
    canProbe: true,
    sections: [
      {
        heading: "Which address books?",
        answers: [
          { label: "Every address book" },
          { label: "Only the address books I choose", fields: ["fastmail.addressbooks"] },
        ],
      },
    ],
    fields: [
      {
        kind: "text",
        latchkey: true,
        target: "latchkey_settings.account",
        label: "Fastmail account",
        help:
          "The name the app password is stored under in latchkey — “you@fastmail.com contacts”, " +
          "say. Leave it empty if only one is stored.",
      },
      {
        kind: "string_list",
        probe: "addressbooks",
        target: "fastmail.addressbooks",
        label: "Address books",
      },
    ],
  },
  {
    type: "contacts",
    variantKey: "carddav",
    label: "CardDAV contacts",
    blurb: "Copy the address books on any CardDAV server: iCloud, Nextcloud, Radicale, ….",
    keywords: ["contacts", "carddav", "icloud", "nextcloud", "vcard", "address book"],
    kind: "api",
    icon: "contacts",
    defaultName: "contacts",
    nameHint: "Phone contacts",
    wizard: true,
    // No `credentialService`, for the reason CalDAV has none: latchkey
    // keys the login by the server's host, and registering one takes an
    // app password, which the Connect flow cannot do.
    canProbe: true,
    before: {
      text: "The sign-in for this server is set up in a terminal, with latchkey:",
      needs: [
        "`latchkey services register` a service for this host.",
        '`latchkey auth set <service> -u "you@example.com:<app password>"`',
      ],
    },
    sections: [
      { heading: "Server address", fields: ["carddav.server_url"] },
      {
        heading: "Which address books?",
        answers: [
          { label: "Every address book" },
          { label: "Only the address books I choose", fields: ["carddav.addressbooks"] },
        ],
      },
    ],
    fields: [
      {
        kind: "text",
        required: true,
        target: "carddav.server_url",
        label: "Server address",
        help:
          "Where the server's CardDAV starts, e.g. https://contacts.icloud.com/. The host alone" +
          " is usually enough.",
      },
      {
        kind: "text",
        target: "latchkey_settings.account",
        label: "Latchkey account",
        help: "Which stored login to use, when more than one is stored for this host.",
      },
      {
        kind: "string_list",
        probe: "addressbooks",
        target: "carddav.addressbooks",
        label: "Address books",
      },
    ],
  },
  {
    type: "contacts",
    variantKey: "vcf",
    label: "Contact files (.vcf)",
    blurb: "A folder of .vcf exports, from Google Contacts, iCloud or a phone.",
    keywords: ["contacts", "vcf", "vcard", "export", "address book"],
    kind: "export",
    icon: "contacts",
    defaultName: "vcf-contacts",
    nameHint: "Old address book",
    wizard: true,
    sections: [{ heading: "Contacts folder", fields: ["vcf.path"] }],
    fields: [
      {
        kind: "path",
        picks: "dir",
        pickTitle: "Choose the folder of .vcf files",
        required: true,
        target: "vcf.path",
        label: "Contacts folder",
        help:
          "A folder of .vcf files, from Google Contacts, iCloud or a phone: ~/Downloads/contacts, say. Folders inside it " +
          "are read too. A file may hold one contact or a whole address book.",
      },
    ],
  },
  {
    type: "garmin",
    method: "api",
    label: "Garmin",
    blurb: "Weight, sleep, heart rate, activities and FIT files from Garmin Connect.",
    keywords: [
      "garmin",
      "connect",
      "watch",
      "forerunner",
      "fenix",
      "running",
      "weight",
      "sleep",
      "fitness",
    ],
    kind: "api",
    icon: "garmin",
    defaultName: "garmin",
    nameHint: "My Garmin",
    wizard: true,
    // From latchkey's Garmin plugin, which datalib ships and installs
    // on the first sign-in (datalib/backend/http/src/plugins.rs).
    credentialService: "garmin",
    credentialPaste: {
      help:
        "A folder holding oauth1_token.json, as garth or python-garminconnect write it. " +
        "latchkey keeps a copy; the folder is not read again.",
    },
    canProbe: true,
    sections: [
      {
        heading: "How far back?",
        answers: [
          { label: "One year before the first sync" },
          { label: "From a date I choose", fields: ["api.since"] },
        ],
      },
      {
        heading: "FIT files",
        fields: ["api.activity_files", "api.wellness_files"],
        advanced: true,
      },
    ],
    fields: [
      {
        kind: "date",
        target: "api.since",
        label: "First day",
        help: "Choose an earlier date later to bring in older data.",
      },
      {
        kind: "bool",
        target: "api.activity_files",
        label: "Keep each activity's original FIT file",
        default: true,
        help: "The complete record of a workout; the JSON summary is a projection of it.",
      },
      {
        kind: "bool",
        target: "api.wellness_files",
        label: "Keep each day's wellness FIT bundle",
        default: false,
        help:
          "All-day heart rate, stress, steps, body battery and sleep at sensor resolution, one " +
          "zip per day. The per-day metrics already carry the same series at chart resolution.",
      },
    ],
  },
  {
    type: "yolink",
    method: "api",
    label: "YoLink",
    blurb: "Per-device temperature, humidity and water history.",
    keywords: ["yolink", "sensor", "temperature", "iot", "yosmart"],
    kind: "api",
    icon: "yolink",
    defaultName: "yolink",
    nameHint: "House sensors",
    wizard: false,
  },

  {
    type: "claude",
    variantKey: "export",
    label: "Claude export",
    blurb: "Ingest an unpacked Claude data export already on disk.",
    keywords: ["claude", "anthropic", "export", "backup"],
    kind: "export",
    icon: "claude",
    defaultName: "claude-export",
    nameHint: "Claude export",
    wizard: true,
    sections: [{ heading: "Export folder", fields: ["export.path"] }],
    fields: [
      {
        kind: "path",
        picks: "dir",
        pickTitle: "Choose the unpacked Claude export",
        required: true,
        target: "export.path",
        label: "Export folder",
        help:
          "The folder you unpacked the Claude export into: the one holding conversations.json, ~/Downloads/claude-export say. " +
          "The export is a complete snapshot: a conversation it no longer mentions is dropped " +
          "from the copy.",
      },
    ],
  },
  {
    type: "claude_code",
    // With nothing filled in the form still writes `sessions = {}`, which
    // is the standard store: every Claude Code on this machine keeps its
    // transcripts under ~/.claude/projects.
    method: "sessions",
    label: "Claude Code",
    blurb:
      "Copy your Claude Code sessions — terminal, desktop and IDE — from their store on this machine.",
    keywords: ["claude", "code", "anthropic", "agent", "sessions", "transcripts", "coding"],
    kind: "local",
    icon: "claude_code",
    defaultName: "claude-code",
    nameHint: "Claude Code on this Mac",
    wizard: true,
    sections: [
      {
        heading: "Sessions",
        help: "A session Claude Code later deletes stays in the copy.",
        answers: [
          { label: "The standard place on this computer", help: "~/.claude/projects" },
          { label: "Another folder", fields: ["sessions.path"] },
        ],
      },
    ],
    fields: [
      {
        kind: "path",
        picks: "dir",
        pickTitle: "Choose the Claude Code sessions folder",
        required: false,
        target: "sessions.path",
        label: "Sessions folder",
        help: "For a copy of that folder from another machine.",
      },
    ],
  },
  {
    type: "codex",
    // With nothing filled in the form still writes `sessions = {}`, which
    // is the standard home: Codex keeps its rollouts under ~/.codex.
    method: "sessions",
    label: "Codex",
    blurb: "Copy your Codex CLI sessions from their store on this machine.",
    keywords: ["codex", "openai", "agent", "sessions", "rollouts", "transcripts", "coding"],
    kind: "local",
    icon: "codex",
    defaultName: "codex",
    nameHint: "Codex on this Mac",
    wizard: true,
    sections: [
      {
        heading: "Sessions",
        help: "A thread Codex later deletes stays in the copy.",
        answers: [
          { label: "The standard place on this computer", help: "~/.codex" },
          { label: "Another folder", fields: ["sessions.path"] },
        ],
      },
    ],
    fields: [
      {
        kind: "path",
        picks: "dir",
        pickTitle: "Choose the Codex home folder",
        required: false,
        target: "sessions.path",
        label: "Codex home",
        help: "For a copy of that folder from another machine.",
      },
    ],
  },
  {
    type: "google_takeout",
    label: "Google Takeout",
    blurb: "Google Chat, Voice, Gemini, Maps and YouTube from an export.",
    keywords: ["google", "takeout", "chat", "voice", "youtube", "maps", "gemini"],
    kind: "export",
    icon: "google_takeout",
    defaultName: "google-takeout",
    nameHint: "My Google Takeout",
    wizard: true,
    // Every feed starts ticked: an export holds what its owner asked
    // Google for, so reading all of it is what they expect. The provider
    // still defaults each feed off, so a config that names none reads
    // none (providers/google_takeout/INGEST.md).
    sections: [
      { heading: "Takeout folder", fields: ["export.path"] },
      {
        heading: "Conversations",
        fields: ["export.google_chat", "export.google_voice", "export.google_voice_include_spam"],
      },
      {
        heading: "Activity",
        fields: [
          "export.youtube_watch_history",
          "export.youtube_subscriptions",
          "export.maps_reviews",
          "export.maps_saved_places",
          "export.maps_photos",
          "export.gemini_apps",
        ],
      },
    ],
    fields: [
      {
        kind: "path",
        picks: "dir",
        pickTitle: "Choose your unzipped Google Takeout folder",
        required: true,
        target: "export.path",
        label: "Takeout folder",
        help:
          "The unzipped export: the Takeout folder holding Google Chat, Voice, YouTube and " +
          "YouTube Music and the rest, ~/Downloads/Takeout say. Gmail is not read here: its .mbox is an email source of " +
          "its own.",
      },
      {
        kind: "bool",
        target: "export.google_chat",
        label: "Google Chat",
        default: true,
        help: "Direct messages and spaces, with their attachments.",
      },
      {
        kind: "bool",
        target: "export.google_voice",
        label: "Google Voice",
        default: true,
        help: "Texts, voicemails, calls and bills.",
      },
      {
        kind: "bool",
        target: "export.google_voice_include_spam",
        requires: "export.google_voice",
        label: "Include Voice spam",
        default: true,
        help: "Also reads Voice/Spam. Bulky, and rarely worth searching.",
      },
      {
        kind: "bool",
        target: "export.youtube_watch_history",
        label: "YouTube watch history",
        default: true,
      },
      {
        kind: "bool",
        target: "export.youtube_subscriptions",
        label: "YouTube subscriptions",
        default: true,
      },
      {
        kind: "bool",
        target: "export.maps_reviews",
        label: "Maps reviews",
        default: true,
      },
      {
        kind: "bool",
        target: "export.maps_saved_places",
        label: "Maps saved places",
        default: true,
      },
      {
        kind: "bool",
        target: "export.maps_photos",
        label: "Maps photos and videos",
        default: true,
      },
      {
        kind: "bool",
        target: "export.gemini_apps",
        label: "Gemini activity",
        default: true,
      },
    ],
  },
  {
    type: "facebook",
    label: "Facebook",
    blurb: "Posts, photo albums, comments, reactions and friends from a data export.",
    keywords: ["facebook", "meta", "export", "posts", "photos", "friends"],
    kind: "export",
    icon: "facebook",
    defaultName: "facebook",
    nameHint: "My Facebook",
    wizard: true,
    sections: [{ heading: "Export folder", fields: ["export.path"] }],
    fields: [
      {
        kind: "path",
        picks: "dir",
        pickTitle: "Choose your unpacked Facebook export folder",
        required: true,
        target: "export.path",
        label: "Export folder",
        help:
          'The unzipped "Download your information" export, requested in JSON format: the ' +
          "folder holding your_facebook_activity, connections and the rest. The HTML format is " +
          "not read.",
      },
    ],
  },
  {
    type: "linkedin",
    label: "LinkedIn",
    blurb: "Messages and connections from a data export.",
    keywords: ["linkedin", "export", "connections"],
    kind: "export",
    icon: "linkedin",
    defaultName: "linkedin",
    nameHint: "My LinkedIn",
    wizard: true,
    sections: [
      { heading: "Export folder", fields: ["export.path"] },
      { heading: "Profile photos", fields: ["export.fetch_photos"] },
    ],
    fields: [
      {
        kind: "path",
        picks: "dir",
        pickTitle: "Choose your unzipped LinkedIn export folder",
        required: true,
        target: "export.path",
        label: "Export folder",
        help:
          'The unzipped "Get a copy of your data" export: the folder of CSV files, ~/Downloads/LinkedInDataExport say. Every CSV in' +
          " it is read.",
      },
      {
        kind: "bool",
        target: "export.fetch_photos",
        label: "Fetch each connection's public profile photo from linkedin.com",
        default: false,
        help:
          "The export has no photos. This is the one part of this source that goes online. No " +
          "login is needed.",
      },
    ],
  },
  {
    type: "signal",
    label: "Signal",
    blurb: "Decrypt and copy an Android Signal backup.",
    keywords: ["signal", "backup", "messages", "sms", "chat"],
    kind: "export",
    icon: "signal",
    defaultName: "signal",
    nameHint: "Signal on my phone",
    wizard: true,
    before: {
      text: "This reads a Signal backup from an Android phone. You need two things:",
      needs: [
        "The backup folder from the phone, copied to this computer.",
        "The backup passphrase Signal showed when backups were turned on.",
      ],
    },
    sections: [
      { heading: "Backup folder", fields: ["backup.path"] },
      { heading: "Passphrase", fields: ["backup.aep_env_var"] },
    ],
    fields: [
      {
        kind: "path",
        picks: "dir",
        pickTitle: "Choose your Signal backups folder",
        required: true,
        target: "backup.path",
        label: "Backup folder",
        help: "The folder holding your signal-backup-* files. The newest one in it is the one read.",
      },
      {
        kind: "text",
        target: "backup.aep_env_var",
        label: "Passphrase",
        envVar: { default: "SIGNAL_BACKUP_PASSPHRASE", holds: "passphrase" },
        help: "Name of the environment variable holding the passphrase, not the passphrase itself.",
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
    blurb: "Decrypt and copy an Android crypt15 backup.",
    keywords: ["whatsapp", "backup", "messages", "chat"],
    kind: "export",
    icon: "whatsapp",
    defaultName: "whatsapp",
    nameHint: "WhatsApp old phone",
    wizard: true,
    before: {
      text:
        "This reads a WhatsApp backup from an Android phone, protected with a 64-digit key " +
        "(not a password). You need two things:",
      needs: [
        "The WhatsApp folder from the phone, copied to this computer.",
        "The 64-digit key WhatsApp showed when the encrypted backup was turned on.",
      ],
    },
    sections: [
      { heading: "WhatsApp folder", fields: ["backup.path"] },
      { heading: "Backup key", fields: ["backup.key_env_var"] },
    ],
    fields: [
      {
        kind: "path",
        picks: "dir",
        pickTitle: "Choose your WhatsApp backup folder",
        required: true,
        target: "backup.path",
        label: "WhatsApp folder",
        help: "Choose the folder named WhatsApp: the one with Databases and Media inside it.",
      },
      {
        kind: "text",
        target: "backup.key_env_var",
        label: "Backup key",
        envVar: { default: "WHATSAPP_BACKUP_DECRYPTION_KEY", holds: "64-digit key" },
        help:
          "Name of the environment variable holding the hex-encoded 32-byte root key, not the " +
          "key itself.",
      },
    ],
  },
  {
    type: "sms_backup_restore",
    label: "SMS & calls",
    blurb: "Android SMS Backup & Restore XML exports.",
    keywords: ["sms", "mms", "calls", "android", "texts"],
    kind: "export",
    icon: "sms",
    defaultName: "sms",
    nameHint: "Texts and calls",
    wizard: true,
    sections: [{ heading: "Backup folder", fields: ["backup.path"] }],
    fields: [
      {
        kind: "path",
        picks: "dir",
        pickTitle: "Choose your SMS Backup & Restore folder",
        required: true,
        target: "backup.path",
        label: "Backup folder",
        help:
          "The folder the Android app SMS Backup & Restore writes its sms-*.xml and calls-*.xml" +
          " files to, copied off the phone: ~/Documents/SMSBackupRestore, say. The path of a single .xml file, typed in, works " +
          "too.",
      },
    ],
  },
  {
    type: "beeper",
    label: "Beeper",
    blurb:
      "Read Beeper Texts' local store across its networks. Poorly supported — expect rough edges.",
    keywords: ["beeper", "matrix", "chat", "imessage"],
    kind: "export",
    icon: "beeper",
    defaultName: "beeper",
    nameHint: "Beeper on this Mac",
    wizard: false,
  },

  {
    type: "pdf",
    label: "PDFs",
    blurb: "Convert a directory tree of PDFs into searchable markdown.",
    keywords: ["pdf", "documents", "papers", "files"],
    kind: "local",
    icon: "pdf",
    defaultName: "pdfs",
    nameHint: "Papers and manuals",
    wizard: true,
    sections: [{ heading: "PDF folder", fields: ["fswalk.path"] }],
    fields: [
      {
        kind: "path",
        picks: "dir",
        pickTitle: "Choose the folder of PDFs to index",
        required: true,
        target: "fswalk.path",
        label: "PDF folder",
        help:
          "Every PDF in this folder and the folders inside it. The same file in two places " +
          "counts as one document. A scanned PDF with no text is recorded but not converted.",
      },
      {
        kind: "string_list",
        target: "ignore",
        label: "Ignore patterns",
        help:
          "Gitignore-shaped patterns, comma-separated (drafts/**, **/scans/**), on top of any " +
          ".gitignore in the tree.",
      },
      {
        kind: "bytes",
        target: "max_bytes",
        label: "Skip files larger than",
        help: "Empty is the 512 MiB default.",
      },
    ],
  },
  {
    type: "fsindex",
    label: "File index",
    blurb: "Index a directory tree — paths, sizes, content hashes.",
    keywords: ["files", "filesystem", "index", "directory", "disk"],
    kind: "local",
    icon: "fsindex",
    defaultName: "fsindex",
    nameHint: "My home folder",
    wizard: true,
    sections: [
      { heading: "Folder", fields: ["fswalk.path"] },
      { heading: "Scan", fields: ["stamp"], advanced: true },
    ],
    fields: [
      {
        kind: "path",
        picks: "dir",
        pickTitle: "Choose the folder to index",
        required: true,
        target: "fswalk.path",
        label: "Folder",
        help:
          "Records the path, kind, size and content hash of everything in this folder and the " +
          "folders inside it. Files are not turned into pages: this source appears in Datalib " +
          "as a storage report.",
      },
      {
        kind: "bool",
        target: "stamp",
        label: "Write UUID breadcrumbs into the tree",
        default: false,
        help:
          "Off keeps the scan read-only. On writes a UUID into the .fsindex.yaml of any " +
          "directory that set stamp_me_with_uuid: true, so it keeps one identity across moves " +
          "and renames.",
      },
    ],
  },
  {
    type: "media",
    label: "Music, photos & video",
    blurb: "Index a media tree — tags, EXIF, playlists, and a metadata-free content hash.",
    keywords: ["music", "photos", "video", "mp3", "jpeg", "raw", "dng", "playlists", "media"],
    kind: "local",
    icon: "media",
    defaultName: "media",
    nameHint: "Photos and music",
    wizard: true,
    // Download-only: media has no text to convert, so nothing is
    // rendered and no render step is declared.
    renderStep: false,
    sections: [
      { heading: "Media folder", fields: ["fswalk.path"] },
      { heading: "Scan", fields: ["playlists", "skip_dataless"], advanced: true },
    ],
    fields: [
      {
        kind: "path",
        picks: "dir",
        pickTitle: "Choose your media folder",
        required: true,
        target: "fswalk.path",
        label: "Media folder",
        help:
          "Scanned for audio, images, video and .m3u playlists. Retagging a track or re-" +
          "rendering a preview does not make a file look new.",
      },
      {
        kind: "bool",
        target: "playlists",
        label: "Index .m3u playlists",
        default: true,
        help:
          "Records each playlist's entries in order, missing tracks included. Streaming .m3u8 " +
          "manifests are skipped.",
      },
      {
        kind: "bool",
        target: "skip_dataless",
        label: "Skip cloud placeholders",
        default: true,
        help:
          "Leaves Dropbox online-only and iCloud evicted files alone. Turn off only if the " +
          "filesystem reports no block counts.",
      },
    ],
  },
  {
    type: "airvisual",
    method: "export",
    label: "AirVisual",
    blurb: "IQAir AirVisual Pros' own history, read off each unit's network share.",
    keywords: ["airvisual", "iqair", "air quality", "co2", "pm2.5", "sensor", "samba"],
    kind: "local",
    icon: "airvisual",
    defaultName: "airvisual",
    nameHint: "Air quality",
    // One `devices` entry per Pro, each with its own share path; the
    // wizard has no list field yet, so this one is written by hand
    // (see docs/user/config_examples/all_sources.toml).
    wizard: false,
  },
  {
    type: "lightroom",
    label: "Lightroom",
    blurb: "Copy a Lightroom Classic catalog, with full history.",
    keywords: ["lightroom", "photos", "adobe", "catalog", "sqlite", "images", "backup", "zip"],
    kind: "local",
    icon: "lightroom",
    defaultName: "lightroom",
    nameHint: "Lightroom catalog",
    wizard: true,
    // Download-only: a photo catalog isn't chat-shaped, so nothing is
    // rendered and no render step is declared.
    renderStep: false,
    requiresOneOf: ["catalog.path", "backups.path"],
    intro:
      "Copies a Lightroom Classic catalog, with full history. Choose the catalog, the backups " +
      "folder, or both.",
    sections: [
      { heading: "Catalog file", fields: ["catalog.path"] },
      { heading: "Backups folder", fields: ["backups.path"] },
      { heading: "Catalog database", fields: ["skip_xmp", "snapshot", "gc"], advanced: true },
    ],
    fields: [
      {
        kind: "path",
        picks: "file",
        pickTitle: "Choose your Lightroom catalog",
        extensions: ["lrcat", "zip"],
        startIn: "~/Pictures/Lightroom",
        target: "catalog.path",
        label: "Catalog file",
        help:
          "A .lrcat catalog, or one of Lightroom's backup .zip files. Lightroom keeps it in " +
          "~/Pictures/Lightroom.",
      },
      {
        kind: "path",
        picks: "dir",
        pickTitle: "Choose your Lightroom backups folder",
        startIn: "~/Pictures/Lightroom",
        target: "backups.path",
        label: "Backups folder",
        help:
          "The folder Lightroom writes its backups into, by default a Backups folder beside the" +
          " catalog in ~/Pictures/Lightroom. Each backup becomes one point in the history, " +
          "dated when it was taken, so the history reaches back before your first sync.",
      },
      {
        kind: "bool",
        target: "skip_xmp",
        label: "Skip XMP packets and search indexes",
        default: false,
        help: "The bulkiest columns, wholly derived from columns that stay.",
      },
      {
        kind: "bool",
        target: "snapshot",
        label: "Snapshot before reading",
        default: true,
        help: "Takes a VACUUM INTO copy first. An open catalog can't then be read half-written.",
      },
      {
        kind: "bool",
        target: "gc",
        label: "Collect unreachable chunks each sync",
        default: false,
        help:
          "Much smaller store, history unaffected, but rewrites the whole chunk store every " +
          "sync.",
      },
    ],
  },
  {
    type: "apple_messages",
    label: "Apple Messages",
    blurb: "Copy the Messages app's own database, with full history.",
    keywords: ["apple", "messages", "imessage", "sms", "texts", "chat.db", "iphone"],
    kind: "local",
    icon: "apple_messages",
    defaultName: "apple-messages",
    nameHint: "Messages on this Mac",
    wizard: true,
    intro:
      "Copies your full Messages history from this Mac. Photos, videos and files in a " +
      "conversation are listed by name; the files themselves are not copied.",
    sections: [
      { heading: "Messages folder", fields: ["messages.path"] },
      { heading: "Messages database", fields: ["skip_churn", "snapshot", "gc"], advanced: true },
    ],
    fields: [
      {
        kind: "path",
        // Choosing the folder here is what grants the app access to it on
        // macOS (docs/dev/wizard_file_pickers.md) — the same wall Photos
        // sits behind. Not the file: picking chat.db grants that one file,
        // and the snapshot also reads chat.db-wal beside it.
        picks: "dir",
        pickTitle: "Choose your Messages folder",
        startIn: "~/Library/Messages",
        required: true,
        target: "messages.path",
        label: "Messages folder",
        guarded: "Let Datalib read your Messages folder",
        help:
          "The folder the Messages app keeps its database in, ~/Library/Messages. A copied " +
          "chat.db file, typed in, works too.",
      },
      {
        kind: "bool",
        target: "skip_churn",
        label: "Skip the app's counters and sync queues",
        default: true,
        help:
          "Messages rewrites these tables continuously; off, every sync commits even when " +
          "nobody messaged. No message data lives there.",
      },
      {
        kind: "bool",
        target: "snapshot",
        label: "Snapshot before reading",
        default: true,
        help:
          "Takes a VACUUM INTO copy first. Messages keeps the database open, so this is the " +
          "only consistent read.",
      },
      {
        kind: "bool",
        target: "gc",
        label: "Collect unreachable chunks each sync",
        default: false,
        help:
          "Much smaller store, history unaffected, but rewrites the whole chunk store every " +
          "sync.",
      },
    ],
  },
  {
    type: "apple_photos",
    label: "Apple Photos",
    blurb: "Copy an Apple Photos library's database, with full history.",
    keywords: ["apple", "photos", "photoslibrary", "iphone", "icloud", "sqlite", "images"],
    kind: "local",
    icon: "apple_photos",
    defaultName: "apple_photos",
    nameHint: "Photos library",
    wizard: true,
    // Download-only, like lightroom: nothing is rendered.
    renderStep: false,
    intro:
      "Copies the database inside your Photos library (database/Photos.sqlite), with full history.",
    sections: [
      { heading: "Photos library", fields: ["library.path"] },
      { heading: "Photos database", fields: ["skip_history", "snapshot", "gc"], advanced: true },
    ],
    fields: [
      {
        kind: "path",
        // A .photoslibrary is a package: the folder picker cannot select
        // it, and choosing it here is also what grants the app access to
        // it on macOS (docs/dev/wizard_file_pickers.md).
        picks: "file",
        pickTitle: "Choose your Photos library",
        extensions: ["photoslibrary"],
        startIn: "~/Pictures",
        required: true,
        target: "library.path",
        label: "Photos library",
        guarded: "Let Datalib read your Photos library",
        help:
          "The library bundle, usually Photos Library.photoslibrary in ~/Pictures. Its " +
          "database/Photos.sqlite is what gets copied.",
      },
      {
        kind: "bool",
        target: "skip_history",
        label: "Skip Core Data's change log and daemon bookkeeping",
        default: true,
        help:
          "Photos writes these continuously; off, every sync commits even when no photo " +
          "changed. No photo data lives there.",
      },
      {
        kind: "bool",
        target: "snapshot",
        label: "Snapshot before reading",
        default: true,
        help:
          "Takes a VACUUM INTO copy first. Photos' daemons keep the library open, so this is " +
          "the only consistent read.",
      },
      {
        kind: "bool",
        target: "gc",
        label: "Collect unreachable chunks each sync",
        default: false,
        help:
          "Much smaller store, history unaffected, but rewrites the whole chunk store every " +
          "sync.",
      },
    ],
  },
  {
    type: "perseus",
    method: "github",
    label: "Perseus library",
    blurb: "Classical texts from the Perseus Digital Library.",
    keywords: ["perseus", "greek", "latin", "classics", "sample"],
    kind: "local",
    icon: "perseus",
    defaultName: "perseus",
    nameHint: "Greek and Latin texts",
    wizard: false,
  },
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
