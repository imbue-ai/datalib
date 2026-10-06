// The ways a site misbehaves, as fake-site handlers a spec installs
// with `internet.override(host, fault)`. Each is the shape the real
// service sends, as far as a probe can tell.

/// The site refuses the credential the way it does in life: Slack
/// answers 200 with `ok: false`, the others 401.
export const rejected = (req) =>
  req.host === "slack.com"
    ? { json: { ok: false, error: "invalid_auth" } }
    : { status: 401, json: { error: { type: "authentication_error" } } };

/// Cloudflare's bot wall: a 403 with its challenge page and marker.
export const cloudflareChallenge = () => ({
  status: 403,
  headers: { "cf-mitigated": "challenge", server: "cloudflare" },
  html: "<!DOCTYPE html><title>Just a moment...</title>",
});

/// Slow down, and come back in a second.
export const rateLimited = () => ({
  status: 429,
  headers: { "retry-after": "1" },
  json: { ok: false, error: "ratelimited" },
});

export const serverError = () => ({ status: 503, text: "upstream unavailable" });

/// A 200 whose body is not what the API returns.
export const malformed = () => ({ html: "<html><body>Sign in to continue</body></html>" });

export const FAULTS = { rejected, cloudflareChallenge, rateLimited, serverError, malformed };
