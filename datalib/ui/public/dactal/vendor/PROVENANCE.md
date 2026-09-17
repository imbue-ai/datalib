# Vendored DACTAL

Source: https://dactal.org (`dactal.js`, `dactal_utils.js`), fetched
2026-06-18. The site's third script, `dactal_storage_indexeddb.js`, is
not vendored: the page runs sandboxed, where IndexedDB does not exist,
and `main.js` supplies an in-memory store in its place.

These are pinned, unmodified copies. To update, re-fetch from dactal.org and
diff — the files are dependency-free browser scripts with no build step.

## License

DACTAL is Imbue's, so this is a provenance record rather than a licensing
question. See `docs/dev/dactal.md`.
