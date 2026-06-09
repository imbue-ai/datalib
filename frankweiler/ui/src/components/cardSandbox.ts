// Build the sandboxed-iframe `srcdoc` that runs a user's card script.
// Kept in a plain .ts file (not the SFC) so the Vue SFC parser never
// sees a `<script>` or `</script>` token inside a template literal,
// which would prematurely close `<script setup>`.

const OPEN_SCRIPT = "<" + "script>";
const CLOSE_SCRIPT = "<" + "/script>";

export function buildCardSrcdoc(userJs: string): string {
  // Base64-encode the body so the iframe's loader is bytes-safe; also
  // keeps user code (which might contain its own script tags) from
  // perturbing the iframe's HTML parser.
  const b64 = btoa(unescape(encodeURIComponent(userJs)));
  const inner = `
  let userFn = null;
  const src = decodeURIComponent(escape(atob("${b64}")));
  try { userFn = new Function('rows', 'mount', src); }
  catch (e) {
    const div = document.createElement('div');
    div.className = 'card-error';
    div.textContent = 'compile error: ' + (e && e.message ? e.message : e);
    document.body.appendChild(div);
  }
  function render(rs) {
    if (!userFn) return;
    const mount = document.getElementById('mount');
    mount.innerHTML = '';
    try { userFn(rs, mount); }
    catch (e) {
      const div = document.createElement('div');
      div.className = 'card-error';
      div.textContent = 'runtime error: ' + (e && e.stack ? e.stack : String(e));
      mount.appendChild(div);
    }
  }
  window.addEventListener('message', (e) => {
    if (e.data && e.data.type === 'rows') render(e.data.rows);
  });
  window.parent.postMessage({ type: 'ready' }, '*');
`;
  return `<!DOCTYPE html>
<html><head><meta charset="utf-8"><style>
  :root { color-scheme: light dark; }
  body { margin: 0; padding: 8px; font-family: system-ui, sans-serif; }
  .card-error { color: #e35d6a; white-space: pre-wrap; font-family: ui-monospace, monospace; font-size: 12px; }
</style></head>
<body><div id="mount"></div>
${OPEN_SCRIPT}${inner}${CLOSE_SCRIPT}
</body></html>`;
}
