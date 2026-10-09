// The third-party sites a sign-in reaches, faked in made-up TNG data:
// a login page that hands out a credential, and the few read-only
// endpoints each provider's probe calls. Only what the probes read is
// here; the shapes are the providers' own (see each provider's
// `probe.rs`). A handler gets the request and returns, or resolves to,
// `{status?, json? | html? | text?, headers?}`.

/// The credential each site accepts. Specs paste these, or expect a
/// browser login to capture them.
export const TNG = {
  slackToken: "xoxp-tng-picard",
  claudeSessionKey: "sk-ant-tng-picard",
  chatgptSessionCookie: "tng-chatgpt-session",
  chatgptAccessToken: "tng-chatgpt-access",
  /// The year-long OAuth1 token a garth folder holds, and the bearer the
  /// exchange mints from it.
  garminOauthToken: "tng-garmin-oauth1",
  garminOauthSecret: "tng-garmin-secret",
  garminBearer: "tng-garmin-bearer",
};

/// Who can sign in to claude.ai and chatgpt.com, each with their own
/// credentials. A login page that sees no session signs in whoever
/// `internet.signInAs` last named, Picard to start.
export const CREW = {
  picard: {
    id: "picard",
    email: "picard@enterprise.test",
    name: "Jean-Luc Picard",
    claudeSessionKey: TNG.claudeSessionKey,
    chatgptSessionCookie: TNG.chatgptSessionCookie,
    chatgptAccessToken: TNG.chatgptAccessToken,
  },
  riker: {
    id: "riker",
    email: "riker@enterprise.test",
    name: "William Riker",
    claudeSessionKey: "sk-ant-tng-riker",
    chatgptSessionCookie: "tng-chatgpt-session-riker",
    chatgptAccessToken: "tng-chatgpt-access-riker",
  },
};
const crewBy = (field, value) => Object.values(CREW).find((m) => value && m[field] === value);
const CHATGPT_SESSION = "__Secure-next-auth.session-token";

const cookies = (req) =>
  Object.fromEntries(
    String(req.headers.cookie ?? "")
      .split(";")
      .map((c) => c.trim().split("="))
      .filter(([k]) => k),
  );
const bearer = (req) => String(req.headers.authorization ?? "").replace(/^Bearer /, "");

function slack(req) {
  // Slack answers a bad token with HTTP 200 and `ok: false`.
  if (bearer(req) !== TNG.slackToken) return { json: { ok: false, error: "invalid_auth" } };
  const page = { ok: true, response_metadata: { next_cursor: "" } };
  switch (req.path) {
    case "/api/auth.test":
      return {
        json: {
          ok: true,
          url: "https://enterprise.slack.com/",
          team: "Enterprise",
          user: "picard",
          team_id: "T_ENTERPRISE",
          user_id: "U_PICARD",
        },
      };
    case "/api/users.list":
      return {
        json: {
          ...page,
          members: [
            { id: "U_PICARD", name: "picard", real_name: "Jean-Luc Picard" },
            { id: "U_RIKER", name: "riker", real_name: "William Riker" },
          ],
        },
      };
    case "/api/conversations.list": {
      const q = new URLSearchParams(req.query);
      if (q.get("types") === "im,mpim") {
        return { json: { ...page, channels: [{ id: "D_RIKER", is_im: true, user: "U_RIKER" }] } };
      }
      // Channels come in two pages, so a picker has progress to show.
      if (q.get("cursor") === "page-2") {
        return {
          json: {
            ...page,
            channels: [{ id: "C_TEN", name: "ten-forward", is_channel: true, num_members: 40 }],
          },
        };
      }
      return {
        json: {
          ok: true,
          response_metadata: { next_cursor: "page-2" },
          channels: [
            { id: "C_BRIDGE", name: "bridge", is_channel: true, is_member: true, num_members: 12 },
            { id: "C_ENG", name: "engineering", is_channel: true, is_private: true, is_member: true, num_members: 4 },
          ],
        },
      };
    }
    default:
      return { status: 404, json: { ok: false, error: "unknown_method" } };
  }
}

function claude(req) {
  const member = crewBy("claudeSessionKey", cookies(req).sessionKey);
  if (req.path === "/login") {
    // As the real one: a signed-in browser is sent on, with no new cookie.
    if (member) return { status: 302, headers: { location: "/new" } };
    const signsIn = CREW[req.signsInAs];
    return {
      html: "<h1>Signed in to claude.ai (fake)</h1>",
      headers: { "set-cookie": `sessionKey=${signsIn.claudeSessionKey}; Path=/; Secure; HttpOnly` },
    };
  }
  if (req.path === "/new" && member) return { html: "<h1>New chat (fake)</h1>" };
  if (!member) {
    return {
      status: 401,
      json: { type: "error", error: { type: "authentication_error", message: "Invalid authorization" } },
    };
  }
  switch (req.path) {
    case "/api/account":
      return { json: { uuid: `acct-${member.id}`, email_address: member.email, full_name: member.name } };
    case "/api/organizations":
      return { json: [{ uuid: "org-enterprise", name: "Enterprise" }] };
    case "/api/organizations/org-enterprise/chat_conversations":
      return {
        json: [
          { uuid: "conv-warp-core", name: "Warp core diagnostics", updated_at: "2026-09-30T12:00:00Z" },
          { uuid: "conv-tea", name: "Tea, Earl Grey, hot", updated_at: "2026-09-29T08:00:00Z" },
        ],
      };
    default:
      return { status: 404, json: { type: "error", error: { type: "not_found_error" } } };
  }
}

function chatgpt(req) {
  const signedIn = crewBy("chatgptSessionCookie", cookies(req)[CHATGPT_SESSION]);
  if (req.path === "/auth/login") {
    // As the real one: a signed-in browser is sent on, with no new cookie.
    if (signedIn) return { status: 302, headers: { location: "/" } };
    const signsIn = CREW[req.signsInAs];
    return {
      html: "<h1>Signed in to ChatGPT (fake)</h1>",
      headers: {
        "set-cookie": `${CHATGPT_SESSION}=${signsIn.chatgptSessionCookie}; Path=/; Secure; HttpOnly`,
      },
    };
  }
  if (req.path === "/" && signedIn) return { html: "<h1>ChatGPT (fake)</h1>" };
  if (req.path === "/api/auth/session") {
    // What chatgpt.com's own page fetches: the bearer token, but only
    // for a signed-in browser.
    return {
      json: signedIn ? { user: { id: `user-${signedIn.id}` }, accessToken: signedIn.chatgptAccessToken } : {},
    };
  }
  const member = crewBy("chatgptAccessToken", bearer(req));
  if (!member) {
    return { status: 401, json: { detail: "Unauthorized" } };
  }
  switch (req.path) {
    case "/backend-api/me":
      return { json: { id: `user-${member.id}`, email: member.email, name: member.name } };
    case "/backend-api/conversations":
      return {
        json: {
          items: [{ id: "c-holodeck", title: "Holodeck safety protocols", update_time: 1790000000 }],
          total: 1,
          limit: 28,
          offset: 0,
        },
      };
    default:
      return { status: 404, json: { detail: "Not Found" } };
  }
}

function garmin(req) {
  if (req.method === "POST" && req.path === "/oauth-service/oauth/exchange/user/2.0") {
    // The plugin signs this with the stored OAuth1 token; the fake
    // checks only that it is the one the garth folder held.
    const signed = /oauth_token="([^"]*)"/.exec(String(req.headers.authorization ?? ""));
    if (signed?.[1] !== TNG.garminOauthToken) return { status: 401, text: "" };
    return { json: { access_token: TNG.garminBearer, expires_in: 3600 } };
  }
  if (bearer(req) !== TNG.garminBearer) return { status: 401, text: "" };
  switch (req.path) {
    case "/userprofile-service/socialProfile":
      return {
        json: {
          profileId: 1701,
          displayName: "picard-1701",
          fullName: "Jean-Luc Picard",
          userName: "picard@enterprise.test",
        },
      };
    default:
      return { status: 404, text: "" };
  }
}

export const FAKE_SITES = {
  "slack.com": slack,
  "claude.ai": claude,
  "chatgpt.com": chatgpt,
  "connectapi.garmin.com": garmin,
};
