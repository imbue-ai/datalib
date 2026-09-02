// The specs that rewrite the data root's `config.toml`.
//
// Shared by `playwright.config.ts`, which gives each one a data root
// and a backend of its own, and by `global-setup.ts`, which checks
// that nothing outside the list writes a config. Two readers, one
// list — the list is the thing that keeps the suite parallelizable, so
// it should not be possible to be on one side of it and not the other.
export const CONFIG_MUTATING = [
  "manager2-sync",
  "manager2-name",
  "grid-source-name",
  "sources-view",
  "wizard-select",
] as const;
