// The photos applet's card: with a photo's uuid, that photo at full size
// with its details; without one, a contact sheet of every photo, newest
// taken first, each opening its own card. Everything drawn from the
// applet's routes is text set as text; nothing is parsed as HTML.

const ID = /^[A-Za-z0-9._][A-Za-z0-9._-]*$/;
const UUID = /^[0-9a-fA-F-]{1,64}$/;

export default function photos(appletId, uuid) {
  // Card source can come from a link: the id must stay one path segment
  // under /applet/, and the uuid one segment under /photo/.
  if (typeof appletId !== "string" || !ID.test(appletId) || appletId === "." || appletId === "..") {
    throw new Error('photos takes the applet\'s id, as in photos("photos")');
  }
  if (uuid !== undefined && (typeof uuid !== "string" || !UUID.test(uuid))) {
    throw new Error("photos takes a photo's uuid as its second argument");
  }
  const base = `/applet/${encodeURIComponent(appletId)}`;
  const cardOf = (id) => `comp.${appletId}.photos(${JSON.stringify(appletId)}, ${JSON.stringify(id)})`;
  const search = (q) => `searchView(${JSON.stringify({ q })})`;
  const quoted = (v) => `"${String(v).replace(/"/g, "")}"`;

  return (root, ctx) => {
    let cancelled = false;
    const style = document.createElement("style");
    style.textContent = `
      .ph { font: 13px/1.5 system-ui, -apple-system, sans-serif; color: var(--datalib-fg, inherit); }
      .ph-sheet { display: grid; grid-template-columns: repeat(auto-fill, minmax(120px, 1fr)); gap: 6px; padding: 8px; }
      .ph-tile { cursor: pointer; aspect-ratio: 1; overflow: hidden; border-radius: 4px; background: var(--datalib-hover, #8882); position: relative; }
      .ph-tile img { width: 100%; height: 100%; object-fit: cover; display: block; }
      .ph-tile span { position: absolute; left: 0; right: 0; bottom: 0; padding: 2px 5px; font-size: 11px; background: #0008; color: #fff; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
      .ph-more { margin: 8px; cursor: pointer; text-decoration: underline; opacity: .7; }
      .ph-frame { position: relative; display: inline-block; max-width: 100%; }
      .ph-frame img { max-width: 100%; max-height: 70vh; display: block; }
      .ph-face { position: absolute; border: 2px solid #ffd54a; border-radius: 3px; box-sizing: border-box; }
      .ph-face span { position: absolute; top: 100%; left: 0; font-size: 11px; background: #000a; color: #fff; padding: 0 4px; white-space: nowrap; }
      .ph-body { padding: 10px 12px; }
      .ph-title { font-size: 16px; font-weight: 600; margin-bottom: 4px; }
      .ph-caption { white-space: pre-wrap; margin-bottom: 8px; }
      .ph-row { display: flex; gap: 8px; padding: 2px 0; }
      .ph-k { opacity: .55; min-width: 90px; }
      .ph-chip { cursor: pointer; text-decoration: underline; margin-right: 8px; }
      .ph-note { font-size: 12px; opacity: .7; padding: 6px 12px; }
      .ph-err { padding: 12px; opacity: .7; white-space: pre-wrap; }
      .ph-toggle { cursor: pointer; text-decoration: underline; font-size: 12px; opacity: .7; margin-left: 8px; }
    `;
    root.appendChild(style);
    const wrap = document.createElement("div");
    wrap.className = "ph";
    root.appendChild(wrap);

    const el = (tag, cls, text) => {
      const e = document.createElement(tag);
      if (cls) e.className = cls;
      if (text !== undefined) e.textContent = text;
      return e;
    };
    const fail = (msg) => {
      wrap.replaceChildren(el("div", "ph-err", msg));
    };
    const get = async (path) => {
      const r = await fetch(base + path);
      const body = await r.json().catch(() => ({}));
      if (!r.ok) throw new Error(body.error || `${r.status} ${r.statusText}`);
      return body;
    };

    async function sheet() {
      ctx.setTitle("Photos");
      const grid = el("div", "ph-sheet");
      wrap.replaceChildren(grid);
      let offset = 0;
      const more = el("div", "ph-more", "More");
      const page = async () => {
        more.remove();
        const data = await get(`/photos?offset=${offset}`);
        if (cancelled) return;
        if (offset === 0 && data.photos.length === 0) {
          wrap.replaceChildren(el("div", "ph-err", "No photos yet. A Lightroom source's photos appear here after it syncs."));
          return;
        }
        for (const p of data.photos) {
          const tile = el("div", "ph-tile");
          tile.title = p.title;
          const img = el("img");
          img.loading = "lazy";
          img.alt = p.title;
          img.src = `${base}/photo/${encodeURIComponent(p.uuid)}/image?size=thumb`;
          tile.append(img, el("span", "", p.title));
          tile.addEventListener("click", () => ctx.host.openCards(cardOf(p.uuid)));
          grid.appendChild(tile);
        }
        if (data.next_offset !== null && data.next_offset !== undefined) {
          offset = data.next_offset;
          wrap.appendChild(more);
        }
      };
      more.addEventListener("click", () => page().catch((e) => fail(String(e.message || e))));
      await page();
    }

    async function one() {
      const data = await get(`/photo/${encodeURIComponent(uuid)}`);
      if (cancelled) return;
      const p = data.photo;
      const title = p.title || p.headline || p.file_name;
      ctx.setTitle(title);

      const frame = el("div", "ph-frame");
      const img = el("img");
      img.alt = title;
      img.src = `${base}/photo/${encodeURIComponent(uuid)}/image?size=full`;
      frame.appendChild(img);
      // Face rectangles are in 0–1 coordinates of the picture. Whether
      // Lightroom measures them before or after a crop is not yet known.
      const faces = p.people.filter((x) => x.face);
      const boxes = faces.map((person) => {
        const f = person.face;
        const box = el("div", "ph-face");
        Object.assign(box.style, {
          left: `${f.left * 100}%`,
          top: `${f.top * 100}%`,
          width: `${(f.right - f.left) * 100}%`,
          height: `${(f.bottom - f.top) * 100}%`,
        });
        box.appendChild(el("span", "", person.name));
        frame.appendChild(box);
        return box;
      });

      const body = el("div", "ph-body");
      body.appendChild(el("div", "ph-title", title));
      if (p.caption) body.appendChild(el("div", "ph-caption", p.caption));
      const row = (label, ...values) => {
        const r = el("div", "ph-row");
        r.appendChild(el("span", "ph-k", label));
        const v = el("span");
        for (const x of values) v.appendChild(typeof x === "string" ? document.createTextNode(x) : x);
        r.appendChild(v);
        body.appendChild(r);
      };
      const chip = (text, q) => {
        const c = el("span", "ph-chip", text);
        c.addEventListener("click", () => ctx.host.openCards(search(q)));
        return c;
      };
      if (p.captured_at) {
        const assumed = p.offset_from === "assumed_utc" ? " (time zone unknown; read as UTC)" : "";
        row("Taken", p.captured_at + assumed);
      }
      if (p.people.length) {
        const r = row("People", ...p.people.map((x) => chip(x.name, `label:${quoted(x.name)}`)));
        if (boxes.length) {
          const t = el("span", "ph-toggle", "hide faces");
          t.addEventListener("click", () => {
            const hidden = boxes[0].style.display === "none";
            for (const b of boxes) b.style.display = hidden ? "" : "none";
            t.textContent = hidden ? "hide faces" : "show faces";
          });
          body.lastChild.appendChild(t);
        }
        void r;
      }
      if (p.keywords.length) {
        row("Keywords", ...p.keywords.map((k) => chip(k.join(" › "), `label:${quoted(k[k.length - 1])}`)));
      }
      if (p.albums.length) row("Albums", ...p.albums.map((a) => chip(a, `label:${quoted(a)}`)));
      const places = [p.place.location, p.place.city, p.place.state, p.place.country].filter(Boolean);
      if (places.length) row("Place", ...places.map((n) => chip(n, `place:${quoted(n)}`)));
      if (p.h3.length) {
        const city = p.h3.find((c) => c.resolution === 6) || p.h3[0];
        row("Nearby", chip(`photos in this area`, `place:${city.cell}`));
      }
      const exposure = [
        p.exposure.shutter_seconds && (p.exposure.shutter_seconds < 1 ? `1/${Math.round(1 / p.exposure.shutter_seconds)} s` : `${p.exposure.shutter_seconds} s`),
        p.exposure.aperture && `ƒ/${p.exposure.aperture}`,
        p.exposure.iso && `ISO ${p.exposure.iso}`,
        p.exposure.focal_length_mm && `${p.exposure.focal_length_mm} mm`,
      ].filter(Boolean);
      if (p.camera || p.lens) row("Camera", [p.camera, p.lens].filter(Boolean).join(" · "));
      if (exposure.length) row("Exposure", exposure.join(", "));
      if (p.rating) row("Rating", "★".repeat(p.rating));
      if (p.color_label) row("Label", p.color_label);
      if (p.creator) row("By", p.creator);
      if (p.copyright) row("Copyright", p.copyright);
      row("File", [p.folder, p.file_name].filter(Boolean).join(""));

      const notes = [];
      if (!data.previews) {
        notes.push(
          data.original && data.original.drawable
            ? "No Lightroom preview of this photo; showing the original, without your edits."
            : "No Lightroom preview of this photo, and its original cannot be shown here.",
        );
      } else {
        const largest = Math.max(...data.previews.long_edges);
        const long = Math.max(p.width || 0, p.height || 0);
        if (long > largest) notes.push(`Lightroom's preview is ${largest} px across, of a ${long} px original.`);
      }
      if (p.kind === "video" && data.original && data.original.drawable) {
        const video = el("video");
        video.controls = true;
        video.preload = "metadata";
        video.src = `${base}/photo/${encodeURIComponent(uuid)}/original`;
        video.style.maxWidth = "100%";
        frame.replaceChildren(video);
      }
      wrap.replaceChildren(frame, ...notes.map((n) => el("div", "ph-note", n)), body);
      img.addEventListener("error", () => {
        frame.replaceChildren(el("div", "ph-err", "The picture could not be loaded."));
      });
    }

    (uuid === undefined ? sheet() : one()).catch((e) => {
      if (!cancelled) fail(String(e.message || e));
    });
    return () => {
      cancelled = true;
    };
  };
}
