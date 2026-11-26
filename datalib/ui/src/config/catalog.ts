// The source catalog the "+ Data Source" picker renders, and the shape of
// the form descriptors the wizard fills in. No source is compiled in: the
// entries come from the library's packages (`config/packages.ts`) and from
// the programs its config lists with `describe` (`config/programs.ts`),
// `datalib-step` among them, whose own are in
// datalib/backend/datalib_step/src/sources.toml.

import { shallowRef } from "vue";
import type { FanIn, PackageApplet, PackageCommand } from "@/api";

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
  | "labels"
  | "mailboxes"
  | "conversations"
  | "channels"
  | "calendars"
  | "addressbooks"
  | "teams"
  | "projects"
  | "servers";

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
  /// Grouping in the picker. `package` is every entry a package added.
  kind: "api" | "export" | "local" | "package";
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
  /// False for a download-only provider, whose render step writes no
  /// documents: only the storage report, which is all that puts such a
  /// source in the grid. It always gets
  /// that step, and the wizard offers no choice to render and no search
  /// steps, since there is no markdown to search.
  rendersDocuments?: boolean;
  /// The latchkey service name, when the source needs credentials. The
  /// wizard shows its account row only while the params the form
  /// would write reach an origin (`ingestReach`): an import has nothing
  /// to log in to.
  credentialService?: string;
  /// More latchkey services the download reaches, each needing the same
  /// account as `credentialService`: Google Drive's Docs, Sheets and
  /// Slides. One "Sign in with browser" signs in to every one in turn,
  /// and a missing OAuth client is pasted once for all of them
  /// (`multiSignIn.ts`). `label` is the name a person knows it by.
  credentialAlso?: { service: string; label: string }[];
  /// How to register `credentialService` with latchkey when latchkey
  /// has never heard of it, so that a browser login exists at all. The
  /// login flow belongs to the *service* and is fixed when it is
  /// registered, so this is the only moment it can be chosen; a name
  /// latchkey already holds is left exactly as its owner set it up.
  credentialRegister?: import("@/api").ServiceRegistration;
  /// Shown beside the Connect button, when connecting this way costs
  /// something the person should decide about before clicking, or the
  /// sign-in page asks for a choice the download depends on.
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
  /// Offer "Check connection", and a "Load" on every `probe:` field: set
  /// when the source names a `probe` command, which the server runs
  /// (docs/dev/step_protocol.md §"Probe").
  canProbe?: boolean;
  /// One sentence above the form on what this source copies, where
  /// the tile's blurb does not say enough.
  intro?: string;
  /// "Before you start": what the person has to have in hand, for a
  /// source that cannot be set up from this dialog alone. A `needs`
  /// item may hold `code` in backticks. `terminalSignIn` says the steps
  /// are latchkey commands, which cannot reach a protected library.
  before?: { text: string; needs: string[]; terminalSignIn?: boolean };
  /// The form, top to bottom. A field no section names is an advanced
  /// one, drawn under its own label in Advanced options.
  sections?: Section[];
  fields?: Field[];
  /// Set on an entry a package added (docs/dev/packages.md): its steps
  /// run the package's programs.
  package?: PackageOrigin;
  /// Set on an entry a program described (`<program> --describe`): its
  /// steps run that program.
  program?: ProgramOrigin;
  /// The id of the built-in applet that shows this source's data
  /// (`builtinApplets.ts`), which the form offers to turn on.
  applet?: string;
};

/// Which package an entry came from, where it is unpacked relative to the
/// data root, the command each of its steps runs inside it, by function
/// (`ingest`, `render_markdown`, `render_time_series`), the slots of each
/// lock those steps hold, and the applets a source of it gets.
export type PackageOrigin = {
  id: string;
  name: string;
  version: string;
  dir: string;
  commands: Record<string, PackageCommand>;
  locks: Record<string, number>;
  applets: PackageApplet[];
};

/// The program an entry came from, as the config's `[[programs]]` entry
/// names it, and the words after it each of its steps runs, by function.
export type ProgramOrigin = {
  command: string;
  commands: Record<string, PackageCommand>;
};

/// A fixed params value, written without being asked about. See
/// [`CatalogEntry.preset`].
export type Preset = {
  target: string;
  value: string | number | boolean;
  /// Which step it lands on. Defaults to `download`, like a field.
  phase?: FieldPhase;
};

export const KIND_LABELS: Record<CatalogEntry["kind"], string> = {
  package: "From packages in this library",
  api: "Connected accounts",
  export: "Exports & backups",
  local: "On this computer",
};

/// A stable, unique key for an entry — what a `v-for` keys on and what
/// the picker's cursor compares.
export function entryKey(entry: CatalogEntry): string {
  return entry.variantKey ? `${entry.type}:${entry.variantKey}` : entry.type;
}

/// The entries the library's packages add, set from `GET /api/packages`
/// (`config/packages.ts`). Searched before the programs' ones, so a
/// package that reuses a type a program offers is drawn by its own form.
const packageEntries = shallowRef<CatalogEntry[]>([]);

export function setPackageEntries(entries: CatalogEntry[]): void {
  packageEntries.value = entries;
}

/// The entries the config's programs describe, set from
/// `GET /api/programs` (`config/programs.ts`).
const programEntries = shallowRef<CatalogEntry[]>([]);

export function setProgramEntries(entries: CatalogEntry[]): void {
  programEntries.value = entries;
}

/// The fan-ins and the lock slots the config's programs describe.
const described = shallowRef<{ fanIns: FanIn[]; locks: Record<string, number> }>({
  fanIns: [],
  locks: {},
});

export function setProgramDeclarations(fanIns: FanIn[], locks: Record<string, number>): void {
  described.value = { fanIns, locks };
}

/// The steps that read one function of every source, as the config's
/// programs describe them.
export function fanIns(): FanIn[] {
  return described.value.fanIns;
}

/// The slots of every lock a package's or a program's steps hold, by
/// lock name.
export function lockSlots(): Record<string, number> {
  return Object.assign(
    {},
    described.value.locks,
    ...packageEntries.value.map((e) => e.package?.locks ?? {}),
  );
}

function allEntries(): CatalogEntry[] {
  return [...packageEntries.value, ...programEntries.value];
}

/// The first entry for a step type, ignoring variants.
export function catalogFor(type: string): CatalogEntry | undefined {
  return allEntries().find((e) => e.type === type);
}

/// The entry describing a step that already exists: its type, narrowed
/// by which variant its params say it is.
export function catalogForStep(
  type: string | null,
  params: Record<string, unknown>,
): CatalogEntry | undefined {
  if (!type) return undefined;
  const candidates = allEntries().filter((e) => e.type === type);
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
  if (!q) return allEntries();
  return allEntries().filter((e) =>
    [e.label, e.type, ...e.keywords].some((s) => s.toLowerCase().includes(q)),
  );
}
