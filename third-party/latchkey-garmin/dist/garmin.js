/**
 * Garmin Connect, through the Connect mobile app's own OAuth tokens.
 *
 * Garmin offers individuals no public API. What the Connect phone app (and
 * garth, python-garminconnect and GarminDB after it) uses is
 * connectapi.garmin.com, where every call carries an OAuth2 bearer that
 * expires after about an hour. A fresh bearer is minted by an OAuth1-signed
 * request (HMAC-SHA1 over a nonce, a timestamp and a token secret), and the
 * OAuth1 token behind it lasts about a year.
 *
 * Hence:
 * - Login (`latchkey auth browser garmin`): the user signs into the mobile
 *   app's own SSO page, which ends by handing a service ticket to the app;
 *   the plugin stands in for the app there. It trades that ticket for the
 *   OAuth1 token and the first bearer.
 *   Alternatively, `latchkey auth set-nocurl garmin ~/.garth` imports the
 *   OAuth1 token from a directory garth (or anything else writing its format)
 *   has logged into.
 * - Injection: `Authorization: Bearer <token>` on requests to connectapi.
 * - Refresh: when a request finds the bearer expired, the plugin mints a new
 *   one with the OAuth1 token. No browser is involved.
 *
 * The login flow follows garth's (https://github.com/matin/garth, MIT).
 *
 * All of this rides Garmin's undocumented first-party API, which is why it is
 * a plugin rather than a built-in service. It can stop working whenever Garmin
 * changes the Connect app's sign-in.
 */
import { readFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { join } from 'node:path';
import { buildOAuth1AuthorizationHeader, formEncode, freshNonce, percentEncode, } from './oauth1.js';
/**
 * The Connect mobile app's OAuth1 consumer, as garth publishes it at
 * https://thegarth.s3.amazonaws.com/oauth_consumer.json. Pinned rather than
 * fetched, so that minting a bearer does not depend on that bucket.
 */
const CONNECT_MOBILE_CONSUMER = {
    key: 'fc3e99d2-118c-44b8-8ae3-03370dde24c0',
    secret: 'E08WAR897WEy2knn7aFBrvegVAf0AFdWBBF',
};
const DOMAIN = 'garmin.com';
const SSO_CLIENT_ID = 'GCM_ANDROID_DARK';
// The service the SSO page issues its ticket for. Its host does not resolve:
// it only ever reaches the phone app.
const SSO_SERVICE_URL = `https://mobile.integration.${DOMAIN}/gcm/android`;
const SIGN_IN_URL = `https://sso.${DOMAIN}/mobile/sso/en/sign-in?clientId=${SSO_CLIENT_ID}` +
    `&service=${percentEncode(SSO_SERVICE_URL)}`;
// The bridge the phone app gives the SSO page to report events through, and
// the function the session exposes to stand in for the app.
const THICK_CLIENT_BINDING_NAME = 'latchkeyGarminEvent';
// The sign-in page is the phone app's dark theme: light text on a transparent
// page that the app backs with a dark, phone-sized view. A desktop browser
// needs the same backdrop, or the labels are white on white.
const SIGN_IN_PAGE_STYLE = 'html, body { background: #111111; min-height: 100%; } ' +
    '#portal { max-width: 440px; margin: 48px auto; padding: 0 16px; box-sizing: border-box; }';
// Run on the sign-in page: stands in for the app's bridge and applies the style.
const SIGN_IN_PAGE_SCRIPT = `(() => {
  window.GMNEventHandler = { handleEvent: (event) => window.${THICK_CLIENT_BINDING_NAME}(event) };
  const addStyle = () => {
    const style = document.createElement('style');
    style.textContent = ${JSON.stringify(SIGN_IN_PAGE_STYLE)};
    document.head.append(style);
  };
  if (document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', addStyle);
  } else {
    addStyle();
  }
})();`;
// Best effort: the embed page sets a load-balancer cookie that pins the ticket
// exchange to the backend that knows the ticket.
const SSO_EMBED_URL = `https://sso.${DOMAIN}/portal/sso/embed`;
const CONNECT_API_BASE_URL = `https://connectapi.${DOMAIN}/`;
const PREAUTHORIZED_URL = `${CONNECT_API_BASE_URL}oauth-service/oauth/preauthorized`;
const EXCHANGE_URL = `${CONNECT_API_BASE_URL}oauth-service/oauth/exchange/user/2.0`;
const SOCIAL_PROFILE_URL = `${CONNECT_API_BASE_URL}userprofile-service/socialProfile`;
// What the OAuth endpoints expect to see beside the consumer.
const OAUTH_USER_AGENT = 'com.garmin.android.apps.connectmobile';
// The audience the mobile app names when minting its first bearer after a login.
const LOGIN_AUDIENCE = 'GARMIN_CONNECT_MOBILE_ANDROID_DI';
const GARTH_OAUTH1_FILENAME = 'oauth1_token.json';
// Bearers are re-minted this long before they actually expire, so a request
// that is about to go out does not hit the boundary.
const EXPIRY_MARGIN_MS = 120_000;
const DEFAULT_BEARER_LIFETIME_SECONDS = 3600;
const REQUEST_TIMEOUT_SECONDS = 30;
export class GarminRequestError extends Error {
    constructor(message) {
        super(message);
        this.name = 'GarminRequestError';
    }
}
export class GarminTokenExchangeError extends Error {
    constructor(message) {
        super(message);
        this.name = 'GarminTokenExchangeError';
    }
}
/**
 * The service ticket in a URL the SSO page navigated to, if it is the one
 * that hands the ticket over. Exported for the tests.
 */
export function extractServiceTicketFromUrl(rawUrl) {
    if (!rawUrl.startsWith(`${SSO_SERVICE_URL}?`)) {
        return null;
    }
    return new URL(rawUrl).searchParams.get('ticket');
}
export function createGarmin(sdk) {
    const { ApiCredentialsUsageError, BrowserFollowupServiceSession, FollowupWork, LoginFailedError, NoCurlCredentialsNotSupportedError, Service, fetchAccountFromEndpoint, runCurlCapturedAsync, tryParseJson, z, } = sdk;
    // What `latchkey auth set-nocurl` reports to the user when it cannot use
    // what it was handed.
    class GarminCredentialArgumentsError extends NoCurlCredentialsNotSupportedError {
        constructor(message) {
            super('garmin');
            this.message = message;
            this.name = 'GarminCredentialArgumentsError';
        }
    }
    // ─── Credentials ────────────────────────────────────────────────────────────
    /**
     * Stored Garmin credentials: the year-long OAuth1 token, plus the hour-long
     * bearer last minted with it and when that bearer expires. Credentials
     * imported from garth hold no bearer yet; one is minted on first use.
     */
    const GarminCredentialsSchema = z.object({
        objectType: z.literal('garminOAuth'),
        oauthToken: z.string(),
        oauthTokenSecret: z.string(),
        mfaToken: z.string().optional(),
        accessToken: z.string().optional(),
        accessTokenExpiresAt: z.string().datetime().optional(),
    });
    class GarminCredentials {
        oauthToken;
        oauthTokenSecret;
        mfaToken;
        accessToken;
        accessTokenExpiresAt;
        static objectType = 'garminOAuth';
        objectType = GarminCredentials.objectType;
        constructor(oauthToken, oauthTokenSecret, mfaToken, accessToken, accessTokenExpiresAt) {
            this.oauthToken = oauthToken;
            this.oauthTokenSecret = oauthTokenSecret;
            this.mfaToken = mfaToken;
            this.accessToken = accessToken;
            this.accessTokenExpiresAt = accessTokenExpiresAt;
        }
        injectIntoCurlCall(curlArguments) {
            if (this.accessToken === undefined) {
                throw new ApiCredentialsUsageError('Garmin credentials hold no bearer yet. One is minted from the OAuth1 token ' +
                    'when a request is made.');
            }
            return Promise.resolve(['-H', `Authorization: Bearer ${this.accessToken}`, ...curlArguments]);
        }
        // Expired credentials are what Latchkey asks the service to refresh, so
        // credentials without a bearer count as expired too: minting one is the
        // refresh.
        isExpired() {
            if (this.accessToken === undefined || this.accessTokenExpiresAt === undefined) {
                return true;
            }
            return Date.now() >= new Date(this.accessTokenExpiresAt).getTime() - EXPIRY_MARGIN_MS;
        }
        toJSON() {
            return {
                objectType: this.objectType,
                oauthToken: this.oauthToken,
                oauthTokenSecret: this.oauthTokenSecret,
                mfaToken: this.mfaToken,
                accessToken: this.accessToken,
                accessTokenExpiresAt: this.accessTokenExpiresAt,
            };
        }
        static fromJSON(data) {
            const parsed = GarminCredentialsSchema.parse(data);
            return new GarminCredentials(parsed.oauthToken, parsed.oauthTokenSecret, parsed.mfaToken, parsed.accessToken, parsed.accessTokenExpiresAt);
        }
    }
    async function requestGarmin(curlArguments) {
        const result = await runCurlCapturedAsync(['-sS', '-w', '\n%{http_code}', ...curlArguments], REQUEST_TIMEOUT_SECONDS);
        if (result.returncode !== 0) {
            throw new GarminRequestError(`curl failed talking to Garmin: ${result.stderr.trim()}`);
        }
        const separatorIndex = result.stdout.lastIndexOf('\n');
        return {
            status: Number(result.stdout.slice(separatorIndex + 1).trim()),
            body: separatorIndex === -1 ? '' : result.stdout.slice(0, separatorIndex),
        };
    }
    function excerpt(body) {
        return body.slice(0, 300);
    }
    const ExchangeResponseSchema = z.object({
        access_token: z.string(),
        expires_in: z.number().optional(),
    });
    /**
     * Mint a bearer with the OAuth1 token. `isAfterLogin` is true only for the
     * first bearer after a login, where the mobile app names its audience.
     */
    async function exchangeForBearer(oauthToken, oauthTokenSecret, mfaToken, isAfterLogin) {
        const form = [
            ...(isAfterLogin ? [['audience', LOGIN_AUDIENCE]] : []),
            ...(mfaToken === undefined ? [] : [['mfa_token', mfaToken]]),
        ];
        const authorization = buildOAuth1AuthorizationHeader('POST', EXCHANGE_URL, form, CONNECT_MOBILE_CONSUMER, { key: oauthToken, secret: oauthTokenSecret }, freshNonce());
        const response = await requestGarmin([
            '-X',
            'POST',
            '-H',
            `User-Agent: ${OAUTH_USER_AGENT}`,
            '-H',
            `Authorization: ${authorization}`,
            '-H',
            'Content-Type: application/x-www-form-urlencoded',
            '--data-binary',
            formEncode(form),
            EXCHANGE_URL,
        ]);
        if (response.status !== 200) {
            throw new GarminTokenExchangeError(`Garmin refused to mint a bearer (HTTP ${String(response.status)}: ` +
                `${excerpt(response.body)}). The OAuth1 token has probably expired (they last ` +
                'about a year); log in again with `latchkey auth browser garmin`.');
        }
        const parsed = ExchangeResponseSchema.safeParse(tryParseJson(response.body));
        if (!parsed.success) {
            throw new GarminTokenExchangeError(`Garmin answered the bearer exchange in an unexpected shape: ${excerpt(response.body)}`);
        }
        const lifetimeSeconds = parsed.data.expires_in ?? DEFAULT_BEARER_LIFETIME_SECONDS;
        return new GarminCredentials(oauthToken, oauthTokenSecret, mfaToken, parsed.data.access_token, new Date(Date.now() + lifetimeSeconds * 1000).toISOString());
    }
    /**
     * Trade a service ticket for the OAuth1 token, then mint the first bearer.
     * The ticket exchange is signed by the consumer alone.
     */
    async function logInWithServiceTicket(serviceTicket, cookieHeader) {
        const url = `${PREAUTHORIZED_URL}?ticket=${percentEncode(serviceTicket)}` +
            `&login-url=${percentEncode(SSO_SERVICE_URL)}&accepts-mfa-tokens=true`;
        const authorization = buildOAuth1AuthorizationHeader('GET', url, [], CONNECT_MOBILE_CONSUMER, undefined, freshNonce());
        const response = await requestGarmin([
            '-H',
            `User-Agent: ${OAUTH_USER_AGENT}`,
            '-H',
            `Authorization: ${authorization}`,
            ...(cookieHeader === '' ? [] : ['-H', `Cookie: ${cookieHeader}`]),
            url,
        ]);
        if (response.status !== 200) {
            throw new LoginFailedError(`Garmin refused the service ticket (HTTP ${String(response.status)}: ` +
                `${excerpt(response.body)}).`);
        }
        // The reply is form-encoded, not JSON.
        const fields = new URLSearchParams(response.body.trim());
        const oauthToken = fields.get('oauth_token');
        const oauthTokenSecret = fields.get('oauth_token_secret');
        if (oauthToken === null || oauthTokenSecret === null) {
            throw new LoginFailedError(`Garmin's ticket exchange returned no OAuth1 token: ${excerpt(response.body)}`);
        }
        const mfaToken = fields.get('mfa_token') ?? '';
        return await exchangeForBearer(oauthToken, oauthTokenSecret, mfaToken === '' ? undefined : mfaToken, true);
    }
    // ─── Importing from garth ───────────────────────────────────────────────────
    const GarthOAuth1TokenSchema = z.object({
        oauth_token: z.string(),
        oauth_token_secret: z.string(),
        mfa_token: z.string().nullish(),
        domain: z.string().nullish(),
    });
    function expandHomeDirectory(path) {
        return path === '~' || path.startsWith('~/') ? join(homedir(), path.slice(1)) : path;
    }
    function readGarthCredentials(directory) {
        const path = join(expandHomeDirectory(directory), GARTH_OAUTH1_FILENAME);
        let contents;
        try {
            contents = readFileSync(path, 'utf-8');
        }
        catch {
            throw new GarminCredentialArgumentsError(`Could not read ${path}. Expected a directory garth has saved its tokens to.`);
        }
        const parsed = GarthOAuth1TokenSchema.safeParse(tryParseJson(contents));
        if (!parsed.success) {
            throw new GarminCredentialArgumentsError(`${path} is not a garth OAuth1 token file.`);
        }
        const { oauth_token, oauth_token_secret, mfa_token, domain } = parsed.data;
        if (domain !== undefined && domain !== null && domain !== DOMAIN) {
            throw new GarminCredentialArgumentsError(`The token in ${path} is for ${domain}; only ${DOMAIN} is supported.`);
        }
        return new GarminCredentials(oauth_token, oauth_token_secret, mfa_token ?? undefined);
    }
    // ─── Login ──────────────────────────────────────────────────────────────────
    // What the SSO page tells the phone app once the user is signed in.
    const ThickClientEventSchema = z.object({
        eventType: z.literal('USER_AUTHENTICATED'),
        data: z.object({ serviceTicket: z.string() }),
    });
    /**
     * The user signs into the mobile app's SSO page, which hands the resulting
     * service ticket to the app. Garmin configures this client as a "thick
     * client", so the page does that by calling `window.GMNEventHandler`, the
     * bridge the app provides; the session provides it instead. Should Garmin
     * turn that off, the page navigates to the app's service URL with the
     * ticket, which the session watches for as well.
     */
    class GarminServiceSession extends BrowserFollowupServiceSession {
        followupWork = FollowupWork.RetrieveApiToken;
        serviceTicket = null;
        isListening = false;
        get capturedServiceTicket() {
            return this.serviceTicket;
        }
        onResponse(_response) {
            // The ticket arrives through the bridge or a request, not a response.
        }
        noteThickClientEvent(eventJson) {
            if (typeof eventJson !== 'string') {
                return;
            }
            const parsed = ThickClientEventSchema.safeParse(tryParseJson(eventJson));
            if (parsed.success) {
                this.serviceTicket ??= parsed.data.data.serviceTicket;
            }
        }
        noteRequestUrl(url) {
            this.serviceTicket ??= extractServiceTicketFromUrl(url);
        }
        async whileWaitingForLogin(page) {
            if (this.isListening) {
                return;
            }
            this.isListening = true;
            page.context().on('request', (request) => {
                this.noteRequestUrl(request.url());
            });
            await page.exposeFunction(THICK_CLIENT_BINDING_NAME, (eventJson) => {
                this.noteThickClientEvent(eventJson);
            });
            // The page is already loaded by now, so the script runs on it directly
            // as well as on whatever it loads next.
            await page.addInitScript(SIGN_IN_PAGE_SCRIPT);
            await page.evaluate(SIGN_IN_PAGE_SCRIPT);
        }
        isLoginComplete() {
            return this.serviceTicket !== null;
        }
        async performBrowserFollowup(context) {
            if (this.serviceTicket === null) {
                throw new LoginFailedError('The Garmin login finished without a service ticket.');
            }
            // In thick-client mode the page goes to the embed page by itself.
            const page = context.pages()[0];
            if (page !== undefined && !page.url().startsWith(SSO_EMBED_URL)) {
                await page.goto(SSO_EMBED_URL).catch(() => undefined);
            }
            const cookies = await context.cookies(PREAUTHORIZED_URL);
            const cookieHeader = cookies.map((cookie) => `${cookie.name}=${cookie.value}`).join('; ');
            return await logInWithServiceTicket(this.serviceTicket, cookieHeader);
        }
    }
    // ─── Service ────────────────────────────────────────────────────────────────
    const SocialProfileSchema = z.object({
        userName: z.string().optional(),
        displayName: z.string().optional(),
    });
    class Garmin extends Service {
        name = 'garmin';
        displayName = 'Garmin Connect';
        baseApiUrls = [CONNECT_API_BASE_URL];
        loginUrl = SIGN_IN_URL;
        info = [
            `Garmin Connect, through the Connect mobile app's own API at ${CONNECT_API_BASE_URL} ` +
                '(undocumented; the same API garth and python-garminconnect use).',
            `Known endpoint: GET ${SOCIAL_PROFILE_URL} returns the signed-in user's profile, whose ` +
                'displayName is what per-user paths expect.',
            "This relies on Garmin's first-party mobile client and may break when Garmin changes it.",
        ].join(' ');
        // A cheap read that only a valid bearer gets a 200 for.
        credentialCheckCurlArguments = [SOCIAL_PROFILE_URL];
        setCredentialsExample(serviceName) {
            return `latchkey auth set-nocurl ${serviceName} ~/.garth`;
        }
        getCredentialsNoCurl(noCurlArguments) {
            const [directory, ...unexpected] = noCurlArguments;
            if (directory === undefined || unexpected.length > 0) {
                throw new GarminCredentialArgumentsError(`Expected exactly one argument, a directory holding garth's ${GARTH_OAUTH1_FILENAME}. ` +
                    `Example: ${this.setCredentialsExample(this.name)}`);
            }
            return readGarthCredentials(directory);
        }
        getSession(appNamePrefix) {
            return new GarminServiceSession(this, appNamePrefix);
        }
        async getAccount(apiCredentials) {
            return await fetchAccountFromEndpoint(apiCredentials, [SOCIAL_PROFILE_URL], (body) => {
                const parsed = SocialProfileSchema.safeParse(tryParseJson(body));
                return parsed.success ? (parsed.data.userName ?? parsed.data.displayName ?? null) : null;
            });
        }
        /**
         * Mint a fresh bearer with the stored OAuth1 token. Latchkey calls this
         * only when a request finds the bearer expired, so idle credentials cost
         * nothing.
         */
        async refreshCredentials(apiCredentials) {
            if (!(apiCredentials instanceof GarminCredentials)) {
                return null;
            }
            return await exchangeForBearer(apiCredentials.oauthToken, apiCredentials.oauthTokenSecret, apiCredentials.mfaToken, false);
        }
    }
    return { Garmin, GarminCredentials, GarminServiceSession, logInWithServiceTicket };
}
