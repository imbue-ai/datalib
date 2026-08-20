// The Slack applet's card component, served from the content-addressed
// store at /modules/<sha256> and evaluated by the browser exactly once
// no matter how many applet instances offer it.
//
// That sharing is the reason for the shape below. The module closes
// over nothing instance-specific: the namespace arrives as an argument
// to the factory, supplied by the gallery entry the applet wrote
// (`comp.slack_work.channels("slack_work")`). Two workspaces are two
// calls into one module, not two modules.
//
// The default export is the factory card source calls. Its return value
// is a CardRender — `(root, ctx) => teardown` — the same contract every
// builtin card view honors, so nothing about the host had to change to
// host this.
//
// ## Three levels, mirroring Slack
//
//   channels          every channel, with its thread and message counts
//     └ one channel   each thread's *opening message*, replies collapsed
//         └ a thread  the whole conversation
//
// The first two are this card, navigating in place with a back link —
// the same card, so a workspace never costs more than one column. The
// third opens the rendered document as its own card, which in the
// miller layout lands beside this one, the way Slack opens a thread in
// a side panel.
//
// Level three delegates to `documentView` on purpose: the thread
// document is real rendered markdown with formatting, media and edges,
// and reimplementing that here would be a worse copy of something the
// app already does.

export default function channels(appletId) {
  const base = `/applet/${appletId}`;

  return (root, ctx) => {
    let cancelled = false;
    // Restored on re-render so a card showing a channel still is. The
    // host round-trips this string opaquely.
    let channel = ctx.initialState || null;

    const style = document.createElement("style");
    style.textContent = `
      .sv { font: 13px/1.5 system-ui, -apple-system, sans-serif; color: var(--datalib-fg, inherit); }
      .sv-head { padding: 8px 12px; opacity: .6; border-bottom: 1px solid var(--datalib-border, #8884); display: flex; gap: 8px; align-items: baseline; }
      .sv-back { cursor: pointer; text-decoration: underline; }
      .sv-row { padding: 7px 12px; border-bottom: 1px solid var(--datalib-border, #8882); display: flex; gap: 10px; align-items: baseline; }
      .sv-chan { cursor: pointer; }
      .sv-chan:hover { background: var(--datalib-hover, rgba(127,127,127,.12)); }
      .sv-name { font-weight: 600; }
      .sv-meta { opacity: .55; font-variant-numeric: tabular-nums; margin-left: auto; white-space: nowrap; }
      /* A message, laid out like a chat line: who and when above, the
         body below, replies as an affordance under that. */
      .sv-msg { display: block; padding: 9px 12px; border-bottom: 1px solid var(--datalib-border, #8882); }
      .sv-who { display: flex; gap: 8px; align-items: baseline; }
      .sv-author { font-weight: 600; }
      .sv-when { opacity: .5; font-size: 11px; font-variant-numeric: tabular-nums; }
      .sv-text { white-space: pre-wrap; overflow-wrap: anywhere; margin-top: 2px; }
      .sv-replies { margin-top: 4px; font-size: 12px; cursor: pointer; text-decoration: underline; opacity: .8; }
      .sv-open { margin-top: 4px; font-size: 12px; cursor: pointer; text-decoration: underline; opacity: .55; }
      .sv-empty, .sv-err { padding: 12px; opacity: .7; }
      .sv-err { white-space: pre-wrap; font: 11px/1.5 ui-monospace, Menlo, monospace; }
      .sv-warn { padding: 7px 12px; font-size: 12px; opacity: .8; border-bottom: 1px solid var(--datalib-border, #8882); }
    `;
    root.appendChild(style);

    const wrap = document.createElement("div");
    wrap.className = "sv";
    root.appendChild(wrap);

    const plural = (n, one, many) => `${n} ${n === 1 ? one : many}`;

    // Channel names already carry their '#' in the data, so prefixing
    // one here produced '##cat-qi'. Add it only when it is missing.
    const hash = (name) => (name.startsWith("#") ? name : `#${name}`);

    // "2026-07-16 20:30" — enough to place a message, short enough to
    // sit on the same line as its author.
    function stamp(ts) {
      if (!ts) return "";
      const d = new Date(ts);
      if (Number.isNaN(d.getTime())) return ts.slice(0, 16).replace("T", " ");
      const pad = (n) => String(n).padStart(2, "0");
      return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
    }

    function el(tag, cls, text) {
      const e = document.createElement(tag);
      if (cls) e.className = cls;
      if (text !== undefined) e.textContent = text;
      return e;
    }

    function warnRow(warnings) {
      if (!warnings || warnings.length === 0) return null;
      const w = el("div", "sv-warn");
      w.textContent = `${plural(warnings.length, "path", "paths")} could not be read; this list is incomplete.`;
      w.title = warnings.join("\n");
      return w;
    }

    function show(nodes) {
      if (cancelled) return;
      wrap.replaceChildren(...nodes.filter(Boolean));
    }

    function fail(e) {
      // The gateway answers 502 with a JSON `error` when the applet
      // will not start; showing it beats an empty list that reads as
      // "no data".
      const err = el("div", "sv-err");
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
      show([el("div", "sv-empty", `Loading ${what}…`)]);
    }

    // ---- level 1: channels ------------------------------------------------

    function showChannels(data) {
      ctx.setTitle(data.workspace ? `Slack — ${data.workspace}` : "Slack");
      const head = el(
        "div",
        "sv-head",
        `${data.workspace || appletId} — ${plural(data.channels.length, "channel", "channels")}`,
      );
      const nodes = [head, warnRow(data.warnings)];

      if (data.channels.length === 0) {
        // A configured applet whose step has never run is a normal
        // state on a fresh install, not an error.
        nodes.push(
          el(
            "div",
            "sv-empty",
            "No channels rendered yet. Run a sync for this source and reopen the card.",
          ),
        );
      }
      for (const ch of data.channels) {
        const row = el("div", "sv-row sv-chan");
        row.append(
          el("span", "sv-name", hash(ch.name)),
          el(
            "span",
            "sv-meta",
            `${plural(ch.threads, "thread", "threads")} · ${ch.messages}`,
          ),
        );
        row.addEventListener("click", () => openChannel(ch.name));
        nodes.push(row);
      }
      show(nodes);
    }

    // ---- level 2: one channel, opening messages ---------------------------

    function showChannel(data) {
      ctx.setTitle(`Slack — ${hash(data.channel)}`);
      const head = el("div", "sv-head");
      const back = el("span", "sv-back", "← channels");
      back.addEventListener("click", openChannels);
      head.append(
        back,
        el(
          "span",
          null,
          `${hash(data.channel)} — ${plural(data.threads.length, "thread", "threads")}`,
        ),
      );
      const nodes = [head, warnRow(data.warnings)];

      if (data.threads.length === 0) {
        nodes.push(el("div", "sv-empty", "No threads in this channel."));
      }
      for (const t of data.threads) {
        const msg = el("div", "sv-msg");
        const who = el("div", "sv-who");
        who.append(
          el("span", "sv-author", t.author || "unknown"),
          el("span", "sv-when", stamp(t.when_ts)),
        );
        msg.append(who, el("div", "sv-text", t.text || "(no text)"));

        // Level 3, and only where there is something more to see:
        // a thread whose opening message is all there is has nothing
        // to expand.
        if (t.replies > 0) {
          const link = el(
            "div",
            "sv-replies",
            `${plural(t.replies, "reply", "replies")} →`,
          );
          link.addEventListener("click", () => openThread(t.markdown_uuid));
          msg.appendChild(link);
        } else {
          const link = el("div", "sv-open", "open →");
          link.addEventListener("click", () => openThread(t.markdown_uuid));
          msg.appendChild(link);
        }
        nodes.push(msg);
      }
      show(nodes);
    }

    // ---- navigation -------------------------------------------------------

    function openChannels() {
      channel = null;
      ctx.host.setState("");
      loading("channels");
      get("/channels").then(showChannels).catch(fail);
    }

    function openChannel(name) {
      channel = name;
      ctx.host.setState(name);
      loading(`${hash(name)}`);
      get(`/channel?name=${encodeURIComponent(name)}`)
        .then(showChannel)
        .catch(fail);
    }

    // Opening a document is a host command, and the source it opens is
    // a builtin view — an applet composes with the rest of the app
    // through card source like anything else.
    function openThread(markdownUuid) {
      ctx.host.openCards(`documentView(${JSON.stringify(markdownUuid)})`);
    }

    ctx.setTitle("Slack");
    if (channel) openChannel(channel);
    else openChannels();

    return () => {
      cancelled = true;
    };
  };
}
