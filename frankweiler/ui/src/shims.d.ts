declare module "*.css";
declare module "*.svg" {
  const url: string;
  export default url;
}
declare module "splitpanes" {
  import type { DefineComponent } from "vue";
  export const Splitpanes: DefineComponent<Record<string, unknown>>;
  export const Pane: DefineComponent<Record<string, unknown>>;
}
