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
//
// Two levels, because that is how the data is shaped: Slack renders one
// document per *thread*, and every message in a thread carries that
// thread's markdown_uuid. So a channel is a list of threads, and only a
// thread maps to a document worth opening. Going straight from channel
// to document would pick one arbitrary thread — which looks exactly
// like a channel that holds a single message.

export default function channels(appletId) {
  const base = `/applet/${appletId}`;

  return (root, ctx) => {
    let cancelled = false;
    // Restored on re-render so a card that was showing a channel's
    // threads still is. The host round-trips this string opaquely.
    let channel = ctx.initialState || null;

    const style = document.createElement("style");
    style.textContent = `
      .sv { font: 13px/1.5 system-ui, -apple-system, sans-serif; color: var(--datalib-fg, inherit); }
      .sv-head { padding: 8px 12px; opacity: .6; border-bottom: 1px solid var(--datalib-border, #8884); display: flex; gap: 8px; align-items: baseline; }
      .sv-back { cursor: pointer; text-decoration: underline; }
      .sv-row { padding: 7px 12px; cursor: pointer; border-bottom: 1px solid var(--datalib-border, #8882); display: flex; gap: 10px; align-items: baseline; }
      .sv-row:hover { background: var(--datalib-hover, rgba(127,127,127,.12)); }
      .sv-name { font-weight: 600; }
      .sv-title { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
      .sv-meta { opacity: .55; font-variant-numeric: tabular-nums; margin-left: auto; white-space: nowrap; }
      .sv-empty, .sv-err { padding: 12px; opacity: .7; }
      .sv-err { white-space: pre-wrap; font: 11px/1.5 ui-monospace, Menlo, monospace; }
      .sv-warn { padding: 7px 12px; font-size: 12px; opacity: .8; border-bottom: 1px solid var(--datalib-border, #8882); }
    `;
    root.appendChild(style);

    const wrap = document.createElement("div");
    wrap.className = "sv";
    root.appendChild(wrap);

    function plural(n, one, many) {
      return `${n} ${n === 1 ? one : many}`;
    }

    // Channel names already carry their '#' in the data, so prefixing
    // one here produced '##cat-qi'. Add it only when it is missing.
    function hash(name) {
      return name.startsWith("#") ? name : `#${name}`;
    }

    function warnRow(warnings) {
      if (!warnings || warnings.length === 0) return null;
      const warn = document.createElement("div");
      warn.className = "sv-warn";
      warn.textContent = `${plural(warnings.length, "path", "paths")} could not be read; this list is incomplete.`;
      warn.title = warnings.join("\n");
      return warn;
    }

    function row(children, onPick) {
      const el = document.createElement("div");
      el.className = "sv-row";
      el.append(...children);
      el.addEventListener("click", onPick);
      return el;
    }

    function show(nodes) {
      if (cancelled) return;
      wrap.replaceChildren(...nodes.filter(Boolean));
    }

    function fail(e) {
      const err = document.createElement("div");
      err.className = "sv-err";
      // The gateway answers 502 with a JSON `error` when the applet
      // will not start; showing it beats an empty list that reads as
      // "no data".
      err.textContent = `Could not reach applet "${appletId}":\n${e.message}`;
      show([err]);
    }

    async function get(path) {
      const r = await fetch(`${base}${path}`);
      const body = await r.text();
      if (!r.ok) throw new Error(body || `HTTP ${r.status}`);
      return JSON.parse(body);
    }

    function loading(what) {
      const el = document.createElement("div");
      el.className = "sv-empty";
      el.textContent = `Loading ${what}…`;
      show([el]);
    }

    function showChannels(data) {
      ctx.setTitle(data.workspace ? `Slack — ${data.workspace}` : "Slack");
      const head = document.createElement("div");
      head.className = "sv-head";
      head.textContent = `${data.workspace || appletId} — ${plural(data.channels.length, "channel", "channels")}`;

      const nodes = [head, warnRow(data.warnings)];
      if (data.channels.length === 0) {
        const empty = document.createElement("div");
        empty.className = "sv-empty";
        // A configured applet whose step has never run is a normal
        // state on a fresh install, not an error.
        empty.textContent =
          "No channels rendered yet. Run a sync for this source and reopen the card.";
        nodes.push(empty);
      }
      for (const ch of data.channels) {
        const name = document.createElement("span");
        name.className = "sv-name";
        name.textContent = hash(ch.name);
        const meta = document.createElement("span");
        meta.className = "sv-meta";
        meta.textContent = `${plural(ch.threads, "thread", "threads")} · ${ch.messages}`;
        nodes.push(row([name, meta], () => openChannel(ch.name)));
      }
      show(nodes);
    }

    function showThreads(data) {
      ctx.setTitle(`Slack — ${hash(data.channel)}`);
      const head = document.createElement("div");
      head.className = "sv-head";
      const back = document.createElement("span");
      back.className = "sv-back";
      back.textContent = "← channels";
      back.addEventListener("click", () => openChannels());
      const label = document.createElement("span");
      label.textContent = `${hash(data.channel)} — ${plural(data.threads.length, "thread", "threads")}`;
      head.append(back, label);

      const nodes = [head, warnRow(data.warnings)];
      if (data.threads.length === 0) {
        const empty = document.createElement("div");
        empty.className = "sv-empty";
        empty.textContent = "No threads in this channel.";
        nodes.push(empty);
      }
      for (const t of data.threads) {
        const title = document.createElement("span");
        title.className = "sv-title";
        title.textContent = t.title;
        const meta = document.createElement("span");
        meta.className = "sv-meta";
        const day = (t.when_ts || "").slice(0, 10);
        meta.textContent = `${day} · ${t.messages}`;
        nodes.push(
          row([title, meta], () => {
            // Opening a document is a host command, and the source it
            // opens is a builtin view — an applet composes with the
            // rest of the app through card source like anything else.
            ctx.host.openCards(`documentView(${JSON.stringify(t.markdown_uuid)})`);
          }),
        );
      }
      show(nodes);
    }

    function openChannels() {
      channel = null;
      ctx.host.setState("");
      loading("channels");
      get("/channels").then(showChannels).catch(fail);
    }

    function openChannel(name) {
      channel = name;
      ctx.host.setState(name);
      loading(`threads in ${hash(name)}`);
      get(`/threads?channel=${encodeURIComponent(name)}`)
        .then(showThreads)
        .catch(fail);
    }

    ctx.setTitle("Slack");
    if (channel) {
      openChannel(channel);
    } else {
      openChannels();
    }

    return () => {
      cancelled = true;
    };
  };
}
