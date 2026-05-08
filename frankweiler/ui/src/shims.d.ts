declare module "*.css";
declare module "splitpanes" {
  import type { DefineComponent } from "vue";
  export const Splitpanes: DefineComponent<Record<string, unknown>>;
  export const Pane: DefineComponent<Record<string, unknown>>;
}
