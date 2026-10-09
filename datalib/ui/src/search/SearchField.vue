<script setup lang="ts">
// The one search field: a single-line CodeMirror editor over the query
// text, which stays the query (docs/dev/plans/search_autocomplete.md
// § "The field"). Every search bar is one of these, given the table's
// search to ask for keys and values. Its look is in `field.ts`'s theme,
// which CodeMirror mounts in whatever root the field is in; a chip's is
// the host's `chip.css`.
import { onBeforeUnmount, onMounted, useTemplateRef, watch } from "vue";
import { Compartment, EditorState } from "@codemirror/state";
import { EditorView, placeholder as placeholderText } from "@codemirror/view";
import { surface } from "@/surface";
import { fieldExtensions, keysOf, setKeys } from "./field";

const props = defineProps<{
  modelValue: string;
  /** The table's search: `${base}/keys` and `${base}/values`. */
  base: string;
  placeholder?: string;
  label?: string;
  testid?: string;
  /** Where a chip's menu opens a card; the surface's, beside what is
   *  showing, when the host has no say. */
  openCard?: (source: string) => void;
  autofocus?: boolean;
}>();
const emit = defineEmits<{
  "update:modelValue": [query: string];
  submit: [];
  escape: [];
}>();

const host = useTemplateRef<HTMLDivElement>("host");
const hint = new Compartment();
let view: EditorView | null = null;

onMounted(() => {
  const el = host.value;
  if (!el) return;
  view = new EditorView({
    parent: el,
    root: el.getRootNode() as Document | ShadowRoot,
    state: EditorState.create({
      doc: props.modelValue,
      extensions: [
        fieldExtensions({
          base: () => props.base,
          openCard: (source) => (props.openCard ?? surface.value?.showCard)?.(source),
          onSubmit: () => emit("submit"),
          onEscape: () => emit("escape"),
        }),
        hint.of(placeholderText(props.placeholder ?? "")),
        EditorView.contentAttributes.of({
          role: "searchbox",
          "aria-multiline": "false",
          "aria-label": props.label ?? props.placeholder ?? "Search",
          ...(props.testid ? { "data-testid": props.testid } : {}),
        }),
        // Only a change the query does not already hold: the prop's own
        // value written in (below) is not an edit to send back.
        EditorView.updateListener.of((u) => {
          const q = u.state.doc.toString();
          if (u.docChanged && q !== props.modelValue) emit("update:modelValue", q);
        }),
      ],
    }),
  });
  if (props.autofocus) view.focus();
  void keysOf(props.base).then((keys) => view?.dispatch({ effects: setKeys.of(keys) }));
});

watch(
  () => props.modelValue,
  (q) => {
    if (!view || view.state.doc.toString() === q) return;
    view.dispatch({
      changes: { from: 0, to: view.state.doc.length, insert: q },
      selection: { anchor: q.length },
    });
  },
);
watch(
  () => props.placeholder,
  (p) => view?.dispatch({ effects: hint.reconfigure(placeholderText(p ?? "")) }),
);

defineExpose({
  focus: () => view?.focus(),
  selectAll: () => view?.dispatch({ selection: { anchor: 0, head: view.state.doc.length } }),
  blur: () => view?.contentDOM.blur(),
});

onBeforeUnmount(() => {
  view?.destroy();
  view = null;
});
</script>

<template>
  <div
    ref="host"
    class="search-field"
    style="display: flex; flex: 1 1 auto; min-width: 0"
    :data-query="modelValue"
  ></div>
</template>
