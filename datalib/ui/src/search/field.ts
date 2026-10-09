// The search field's CodeMirror extensions. The document is the query
// text; a value naming a source, a group or a step is drawn over as its
// chip, and the menu offers keys and values from the table's `…/keys`
// and `…/values` (docs/dev/plans/search_autocomplete.md § "The field").
import {
  acceptCompletion,
  autocompletion,
  completionStatus,
  selectedCompletionIndex,
  setSelectedCompletion,
  startCompletion,
  type Completion,
  type CompletionContext,
  type CompletionResult,
} from "@codemirror/autocomplete";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import {
  EditorState,
  Facet,
  Prec,
  StateEffect,
  StateField,
  type Extension,
  type Transaction,
} from "@codemirror/state";
import {
  Decoration,
  EditorView,
  ViewPlugin,
  WidgetType,
  keymap,
  showTooltip,
  type DecorationSet,
  type ViewUpdate,
} from "@codemirror/view";
import { fetchSearchKeys, fetchSearchValues, type KeyValues, type SearchKeySpec } from "@/api";
import { personSource, searchSource } from "@/cards/cardSources";
import {
  canLinkHandles,
  chipCell,
  contactCell,
  contactLook,
  contactsById,
  chipLook,
  chipMenu,
  composeUri,
  copyText,
  handleValue,
  NOBODY,
  people,
  searchQueryFor,
} from "@/cards/contacts";
import {
  browseQuery,
  entities,
  entityCardSource,
  entityCell,
  entityCopyText,
  entityMenu,
} from "@/cards/entities";
import { copyToClipboard } from "@/clipboard";
import { openExternal } from "@/externalLinks";
import { filterToken } from "@/grid/query";
import { pushToast } from "@/toasts";
import { fieldChipMenu, toggleNegate, type FieldMenuEntry, type FieldMenuId } from "./chipMenu";
import {
  chipFor,
  chipKey,
  chipWords,
  CONTACT,
  completingAt,
  keyNamed,
  termValue,
  words,
  type ChipRef,
  type Word,
} from "./queryText";

export const setKeys = StateEffect.define<SearchKeySpec[]>();

/// Each table's keys, asked once per page; a failure is asked again next
/// time, and offers no keys meanwhile.
const keysAsked = new Map<string, Promise<SearchKeySpec[]>>();
export function keysOf(base: string): Promise<SearchKeySpec[]> {
  let asked = keysAsked.get(base);
  if (!asked) {
    asked = fetchSearchKeys(base).catch(() => {
      keysAsked.delete(base);
      return [];
    });
    keysAsked.set(base, asked);
  }
  return asked;
}

const keysField = StateField.define<SearchKeySpec[]>({
  create: () => [],
  update: (keys, tr) => tr.effects.find((e) => e.is(setKeys))?.value ?? keys,
});

/// The word being typed is drawn as text until the cursor leaves it, so
/// `source_id:sla` does not turn into a chip for a source named "sla".
type Span = { from: number; to: number };

function onlySpace(tr: Transaction): boolean {
  let spaces = true;
  tr.changes.iterChanges((fromA, toA, _fromB, _toB, inserted) => {
    const text = tr.startState.sliceDoc(fromA, toA) + inserted.toString();
    if (/\S/.test(text)) spaces = false;
  });
  return spaces;
}

/// A chip opened to be edited, by a double-click or its menu.
const openWord = StateEffect.define<Span>();

const editingField = StateField.define<Span | null>({
  create: () => null,
  update(span, tr) {
    const opened = tr.effects.find((e) => e.is(openWord));
    if (opened) return opened.value;
    const head = tr.state.selection.main.head;
    // A pick from the menu is finished: its chip shows at once.
    if (tr.isUserEvent("input.complete")) return null;
    // Typing or deleting in a word opens it; a space typed or deleted
    // beside one does not, so Backspace past the space after a chip
    // reaches the chip whole.
    if (tr.docChanged && (tr.isUserEvent("input") || tr.isUserEvent("delete"))) {
      if (onlySpace(tr)) return null;
      const w = words(tr.state.doc.toString()).find((w) => w.from <= head && head <= w.to);
      return w ? { from: w.from, to: w.to } : null;
    }
    if (!span) return null;
    const mapped = tr.docChanged
      ? { from: tr.changes.mapPos(span.from, -1), to: tr.changes.mapPos(span.to, 1) }
      : span;
    const { from, to } = tr.state.selection.main;
    return mapped.from <= from && to <= mapped.to ? mapped : null;
  },
});

const redraw = StateEffect.define<null>();

/// The term whose value a chip draws: the one whose value starts at `pos`.
function termAt(state: EditorState, pos: number): Word | undefined {
  return words(state.doc.toString()).find((w) => w.key !== null && w.valueFrom === pos);
}

/// A chip opened to be edited: its value selected as text, and the menu
/// offering the key's values.
function editChip(view: EditorView, w: Word) {
  view.dispatch({
    selection: { anchor: w.valueFrom, head: w.to },
    effects: openWord.of({ from: w.from, to: w.to }),
    scrollIntoView: true,
  });
  startCompletion(view);
}

/// Clicks on a chip: one selects it whole, so Backspace deletes it and
/// typing replaces it; two open it to be edited; the right button opens
/// its menu.
function onChipMouse(view: EditorView, chip: HTMLElement, e: MouseEvent) {
  const w = termAt(view.state, view.posAtDOM(chip));
  if (!w) return;
  e.preventDefault();
  view.focus();
  if (e.type === "contextmenu") {
    // The chip's menu, not the one the field's host gives the bar.
    e.stopPropagation();
    view.dispatch({ effects: showMenu.of(w.valueFrom) });
  } else if (e.button === 0 && e.detail >= 2) {
    editChip(view, w);
  } else if (e.button === 0) {
    view.dispatch({ selection: { anchor: w.valueFrom, head: w.to } });
  }
}

/// What a chip draws from, asked of its resolver as it draws: the answer
/// so far, and the question if nobody has asked.
function answerOf(chip: ChipRef): unknown {
  switch (chip.kind) {
    case "entity":
      return entities.lookup(chip.uri);
    case "person":
      return people.lookup(chip.handle);
    case "contact":
      return contactsById.lookup(chip.id);
  }
}

/// A chip as every surface draws it (docs/dev/chips.md), from what is
/// known now. `shown` is the value as typed, or a contact's name.
function chipDom(chip: ChipRef, shown: string): HTMLAnchorElement {
  const a =
    chip.kind === "entity"
      ? entityCell(chip.uri, shown, entities.get(chip.uri), null)
      : chip.kind === "person"
        ? chipCell(chip.handle, handleValue(chip.handle), people.get(chip.handle), canLinkHandles())
        : contactCell(
            chip.id,
            shown.startsWith(CONTACT) ? "a contact" : shown,
            contactsById.get(chip.id),
          );
  // A chip's href is never followed (docs/dev/chips.md § "Clicks").
  a.addEventListener("click", (e) => e.preventDefault());
  a.draggable = false;
  return a;
}

class ChipWidget extends WidgetType {
  constructor(
    readonly chip: ChipRef,
    readonly shown: string,
    readonly answer: unknown,
    readonly selected: boolean,
  ) {
    super();
  }
  eq(other: ChipWidget): boolean {
    return (
      chipKey(other.chip) === chipKey(this.chip) &&
      other.shown === this.shown &&
      other.answer === this.answer &&
      other.selected === this.selected
    );
  }
  toDOM(view: EditorView): HTMLElement {
    const a = chipDom(this.chip, this.shown);
    if (this.selected) a.classList.add("cm-chip-selected");
    a.addEventListener("mousedown", (e) => onChipMouse(view, a, e));
    a.addEventListener("contextmenu", (e) => onChipMouse(view, a, e));
    return a;
  }
}

function chipDecorations(state: EditorState): DecorationSet {
  const editing = state.field(editingField);
  const sel = state.selection.main;
  const ranges = chipWords(state.doc.toString(), state.field(keysField))
    .filter(({ word }) => !editing || word.to < editing.from || word.from > editing.to)
    .map(({ word, chip }) => {
      const selected = !sel.empty && sel.from <= word.valueFrom && sel.to >= word.to;
      return Decoration.replace({
        widget: new ChipWidget(chip, word.value, answerOf(chip), selected),
      }).range(word.valueFrom, word.to);
    });
  return Decoration.set(ranges);
}

const chips = ViewPlugin.fromClass(
  class {
    decorations: DecorationSet;
    private readonly unsubscribe: (() => void)[];
    constructor(view: EditorView) {
      this.decorations = chipDecorations(view.state);
      const answered = (keys: ReadonlySet<string>) => {
        const shown = chipWords(view.state.doc.toString(), view.state.field(keysField));
        if (shown.some(({ chip }) => keys.has(chipKey(chip)))) {
          view.dispatch({ effects: redraw.of(null) });
        }
      };
      this.unsubscribe = [
        entities.subscribe(answered),
        people.subscribe(answered),
        contactsById.subscribe(answered),
      ];
    }
    update(u: ViewUpdate) {
      const asked = u.transactions.some((t: Transaction) =>
        t.effects.some((e) => e.is(redraw) || e.is(setKeys)),
      );
      if (u.docChanged || u.selectionSet || asked) this.decorations = chipDecorations(u.state);
    }
    destroy() {
      for (const stop of this.unsubscribe) stop();
    }
  },
  {
    decorations: (p) => p.decorations,
    provide: (p) =>
      EditorView.atomicRanges.of((view) => view.plugin(p)?.decorations ?? Decoration.none),
  },
);

/// The chip menu, open on the chip whose value starts at this position.
const showMenu = StateEffect.define<number | null>();

const menuField = StateField.define<number | null>({
  create: () => null,
  update(at, tr) {
    const asked = tr.effects.find((e) => e.is(showMenu));
    if (asked) return asked.value;
    return tr.docChanged || tr.selection ? null : at;
  },
  provide: (f) =>
    showTooltip.from(f, (at) =>
      at === null
        ? null
        : { pos: at, above: false, create: (view) => ({ dom: menuDom(view, at) }) },
    ),
});

/// A chip's name and its own menu entries, as every surface shows them.
function chipNamed(chip: ChipRef, value: string): { name: string; own: FieldMenuEntry[] } {
  if (chip.kind === "entity") {
    const name = entities.get(chip.uri)?.label || value;
    return { name, own: entityMenu(chip.uri, name) };
  }
  if (chip.kind === "contact") {
    const name = contactLook("a contact", contactsById.get(chip.id)).text;
    return {
      name,
      own: [
        { id: "copy-name", label: `Copy “${name}”` },
        { id: "search", label: `Everything from ${name}`, separator: true },
      ],
    };
  }
  const who = people.get(chip.handle) ?? NOBODY;
  const shown = handleValue(chip.handle);
  return {
    name: chipLook(chip.handle, shown, who, false).text,
    // Linking a handle is the popover's, which the field does not host.
    own: chipMenu(chip.handle, shown, who, false),
  };
}

function menuDom(view: EditorView, at: number): HTMLElement {
  const dom = document.createElement("div");
  dom.className = "cm-chip-menu";
  dom.setAttribute("role", "menu");
  const w = termAt(view.state, at);
  const chip = chipWords(view.state.doc.toString(), view.state.field(keysField)).find(
    (c) => c.word.valueFrom === at,
  )?.chip;
  if (!w || !chip) return dom;
  const { name, own } = chipNamed(chip, w.value);
  for (const entry of fieldChipMenu(name, w.negate, own)) {
    if (entry.separator)
      dom.append(
        Object.assign(document.createElement("div"), { className: "cm-chip-menu-divider" }),
      );
    const item = document.createElement("div");
    item.className = "cm-chip-menu-item";
    item.setAttribute("role", "menuitem");
    item.textContent = entry.label;
    // On mousedown, so the field keeps its focus.
    item.addEventListener("mousedown", (e) => {
      e.preventDefault();
      view.dispatch({ effects: showMenu.of(null) });
      void runMenu(view, entry.id, w, chip, name);
    });
    dom.append(item);
  }
  return dom;
}

async function runMenu(view: EditorView, id: FieldMenuId, w: Word, chip: ChipRef, name: string) {
  const hooks = view.state.facet(hooksFacet);
  const copy = async (text: string) => {
    if (await copyToClipboard(text)) pushToast(`Copied ${text}`, "info");
    else pushToast("The clipboard refused the copy");
  };
  const person = chip.kind === "person" ? chip.handle : null;
  const uri = chip.kind === "entity" ? chip.uri : null;
  const contact = chip.kind === "contact" ? chip.id : null;
  switch (id) {
    case "edit-text":
      editChip(view, w);
      break;
    case "toggle-negate":
      view.dispatch({ changes: toggleNegate(w) });
      break;
    case "copy-name":
      await copy(name);
      break;
    case "copy-id":
      await copy(person ? handleValue(person) : w.value);
      break;
    case "copy-both":
      await copy(person ? copyText(person, name) : entityCopyText(uri ?? "", name));
      break;
    case "open": {
      const source = uri ? entityCardSource(uri) : person ? personSource(person) : null;
      if (source) hooks?.openCard(source);
      break;
    }
    case "compose": {
      const mailto = person ? composeUri(person) : null;
      if (mailto) void openExternal(mailto);
      break;
    }
    case "browse": {
      const q = uri ? browseQuery(uri) : null;
      if (q) hooks?.openCard(searchSource(q));
      break;
    }
    case "search":
      if (person) hooks?.openCard(searchSource(searchQueryFor(person)));
      if (contact)
        hooks?.openCard(searchSource(filterToken("from", `${CONTACT}${contact}`, false)));
      break;
    case "edit":
      break;
  }
}

/// Escape closes the chip menu before it means anything else.
function closeMenu(view: EditorView): boolean {
  if (view.state.field(menuField) === null) return false;
  view.dispatch({ effects: showMenu.of(null) });
  return true;
}

/// What a key's values are, in the menu beside its name.
function describe(values: KeyValues): string {
  switch (values.kind) {
    case "words":
      return (values.words ?? []).join(" · ");
    case "stamp":
      return "a date";
    case "text":
      return "";
    default:
      return values.kind;
  }
}

/// The menu: keys while a key is typed, then that key's values.
function completions(base: () => string) {
  return async (ctx: CompletionContext): Promise<CompletionResult | null> => {
    const query = ctx.state.doc.toString();
    const at = completingAt(query, ctx.pos);
    if (!at) return null;
    const keys = ctx.state.field(keysField);
    if (at.kind === "key") {
      const options: Completion[] = keys.map((k) => ({
        label: `${k.key}:`,
        detail: describe(k.values),
        type: "keyword",
        apply: (view, _c, from, to) => {
          const insert = `${k.key}:`;
          view.dispatch({
            changes: { from, to, insert },
            selection: { anchor: from + insert.length },
            userEvent: "input.complete",
          });
          if (k.values.kind !== "stamp") startCompletion(view);
        },
      }));
      return { from: at.from, to: at.to, options, validFor: /^[\w.]*$/ };
    }
    const spec = keyNamed(keys, at.key);
    if (!spec || spec.values.kind === "stamp") return null;
    let values;
    try {
      values = await fetchSearchValues(base(), spec.key, at.typed, at.rest);
    } catch {
      return null;
    }
    if (ctx.aborted || values.length === 0) return null;
    const refs = values.map((v) => chipFor(spec.values, v.value));
    const named = (kind: ChipRef["kind"]) =>
      refs.filter((r): r is ChipRef => r?.kind === kind).map(chipKey);
    await Promise.all([
      entities.ask(named("entity")),
      people.ask(named("person")),
      contactsById.ask(named("contact")),
    ]);
    const atEnd = at.to >= query.length;
    const options: ChipCompletion[] = values.map((v, i) => ({
      label: v.value,
      shown: v.label ?? v.value,
      chip: refs[i] ?? undefined,
      detail: v.count === undefined ? undefined : v.count.toLocaleString(),
      // A name picked is matched whole; a chip is a handle or a contact,
      // whole bare. `@rik` is written as `with:` and the value.
      apply: (at.prefix ?? "") + termValue(v.value, spec.partial && !refs[i]) + (atEnd ? " " : ""),
    }));
    return { from: at.from, to: at.to, options, filter: false };
  };
}

type ChipCompletion = Completion & { chip?: ChipRef; shown?: string };

/// A suggestion naming a source, a group, a step, a person or a contact,
/// drawn as the chip the field will show once it is picked.
const chipOption = {
  position: 45,
  render: (c: ChipCompletion): Node | null => (c.chip ? chipDom(c.chip, c.shown ?? c.label) : null),
};

/// Tab takes the chosen suggestion, or the first when none is chosen; with
/// no menu open it moves focus as Tab always does.
function takeSuggestion(view: EditorView): boolean {
  if (completionStatus(view.state) !== "active") return false;
  if (selectedCompletionIndex(view.state) === null) {
    view.dispatch({ effects: setSelectedCompletion(0) });
  }
  // With the menu open, Tab is the menu's even when it takes nothing.
  acceptCompletion(view);
  return true;
}

/// One line: a newline pasted or typed becomes a space.
const oneLine = EditorState.transactionFilter.of((tr) => {
  if (!tr.docChanged || tr.newDoc.lines === 1) return tr;
  const changes = [...tr.newDoc.toString().matchAll(/\n/g)].map((m) => ({
    from: m.index,
    to: m.index + 1,
    insert: " ",
  }));
  return [tr, { changes, sequential: true }];
});

const theme = EditorView.theme({
  "&": { flex: "1 1 auto", minWidth: "0", color: "inherit", background: "transparent" },
  "&.cm-focused": { outline: "none" },
  ".cm-scroller": {
    fontFamily: "inherit",
    lineHeight: "inherit",
    overflowX: "auto",
    overflowY: "hidden",
    scrollbarWidth: "none",
  },
  ".cm-content": { padding: "0", caretColor: "currentColor" },
  ".cm-line": { padding: "0" },
  ".cm-placeholder": { color: "var(--datalib-faint)" },
  ".cm-tooltip": {
    background: "var(--datalib-bg)",
    color: "var(--datalib-fg)",
    border: "1px solid var(--datalib-border)",
    borderRadius: "var(--datalib-radius)",
  },
  ".cm-tooltip.cm-tooltip-autocomplete > ul": {
    fontFamily: "inherit",
    fontSize: "var(--datalib-font-size)",
    maxHeight: "20em",
    maxWidth: "min(40em, 90vw)",
  },
  ".cm-tooltip.cm-tooltip-autocomplete > ul > li": {
    display: "flex",
    alignItems: "center",
    gap: "6px",
    padding: "3px 10px",
    overflow: "hidden",
    textOverflow: "ellipsis",
  },
  ".cm-completionLabel": { overflow: "hidden", textOverflow: "ellipsis" },
  ".cm-chip-option .cm-completionLabel": { display: "none" },
  ".cm-chip-selected": {
    outline: "2px solid var(--datalib-accent)",
    outlineOffset: "1px",
  },
  ".cm-chip-menu": {
    minWidth: "180px",
    maxWidth: "320px",
    padding: "4px 0",
    fontSize: "var(--datalib-font-size)",
  },
  ".cm-chip-menu-item": {
    padding: "5px 14px",
    cursor: "pointer",
    whiteSpace: "nowrap",
    overflow: "hidden",
    textOverflow: "ellipsis",
  },
  ".cm-chip-menu-item:hover": { background: "var(--datalib-hover, rgb(127 127 127 / 15%))" },
  ".cm-chip-menu-divider": {
    height: "1px",
    background: "var(--datalib-border)",
    margin: "4px 0",
  },
  ".cm-tooltip.cm-tooltip-autocomplete > ul > li[aria-selected]": {
    background: "var(--datalib-accent)",
    color: "var(--datalib-accent-fg, #fff)",
  },
  ".cm-completionDetail": {
    marginLeft: "auto",
    paddingLeft: "1.5em",
    fontStyle: "normal",
    opacity: "0.7",
    fontVariantNumeric: "tabular-nums",
  },
});

const hooksFacet = Facet.define<FieldHooks, FieldHooks | undefined>({
  combine: (values) => values[0],
});

export type FieldHooks = {
  /** Open a card beside the field's own: a chip's dashboard, a Browse. */
  openCard: (source: string) => void;
  /** The table's search, whose `/keys` and `/values` the menu asks. */
  base: () => string;
  /** Enter with no suggestion chosen. */
  onSubmit: () => void;
  /** Escape with no menu open. */
  onEscape: () => void;
};

export function fieldExtensions(hooks: FieldHooks): Extension[] {
  return [
    hooksFacet.of(hooks),
    keysField,
    editingField,
    chips,
    menuField,
    EditorView.focusChangeEffect.of((_state, focusing) => (focusing ? null : showMenu.of(null))),
    history(),
    oneLine,
    theme,
    autocompletion({
      override: [completions(hooks.base)],
      selectOnOpen: false,
      icons: false,
      closeOnBlur: true,
      // Nothing is chosen when the menu opens, so Enter cannot take a
      // pick by accident; Tab right after typing takes the first.
      interactionDelay: 0,
      addToOptions: [chipOption],
      // A source, group or step is drawn as its chip instead of its label.
      optionClass: (c: ChipCompletion) => (c.chip ? "cm-chip-option" : ""),
    }),
    Prec.highest(
      keymap.of([
        { key: "Tab", run: takeSuggestion },
        { key: "Escape", run: closeMenu },
      ]),
    ),
    Prec.high(
      keymap.of([
        { key: "Enter", run: () => (hooks.onSubmit(), true) },
        // Told, and passed on: a dialog the field is in still closes.
        { key: "Escape", run: () => (hooks.onEscape(), false) },
      ]),
    ),
    keymap.of([...defaultKeymap, ...historyKeymap]),
    EditorView.contentAttributes.compute(["doc"], (s) => ({ "data-query": s.doc.toString() })),
  ];
}
