// Icon token → bundled asset URL. A token is a file's name in
// `src/assets/` without its extension, so adding the file is adding the
// icon. Where each mark comes from, and the rules a mark keeps, are in
// `src/assets/README.md`; `scripts/lint_repo.py` holds the catalogs and
// the README's source grid to these files.

const FILES = import.meta.glob<string>("@/assets/*.{svg,png}", {
  eager: true,
  import: "default",
});

const ICONS: Record<string, string> = Object.fromEntries(
  Object.entries(FILES).map(([path, url]) => [path.replace(/^.*\/|\.[a-z]+$/g, ""), url]),
);

export function iconUrl(name: string | null | undefined): string | null {
  return name ? (ICONS[name] ?? null) : null;
}
