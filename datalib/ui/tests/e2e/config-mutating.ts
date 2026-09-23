// The specs that rewrite the data root's `config.toml` — or any other
// store under it that every spec would otherwise share.
export const CONFIG_MUTATING = [
  "config-error",
  "data-sources-browse",
  "data-sources-sync",
  "data-sources-streaming",
  "data-sources-control",
  "data-sources-name",
  // Syncs, which write the root's stores and job queue.
  "data-sources-layout",
  "data-sources-menu",
  "grid-source-id",
  // Writes the remote-media allow-list, not the config.
  "remote-images-load",
  "sources-view",
  "wizard-select",
  "wizard-email",
  "wizard-slack",
] as const;
