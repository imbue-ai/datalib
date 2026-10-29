// The search field's chips: which words it draws over, and when one is
// still being typed and so stays text.

import { EditorState } from "@codemirror/state";
import { EditorView } from "@codemirror/view";
import { afterEach, describe, expect, it, vi } from "vitest";

import type { SearchKeySpec } from "@/api";
import { fieldExtensions, setKeys } from "./field";

const KEYS: SearchKeySpec[] = [
  { key: "after", aliases: [], values: { kind: "stamp" }, partial: false },
  { key: "before", aliases: [], values: { kind: "stamp" }, partial: false },
];

const views: EditorView[] = [];
afterEach(() => {
  for (const v of views.splice(0)) v.destroy();
});

function field(doc = ""): EditorView {
  const parent = document.createElement("div");
  document.body.append(parent);
  const view = new EditorView({
    parent,
    state: EditorState.create({
      doc,
      extensions: fieldExtensions({
        base: () => "/applet/test/search",
        openCard: () => {},
        onSubmit: () => {},
        onEscape: () => {},
      }),
    }),
  });
  view.dispatch({ effects: setKeys.of(KEYS) });
  views.push(view);
  return view;
}

function type(view: EditorView, text: string) {
  const at = view.state.doc.length;
  view.dispatch({
    changes: { from: at, insert: text },
    selection: { anchor: at + text.length },
    userEvent: "input.type",
  });
}

const dateChips = (view: EditorView) => view.contentDOM.querySelectorAll("time.chip").length;

describe("a date in the field", () => {
  it("stays text while it is typed, and is a chip once a space follows it", () => {
    const view = field();
    type(view, "after:2026-04-15T10:05:00Z");
    expect(dateChips(view)).toBe(0);
    type(view, " ");
    expect(dateChips(view)).toBe(1);
  });

  /** "Everything after this" on a chip wrote `after:<date>` into a field
   *  whose last word was still open from typing, and the whole new query
   *  stayed text: no chip for any word in it. */
  it("draws every chip of a query written in from outside", () => {
    const view = field();
    type(view, "after:2026");
    const q = "after:2026-04-15T10:05:00Z before:2026-04-16T10:05:00Z";
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: q },
      selection: { anchor: q.length },
    });
    expect(dateChips(view)).toBe(2);
  });

  /** A query typed and run with Enter kept its last word as text after
   *  the person had clicked away from the field. */
  it("draws the last word as a chip once the field loses focus", async () => {
    const view = field();
    view.focus();
    type(view, "after:2026-04-15T10:05:00Z");
    expect(dateChips(view)).toBe(0);
    view.contentDOM.blur();
    await vi.waitFor(() => expect(dateChips(view)).toBe(1), { timeout: 2000 });
  });
});
