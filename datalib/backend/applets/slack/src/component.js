// The Slack applet's card component, served from the flat module store
// at /modules/<sha256> and evaluated by the browser exactly once no
// matter how many applet instances offer it.
//
// That sharing is the reason for the shape below. The module closes
// over nothing instance-specific: the applet id arrives as an argument
// to the factory, supplied by the gallery snippet the applet itself
// generated (`slack_work.channels("slack_work")`). Two workspaces are
// two calls into one module, not two modules.
//
// The default export is the factory card source calls. Its return
// value is a CardRender — `(root, ctx) => teardown` — which is the
// same contract every builtin card view honors, so nothing about the
// host had to change to host this.

export default function channels(appletId) {
  const base = `/v/${appletId}`;

  return (root, ctx) => {
    let cancelled = false;

    const style = document.createElement("style");
    style.textContent = `
      .sv { font: 13px/1.5 system-ui, -apple-system, sans-serif; color: var(--datalib-fg, inherit); }
      .sv-head { padding: 8px 12px; opacity: .6; border-bottom: 1px solid var(--datalib-border, #8884); display: flex; gap: 8px; }
      .sv-row { padding: 7px 12px; cursor: pointer; border-bottom: 1px solid var(--datalib-border, #8882); display: flex; gap: 10px; align-items: baseline; }
      .sv-row:hover { background: var(--datalib-hover, rgba(127,127,127,.12)); }
      .sv-name { font-weight: 600; }
      .sv-count { opacity: .55; font-variant-numeric: tabular-nums; margin-left: auto; }
      .sv-empty, .sv-err { padding: 12px; opacity: .7; }
      .sv-warn { padding: 7px 12px; font-size: 12px; opacity: .8; border-bottom: 1px solid var(--datalib-border, #8882); }
      .sv-err { white-space: pre-wrap; font: 11px/1.5 ui-monospace, Menlo, monospace; }
    `;
    root.appendChild(style);

    const wrap = document.createElement("div");
    wrap.className = "sv";
    root.appendChild(wrap);

    function show(node) {
      if (cancelled) return;
      wrap.replaceChildren(node);
    }

    function render(data) {
      const frag = document.createDocumentFragment();
      const head = document.createElement("div");
      head.className = "sv-head";
      head.textContent = `${data.workspace || appletId} — ${data.channels.length} channels`;
      frag.appendChild(head);

      // A partial listing must not read as a complete one.
      if (data.warnings && data.warnings.length > 0) {
        const warn = document.createElement("div");
        warn.className = "sv-warn";
        warn.textContent = `${data.warnings.length} path(s) could not be read; this list is incomplete.`;
        warn.title = data.warnings.join("\n");
        frag.appendChild(warn);
      }

      if (data.channels.length === 0) {
        const empty = document.createElement("div");
        empty.className = "sv-empty";
        // A configured applet whose step has never run is a normal
        // state on a fresh install, not an error.
        empty.textContent =
          "No channels rendered yet. Run a sync for this source and reopen the card.";
        frag.appendChild(empty);
      }

      for (const ch of data.channels) {
        const row = document.createElement("div");
        row.className = "sv-row";
        const name = document.createElement("span");
        name.className = "sv-name";
        name.textContent = `#${ch.name}`;
        const count = document.createElement("span");
        count.className = "sv-count";
        count.textContent = `${ch.messages}`;
        row.append(name, count);
        row.addEventListener("click", () => {
          // Opening a document is a host command, and the source it
          // opens is a builtin view — an applet composes with the rest
          // of the app through card source like anything else.
          if (ch.markdown_uuid) {
            ctx.host.openCards(`documentView(${JSON.stringify(ch.markdown_uuid)})`);
          }
        });
        frag.appendChild(row);
      }
      const div = document.createElement("div");
      div.appendChild(frag);
      show(div);
    }

    ctx.setTitle("Slack");
    const loading = document.createElement("div");
    loading.className = "sv-empty";
    loading.textContent = "Loading channels…";
    wrap.appendChild(loading);

    fetch(`${base}/channels`)
      .then(async (r) => {
        const body = await r.text();
        if (!r.ok) throw new Error(body || `HTTP ${r.status}`);
        return JSON.parse(body);
      })
      .then((data) => {
        ctx.setTitle(data.workspace ? `Slack — ${data.workspace}` : "Slack");
        render(data);
      })
      .catch((e) => {
        const err = document.createElement("div");
        err.className = "sv-err";
        // The gateway answers 502 with a JSON `error` when the applet
        // will not start; showing it beats an empty list that reads as
        // "no data".
        err.textContent = `Could not reach applet "${appletId}":\n${e.message}`;
        show(err);
      });

    return () => {
      cancelled = true;
    };
  };
}
