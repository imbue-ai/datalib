<!--
  FeedbackButton — drop-in inline button for page-level "Feedback…".
  Each call site that has a stable entity UUID (e.g. conversation page,
  account page, project page) can park one of these next to the page
  title. The surface is always `page_header`; for finer-grained targets
  the right-click context-menu / preview-pane cascade is the right path.
-->
<script setup lang="ts">
import { ref, useTemplateRef } from "vue";

import FeedbackModal from "./FeedbackModal.vue";
import { buildContext, type FeedbackContext } from "../feedback/context";

const props = defineProps<{
  /** Stable UUID for the entity the user is "on" — drives target_uuids. */
  entityUuid: string;
  /** Discriminator for the page_header payload. Today only
   * "conversation" is supported in the schema; other entity kinds will
   * require a schema bump first. */
  entityKind?: "conversation";
  /** Short human label for the modal title bar. */
  label?: string;
}>();

const btn = useTemplateRef<HTMLButtonElement>("btn");
const open = ref(false);
const context = ref<FeedbackContext | null>(null);
const surfaceLabel = ref("");

function onClick() {
  // Use the button itself as the breadcrumb anchor so the DOM path
  // captures the surrounding header (h2 / page title) but stops well
  // short of the whole app shell.
  context.value = buildContext({
    surface: "page_header",
    anchor: btn.value,
    targetUuids: [props.entityUuid],
    payload: {
      entity_kind: props.entityKind ?? "conversation",
      entity_uuid: props.entityUuid,
    },
  });
  surfaceLabel.value = props.label ?? "Page";
  open.value = true;
}
</script>

<template>
  <button
    ref="btn"
    type="button"
    class="fb-btn-inline"
    title="File feedback on this page"
    aria-label="Feedback on page"
    @click="onClick"
  >
    💬
  </button>
  <FeedbackModal
    :open="open"
    :surface-label="surfaceLabel"
    :context="context"
    @close="open = false"
  />
</template>

<style scoped>
.fb-btn-inline {
  display: inline-flex;
  align-items: center;
  justify-content: center;
  vertical-align: baseline;
  padding: 0 0.25rem;
  margin: 0 0 0 0.25rem;
  background: transparent;
  border: 1px solid transparent;
  border-radius: 4px;
  color: inherit;
  font: inherit;
  line-height: 1;
  cursor: pointer;
  opacity: 0.7;
}
.fb-btn-inline:hover {
  opacity: 1;
  border-color: var(--fw-muted, #94a3b8);
}
</style>
