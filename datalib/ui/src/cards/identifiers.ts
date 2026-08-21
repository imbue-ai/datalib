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

// Replace whole-identifier occurrences of `from` with `to`, using the
// same token scan as referencedIdentifiers — so `obj.from` member
// accesses survive, but `from(`, `from ,` etc. are rewritten. Like the
// scanner it is deliberately over-approximate about strings/comments;
// for the rename use case that's the right bias (better to rewrite a
// mention in a comment than to leave a live reference stale).
export function replaceIdentifier(source: string, from: string, to: string): string {
  const isStart = (c: string) => /[A-Za-z_$]/.test(c);
  const isPart = (c: string) => /[A-Za-z0-9_$]/.test(c);
  const n = source.length;
  let out = "";
  let i = 0;
  while (i < n) {
    if (isStart(source[i])) {
      let j = i + 1;
      while (j < n && isPart(source[j])) j++;
      const token = source.slice(i, j);
      let k = i - 1;
      while (k >= 0 && /\s/.test(source[k])) k--;
      const isMember = k >= 0 && source[k] === ".";
      out += !isMember && token === from ? to : token;
      i = j;
    } else {
      out += source[i];
      i++;
    }
  }
  return out;
}
