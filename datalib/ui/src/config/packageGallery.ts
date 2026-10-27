// The packages datalib suggests, shown above the link box wherever a
// package is added. Each is only a `source` to look up and review like a
// pasted link; nothing here is trusted more than one. To list a package,
// add an entry (docs/dev/packages.md §"The gallery").

export type GalleryEntry = {
  /// What a person would paste: `github:owner/repo` or a manifest link.
  source: string;
  name: string;
  description: string;
};

export const GALLERY: GalleryEntry[] = [
  {
    source: "github:qi-imbue/datalib-hello",
    name: "Hello",
    description:
      "An example: a source that writes a greeting each sync, as a document and a time series, with a card that lists them.",
  },
];
