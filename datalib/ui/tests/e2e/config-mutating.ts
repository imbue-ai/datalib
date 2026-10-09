// The specs that rewrite the data root's `config.toml` — or any other
// store under it that every spec would otherwise share.
export const CONFIG_MUTATING = [
  "config-error",
  "data-sources-browse",
  "data-sources-sync",
  "data-sources-streaming",
  "data-sources-control",
  "data-sources-name",
  // Syncs, which write the root's stores.
  "data-sources-layout",
  "data-sources-menu",
  "grid-source-id",
  // Writes the remote-media allow-list, not the config.
  "remote-images-load",
  "wizard-select",
  "wizard-email",
  "wizard-slack",
  // Writes the library's saved layout (system/ui-state/layout.json).
  "containers",
  // Links handles to contacts in the contacts app's store, on a root
  // that has the app (`contactsRoot` in playwright.config.ts).
  "contacts",
] as const;
