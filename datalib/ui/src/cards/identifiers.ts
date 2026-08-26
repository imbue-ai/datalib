// Lexical helpers over card source.
//
// Deliberately its own module with no imports: these are pure string
// functions, and the registries that use them pull in every view in
// the app. Keeping the scanner separate is what lets it be tested
// without loading the UI, and what lets both the alias registry and
// the applet registry share one definition of "a name this source
// refers to".

// Identifiers a piece of source references "freely" — every identifier
// token not immediately preceded by `.` (so `obj.foo` doesn't count as
// a reference to `foo`). Intentionally over-approximate: it scans
// across string/comment contents too, so it may flag a name that isn't
// really used. That only ever causes an extra (harmless) re-render; the
// dangerous direction — missing a real dependency — can't happen,
// because every identifier token is considered.
export function referencedIdentifiers(source: string): Set<string> {
  const ids = new Set<string>();
  const isStart = (c: string) => /[A-Za-z_$]/.test(c);
  const isPart = (c: string) => /[A-Za-z0-9_$]/.test(c);
  const n = source.length;
  let i = 0;
  while (i < n) {
    if (isStart(source[i])) {
      let j = i + 1;
      while (j < n && isPart(source[j])) j++;
      let k = i - 1;
      while (k >= 0 && /\s/.test(source[k])) k--;
      if (k < 0 || source[k] !== ".") ids.add(source.slice(i, j));
      i = j;
    } else {
      i++;
    }
  }
  return ids;
}
