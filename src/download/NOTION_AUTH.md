# Notion (unofficial web API) — latchkey setup

`download/notion_web.py` talks to Notion's internal `https://www.notion.so/api/v3/*`
endpoints (the same ones the web client uses), not the public `api.notion.com`
integration API. That route doesn't require admin/integration approval; it
authenticates with your normal browser session cookies.

We register it as a **separate** latchkey service (`notion_unofficial`) so it
doesn't collide with the existing `notion` service, which is pointed at the
official `api.notion.com` (kept around for when an integration token is
available).

## One-time setup

```bash
latchkey services register notion_unofficial --base-api-url="https://www.notion.so/"
```

## Auth (re-run whenever the session expires)

1. Open <https://www.notion.so> in a logged-in browser tab.
2. DevTools → **Console** → paste this snippet:

   ```js
   (() => {
     const visible = Object.fromEntries(
       document.cookie.split('; ').filter(Boolean).map(c => {
         const i = c.indexOf('=');
         return [c.slice(0, i), c.slice(i + 1)];
       })
     );
     const ua = navigator.userAgent;
     const cookieStr =
       Object.entries(visible).map(([k, v]) => `${k}=${v}`).join('; ') +
       '; token_v2=<PASTE_TOKEN_V2_HERE>';
     const cmd =
       `latchkey auth set notion_unofficial \\\n` +
       `  -H 'Cookie: ${cookieStr}' \\\n` +
       `  -H 'User-Agent: ${ua}' \\\n` +
       `  -H 'Notion-Client-Version: <PASTE_FROM_NETWORK_TAB>'`;
     console.log(cmd);
   })();
   ```

3. Copy `token_v2` from DevTools → **Application** → **Cookies** →
   `https://www.notion.so` (the row marked HttpOnly + Secure). JS cannot read
   HttpOnly cookies, so the snippet leaves a placeholder for it.
4. Get `Notion-Client-Version` from DevTools → **Network** tab → filter `api/v3`
   → click any request → **Request Headers** → copy the `notion-client-version`
   value (e.g. `23.13.0.xxx`). The web app rejects some endpoints without it.
5. Paste both values into the printed command, then run it in a terminal.

## Smoke test

```bash
latchkey curl https://www.notion.so/api/v3/getSpaces \
  -X POST -H 'Content-Type: application/json' --data '{}'
```

Expected: a large JSON blob starting with a workspace UUID and a `recordMap`.
If you get `UnauthorizedError`, the cookies are stale — repeat the auth step.
If you get `No service matches URL`, the `services register` step didn't run.

## Cloudflare-protected endpoints (`LATCHKEY_CURL`)

`loadCachedPageChunkV2` and `getNotificationLog` sit behind Cloudflare and
bounce off-fingerprint clients with `HTTP 403`. The downloader handles this
automatically: on startup it points `LATCHKEY_CURL` at
`src/download/latchkey_curl_shim.py`, a thin wrapper around `curl_cffi`
that emits a Chrome TLS fingerprint. Latchkey then runs that wrapper as its
curl backend, so every request — cheap or Cloudflare-protected — goes out
impersonating Chrome.

If you want to override this (e.g. point at a real `curl-impersonate-chrome`
binary you've installed via Homebrew), set `LATCHKEY_CURL` yourself before
invoking the downloader.

## Notes / gotchas

- `token_v2` is the credential that matters. It's HttpOnly so it never appears
  in `document.cookie`; copy it from the DevTools cookie inspector.
- Lifetime is long (~1 year) but rotates on logout. Re-auth using the same
  steps when requests start returning `UnauthorizedError`.
- `Notion-Client-Version` is a build constant — it drifts as Notion ships
  updates. If responses start failing oddly, re-grab it from the Network tab.
- Attachments (images, file blocks) live on signed S3 URLs at
  `prod-files-secure.s3.us-west-2.amazonaws.com` and `file.notion.so`. Those
  hosts are not covered by the `notion_unofficial` service URL pattern; when
  we add media support we'll harvest the `file_token` cookie + replay headers
  with plain `curl`, same pattern as `slack_web.py`'s media path.
