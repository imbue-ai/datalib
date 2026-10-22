<script setup lang="ts">
// A group or step drawn as its chip in a card's own template, where no
// grid or document draws it: the same chip, kept live by `entities`,
// with the same double-click and right-click as everywhere
// (docs/dev/chips.md § Clicks). A card that uses it lists it, ChipMenu
// and chip.css in its `styleSources`.
import { onBeforeUnmount, ref, watchEffect } from "vue";
import ChipMenu from "./ChipMenu.ce.vue";
import {
  chipMenuOf,
  chipOf,
  runChipAction,
  type Chip,
  type ChipEntryId,
  type ChipEntry,
} from "./chipActions";
import { entities, entityCell } from "./entities";

const props = defineProps<{
  uri: string;
  /** The name to show until the chip resolves. */
  shown: string;
  /** Keep `shown` as the chip's text: the context names the rest. */
  short?: boolean;
  /** No status dot: a status is drawn right beside the chip. */
  quiet?: boolean;
  /** Open a card, from the double-click or the menu. */
  openCard: (source: string) => void;
}>();

const box = ref<HTMLElement | null>(null);
const version = ref(0);

watchEffect(() => {
  void version.value;
  const el = box.value;
  if (!el) return;
  el.replaceChildren(
    entityCell(props.uri, props.shown, entities.lookup(props.uri), null, {
      keepShown: props.short === true,
      quiet: props.quiet === true,
    }),
  );
});

const stop = entities.subscribe((uris) => {
  if (uris.has(props.uri)) version.value++;
});
onBeforeUnmount(stop);

function chip(): Chip | null {
  const el = box.value?.querySelector<HTMLElement>("a.chip");
  return el ? chipOf(el) : null;
}

const host = () => ({ openCard: (source: string) => props.openCard(source) });

/// The href names something in this app, not a page: a click never follows it.
function onClick(ev: MouseEvent) {
  ev.preventDefault();
}

function onDblClick(ev: MouseEvent) {
  const c = chip();
  if (!c) return;
  ev.preventDefault();
  ev.stopPropagation();
  void runChipAction("open", c, host());
}

const menu = ref<{ chip: Chip; entries: ChipEntry[]; x: number; y: number } | null>(null);

function onContextMenu(ev: MouseEvent) {
  const c = chip();
  if (!c) return;
  ev.preventDefault();
  ev.stopPropagation();
  menu.value = { chip: c, entries: chipMenuOf(c, false), x: ev.clientX, y: ev.clientY };
}

async function onPick(id: ChipEntryId) {
  const m = menu.value;
  menu.value = null;
  if (m) await runChipAction(id, m.chip, host());
}
</script>

<template>
  <span
    ref="box"
    class="entity-chip-box"
    @click="onClick"
    @dblclick="onDblClick"
    @contextmenu="onContextMenu"
  />
  <ChipMenu
    v-if="menu"
    :entries="menu.entries"
    :x="menu.x"
    :y="menu.y"
    @pick="onPick"
    @close="menu = null"
  />
</template>

<style>
.entity-chip-box {
  display: inline-flex;
  min-width: 0;
  max-width: 100%;
}
</style>
