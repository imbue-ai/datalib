# Vendored DACTAL

Source: https://dactal.org (`dactal.js`, `dactal_utils.js`), fetched
2026-06-18. The site's third script, `dactal_storage_indexeddb.js`, is
not vendored: the page runs sandboxed, where IndexedDB does not exist,
and `main.js` supplies an in-memory store in its place.

These are pinned, unmodified copies. To update, re-fetch from dactal.org and
diff — the files are dependency-free browser scripts with no build step.

## License

DACTAL is Imbue's own work. The files carry no header and dactal.org
states no license; Imbue licenses these copies under this repository's
MIT license (`LICENSE` at the root), the same as everything else here.
