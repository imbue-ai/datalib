// A `datalib:photo/…` name, drawn by whichever applet says it serves
// `photo` (its component's `serves`, docs/dev/plans/photos_applet.md):
// its card for the photo, and the address of its pictures.

import { frontendManifest, gallerySource } from "./frontendRegistry";

/// The applet namespace and component that serve `kind`, or null when no
/// configured applet does.
export function servingComponent(kind: string): { ns: string; name: string } | null {
  for (const [ns, entries] of frontendManifest.value) {
    for (const [name, meta] of entries) {
      if ("serves" in meta && meta.serves?.includes(kind)) return { ns, name };
    }
  }
  return null;
}

/// The card that shows one photo, by its id.
export function photoCardSource(photoId: string): string | null {
  const c = servingComponent("photo");
  return c ? gallerySource(c.ns, c.name, [c.ns, photoId]) : null;
}

/// Where one photo's picture is served, `thumb` or `full`.
export function photoImageUrl(photoId: string, size: "thumb" | "full"): string | null {
  const c = servingComponent("photo");
  if (!c) return null;
  return `/applet/${encodeURIComponent(c.ns)}/photo/${encodeURIComponent(photoId)}/image?size=${size}`;
}
