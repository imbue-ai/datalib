// A chip is a link the app can resolve (docs/dev/chips.md). The
// renderers write `[Name](mailto:… "Name <addr>")`; this marks such a
// link as it is rendered, so the decorate pass can find it and ask who
// it is. Plain JavaScript, like chatSections.js, so the render preview
// can run the same plugin over the same markdown without a bundler.
//
// The URI forms mirror `datalib_handle::Handle::to_uri` / `from_uri`
// exactly; the Rust tests and `chip_links.test.ts` run the same cases.

/** The handle kinds this build knows; `datalib_handle::HandleKind`. */
const KINDS = new Set([
  "email",
  "tel",
  "slack",
  "signal_aci",
  "facebook",
  "linkedin",
  "google",
  "github",
  "gitlab",
  "notion",
]);

/** The handle (`email:…`, `tel:…`, `slack:T/U`) a URI names, or null
 *  when it names none this build knows.
 *  @param {string} href
 *  @returns {string | null} */
export function handleFromUri(href) {
  const uri = href.trim();
  const scheme = uri.slice(0, uri.indexOf(":")).toLowerCase();
  if (scheme === "mailto") {
    const addr = uri
      .slice("mailto:".length)
      .split("?")[0]
      .trim()
      .toLowerCase();
    return /^[^\s<>,@]+@[^\s<>,@]*\.[^\s<>,@]*$/.test(addr) ? `email:${addr}` : null;
  }
  if (scheme === "tel") {
    const number = uri.slice("tel:".length).split(";")[0].replace(/[\s().-]/g, "");
    return /^\+\d{7,15}$/.test(number) ? `tel:${number}` : null;
  }
  if (uri.toLowerCase().startsWith("datalib:handle/")) {
    // The spelling for a kind with no standard scheme of its own.
    const rest = uri.slice("datalib:handle/".length);
    const slash = rest.indexOf("/");
    if (slash <= 0) return null;
    const kind = rest.slice(0, slash);
    let value;
    try {
      value = decodeURIComponent(rest.slice(slash + 1));
    } catch {
      return null;
    }
    if (kind === "facebook") return facebookHandle(value);
    if (kind === "google") return googleHandle(value);
    if (kind === "linkedin") return linkedinHandle(value);
    if (kind === "github") return githubHandle(value);
    if (kind === "gitlab") return gitlabHandle(value);
    if (kind === "signal_aci" || kind === "notion") {
      // A Signal account id or a Notion user id: a UUID, spelled
      // lowercase with dashes (`Handle::signal_aci`, `Handle::notion_user`).
      const dashed = dashedUuid(value);
      return dashed ? `${kind}:${dashed}` : null;
    }
    // A kind with a scheme of its own reads back through that scheme.
    return KINDS.has(kind) ? handleFromUri(uriFromHandle(`${kind}:${value}`) ?? "") : null;
  }
  const linkedin = linkedinFromProfileUrl(uri);
  if (linkedin) return linkedin;
  if (uri.toLowerCase().startsWith("slack://user?")) {
    const params = new URLSearchParams(uri.slice("slack://user?".length));
    const team = params.get("team") ?? "";
    const user = params.get("id") ?? "";
    const ok = (s) => /^[A-Za-z0-9_]+$/.test(s);
    return ok(team) && ok(user) ? `slack:${team}/${user}` : null;
  }
  return null;
}

/** The URI a handle is written as in a chip link's href.
 *  @param {string} handle
 *  @returns {string | null} */
export function uriFromHandle(handle) {
  const at = handle.indexOf(":");
  const kind = handle.slice(0, at);
  const value = handle.slice(at + 1);
  if (kind === "email") return `mailto:${value}`;
  if (kind === "tel") return `tel:${value}`;
  if (kind === "slack") {
    const [team, user] = value.split("/");
    return `slack://user?team=${team}&id=${user}`;
  }
  if (kind === "signal_aci") return `datalib:handle/signal_aci/${value}`;
  if (kind === "facebook") return `datalib:handle/facebook/${percentEncode(value)}`;
  if (kind === "linkedin") return `https://www.linkedin.com/in/${percentEncode(value)}`;
  if (kind === "google" || kind === "github" || kind === "gitlab" || kind === "notion") {
    return `datalib:handle/${kind}/${percentEncode(value)}`;
  }
  return null;
}

/** A UUID, with or without dashes in any case, lowercase with dashes.
 *  @param {string} value
 *  @returns {string | null} */
function dashedUuid(value) {
  const hex = value.replace(/-/g, "").toLowerCase();
  if (!/^[0-9a-f]{32}$/.test(hex)) return null;
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

/** `Handle::github`: a login, lowercased; an app's ends in `[bot]`.
 *  @param {string} value
 *  @returns {string | null} */
function githubHandle(value) {
  const login = value.trim().toLowerCase();
  const name = login.endsWith("[bot]") ? login.slice(0, -"[bot]".length) : login;
  return /^[a-z0-9-]{1,39}$/.test(name) ? `github:${login}` : null;
}

/** `Handle::gitlab`: `<host>/<username>`, both lowercased.
 *  @param {string} value
 *  @returns {string | null} */
function gitlabHandle(value) {
  const slash = value.indexOf("/");
  if (slash <= 0) return null;
  const host = value.slice(0, slash).trim().toLowerCase();
  const user = value.slice(slash + 1).trim().toLowerCase();
  return /^[a-z0-9.:-]+$/.test(host) && /^[a-z0-9._-]+$/.test(user)
    ? `gitlab:${host}/${user}`
    : null;
}

/** The member a LinkedIn profile URL names, by its slug decoded and
 *  lowercased (`Handle::linkedin_profile_url`).
 *  @param {string} uri
 *  @returns {string | null} */
function linkedinFromProfileUrl(uri) {
  const m = /^https?:\/\/([^/]+)\/in\/([^?#]*)/i.exec(uri);
  if (!m) return null;
  const host = m[1].toLowerCase();
  if (host !== "linkedin.com" && !host.endsWith(".linkedin.com")) return null;
  let slug;
  try {
    slug = decodeURIComponent(m[2].replace(/\/$/, ""));
  } catch {
    return null;
  }
  return linkedinHandle(slug);
}

/** `Handle::linkedin`: a decoded slug, lowercased.
 *  @param {string} slug
 *  @returns {string | null} */
function linkedinHandle(slug) {
  const s = slug.trim().toLowerCase();
  // eslint-disable-next-line no-control-regex
  return s && !/[\s\u0000-\u001f\u007f-\u009f/?#%]/.test(s) ? `linkedin:${s}` : null;
}

/** A Facebook person, `name/<name>` with its whitespace collapsed or
 *  `deleted/<conversation id>` (`Handle::facebook_name`,
 *  `Handle::facebook_deleted`).
 *  @param {string} value
 *  @returns {string | null} */
function facebookHandle(value) {
  const slash = value.indexOf("/");
  const form = value.slice(0, slash);
  const rest = value.slice(slash + 1);
  if (slash > 0 && form === "name") {
    const name = rest.trim().split(/\s+/).join(" ");
    // eslint-disable-next-line no-control-regex
    return name && !/[\u0000-\u001f\u007f-\u009f]/.test(name) ? `facebook:name/${name}` : null;
  }
  if (slash > 0 && form === "deleted") {
    const id = rest.trim();
    return /^\d+$/.test(id) ? `facebook:deleted/${id}` : null;
  }
  return null;
}

/** A Google account by the name it shows, `name/<name>` with its
 *  whitespace collapsed (`Handle::google_name`).
 *  @param {string} value
 *  @returns {string | null} */
function googleHandle(value) {
  if (!value.startsWith("name/")) return null;
  const name = value.slice("name/".length).trim().split(/\s+/).join(" ");
  // eslint-disable-next-line no-control-regex
  return name && !/[\u0000-\u001f\u007f-\u009f]/.test(name) ? `google:name/${name}` : null;
}

/** Every byte but an unreserved one and `/` as `%XX`, as `to_uri` does.
 *  @param {string} value
 *  @returns {string} */
function percentEncode(value) {
  return encodeURIComponent(value)
    .replace(/%2F/g, "/")
    .replace(/[!'()*]/g, (c) => `%${c.charCodeAt(0).toString(16).toUpperCase()}`);
}

/** The group or step a `datalib:group/<id>` or `datalib:step/<id>` URI
 *  names, or null. Mirrors `datalib_columns::Entity::parse`.
 *  @param {string} href
 *  @returns {{ kind: "group" | "step", id: string } | null} */
export function entityFromUri(href) {
  const m = /^datalib:(group|step)\/(.+)$/.exec(href.trim());
  if (!m) return null;
  return { kind: /** @type {"group" | "step"} */ (m[1]), id: m[2] };
}

/** The URI a group or step is named by; `entityFromUri` reads it back.
 *  @param {"group" | "step"} kind
 *  @param {string} id
 *  @returns {string} */
export function uriFromEntity(kind, id) {
  return `datalib:${kind}/${id}`;
}

/** The contact a `datalib:contact/<id>` URI names, or null. Mirrors
 *  `datalib_contacts::contact_uri`.
 *  @param {string} href
 *  @returns {string | null} */
export function contactFromUri(href) {
  const m = /^datalib:contact\/(.+)$/.exec(href.trim());
  return m ? m[1] : null;
}

/** The markdown-it plugin: an explicit link whose href names a handle
 *  gets `class="chip"` and `data-handle`; one naming a group or a step,
 *  `class="chip"` and `data-entity` (its URI). A link linkify made from a bare
 *  address in running text is left alone — its token says so — so a
 *  signature's address stays an address.
 *  @param {import("markdown-it").MarkdownIt} md */
export function chipLinks(md) {
  const fallback =
    md.renderer.rules.link_open ||
    ((tokens, idx, options, _env, self) => self.renderToken(tokens, idx, options));
  md.renderer.rules.link_open = (tokens, idx, options, env, self) => {
    const token = tokens[idx];
    const href = token.attrGet("href");
    if (token.markup !== "linkify" && typeof href === "string") {
      const handle = handleFromUri(href);
      if (handle) {
        token.attrJoin("class", "chip");
        token.attrSet("data-handle", handle);
      } else if (entityFromUri(href)) {
        token.attrJoin("class", "chip");
        token.attrSet("data-entity", href.trim());
      }
    }
    return fallback(tokens, idx, options, env, self);
  };
}
