// The one way text reaches the clipboard from here: the async API
// first, and the selection-and-`copy` route when that is refused,
// which is what works in a webview that has not granted the
// permission.

export async function copyToClipboard(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return copyBySelection(text, null);
  }
}

/// A link as the clipboard carries it: `html` for whatever pastes rich
/// text (a mail, a doc, a chat box), `text` for everything else. Both
/// routes carry both.
export async function copyRichToClipboard(text: string, html: string): Promise<boolean> {
  try {
    await navigator.clipboard.write([
      new ClipboardItem({
        "text/plain": new Blob([text], { type: "text/plain" }),
        "text/html": new Blob([html], { type: "text/html" }),
      }),
    ]);
    return true;
  } catch {
    return copyBySelection(text, html);
  }
}

/// The `copy` route: a selected textarea, so the browser has something
/// to copy, and a listener that replaces what it would copy with `text`
/// and, where given, `html`.
function copyBySelection(text: string, html: string | null): boolean {
  const ta = document.createElement("textarea");
  ta.value = text;
  const onCopy = (e: ClipboardEvent) => {
    if (!e.clipboardData) return;
    e.clipboardData.setData("text/plain", text);
    if (html !== null) e.clipboardData.setData("text/html", html);
    e.preventDefault();
  };
  document.body.appendChild(ta);
  ta.select();
  document.addEventListener("copy", onCopy, true);
  try {
    return document.execCommand("copy");
  } catch {
    return false;
  } finally {
    document.removeEventListener("copy", onCopy, true);
    document.body.removeChild(ta);
  }
}
