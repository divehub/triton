// Vercel Function (Node.js runtime, Web-standard handlers, no dependencies): the NGC firmware proxy.
//
//   GET     /api/firmware?url=<encoded upstream URL>   fetch one S-record file on demand and return it
//   OPTIONS /api/firmware                              CORS preflight
//
// Why it exists: the two original SREC files live on api.multi3s.com and in the Wayback Machine, and neither sends
// CORS headers, so a web page cannot read them directly. The emulator page asks this function instead. The function
// stores nothing and caches nothing (`Cache-Control: private, no-store`): every request fetches the file again, so
// the deployment never holds or redistributes the firmware.
//
// What guards it (details and limits: ../README.md):
//   * the upstream allowlist below is the only thing that decides what is fetched (SSRF / open-proxy guard);
//   * redirects are followed manually, at most 3, and each Location must match the allowlist again;
//   * 20 s deadline, 4 MiB cap (Content-Length first, then while reading) and the body must start with S0;
//   * only the page origins listed in ALLOWED_ORIGINS get CORS headers. This discourages hotlinking by other sites'
//     pages; it is not authentication (a non-browser client can send any Origin), the allowlist is the real guard.
//
// The page-side copy of the URL allowlist is web/firmware-url.js; deploy/test-proxy.mjs checks that
// the two agree. Nothing in this file logs request details.

export const MAX_BYTES = 4 * 1024 * 1024; // Vercel's response limit is about 4.5 MB
export const MAX_REDIRECTS = 3;
export const UPSTREAM_TIMEOUT_MS = 20_000;
export const USER_AGENT = 'ngc-firmware-proxy/1.0 (+https://triton.divehub.ai; on-demand single-file fetch, nothing stored)';

// ---- upstream allowlist ------------------------------------------------------------------------------------

const NAME = '[A-Za-z0-9_-]{1,64}';
const ORIGIN_RE = new RegExp(`^https://api\\.multi3s\\.com/static/(${NAME})\\.srec$`);
const ARCHIVE_RE = new RegExp(`^https://web\\.archive\\.org/web/(\\d{14})(id_)?/https://api\\.multi3s\\.com/static/(${NAME})\\.srec$`);
const MAX_URL_LENGTH = 256;
const HOSTS = new Set(['api.multi3s.com', 'web.archive.org']);

/** A short, fixed explanation of why `raw` is not an allowed upstream address (never echoes the input). */
function explainRejection(raw) {
  if (typeof raw !== 'string' || raw === '') return ['empty', 'The url parameter is empty.'];
  if (raw.length > MAX_URL_LENGTH) return ['too-long', `The address is longer than ${MAX_URL_LENGTH} characters.`];
  if (/[^\x21-\x7e]/.test(raw)) return ['characters', 'The address contains spaces, control or non-ASCII characters.'];
  if (raw.includes('%')) return ['encoded', 'Percent-encoded characters are not accepted in the address.'];
  if (raw.includes('\\')) return ['characters', 'Backslashes are not accepted in the address.'];
  const parts = /^([A-Za-z][A-Za-z0-9+.-]*):\/\/([^/?#]*)([^]*)$/.exec(raw);
  if (!parts) return ['not-url', 'The value is not an absolute https:// address.'];
  const [, scheme, authority, rest] = parts;
  if (scheme !== 'https') return ['scheme', 'Only https:// addresses are accepted.'];
  if (authority.includes('@')) return ['userinfo', 'User names and passwords in the address are not accepted.'];
  const host = authority.replace(/:\d*$/, '');
  if (host !== authority) return ['port', 'Ports are not accepted in the address.'];
  if (!HOSTS.has(host)) return ['host', 'Only api.multi3s.com and web.archive.org addresses are accepted.'];
  if (rest.includes('?')) return ['query', 'Query strings are not accepted in the address.'];
  if (rest.includes('#')) return ['fragment', 'Fragments are not accepted in the address.'];
  if (host === 'api.multi3s.com') return ['path', 'The path must be /static/NAME.srec, where NAME is 1 to 64 letters, digits, "_" or "-".'];
  return ['archive-form', 'A Wayback Machine address must be https://web.archive.org/web/<14-digit timestamp>[id_]/https://api.multi3s.com/static/NAME.srec.'];
}

/**
 * Checks an upstream address against the allowlist. Accepted, character for character (no URL parsing, no decoding):
 *   https://api.multi3s.com/static/<name>.srec
 *   https://web.archive.org/web/<14 digits>[id_]/https://api.multi3s.com/static/<name>.srec
 * `<name>` is 1 to 64 of [A-Za-z0-9_-]. The Wayback form is normalized to the raw `<timestamp>id_` form.
 * Returns {ok: true, kind: 'origin' | 'archive', name, timestamp?, url} or {ok: false, reason, message}.
 */
export function parseUpstream(raw) {
  if (typeof raw === 'string' && raw.length <= MAX_URL_LENGTH) {
    let match = ORIGIN_RE.exec(raw);
    if (match) return { ok: true, kind: 'origin', name: match[1], url: raw };
    match = ARCHIVE_RE.exec(raw);
    if (match) {
      const [, timestamp, , name] = match;
      return { ok: true, kind: 'archive', name, timestamp, url: `https://web.archive.org/web/${timestamp}id_/https://api.multi3s.com/static/${name}.srec` };
    }
  }
  const [reason, message] = explainRejection(raw);
  return { ok: false, reason, message };
}

// ---- who may call it ---------------------------------------------------------------------------------------

const PRODUCTION_ORIGIN = 'https://triton.divehub.ai';
const LOOPBACK_ORIGIN_RE = /^http:\/\/(?:localhost|127\.0\.0\.1)(?::([1-9]\d{0,4}))?$/;

/** True for the production site and for http://localhost[:port] / http://127.0.0.1[:port] (any port). */
export function isAllowedOrigin(origin) {
  if (origin === PRODUCTION_ORIGIN) return true;
  const match = LOOPBACK_ORIGIN_RE.exec(origin);
  return !!match && (match[1] === undefined || Number(match[1]) <= 65535);
}

/**
 * Who is calling: a listed browser origin (`origin` is echoed in the CORS headers), the deployed site itself (a
 * same-origin GET may omit Origin; the browser then says `Sec-Fetch-Site: same-origin`), or nobody we serve.
 */
function identifyCaller(request) {
  const origin = request.headers.get('origin');
  if (origin !== null) {
    return isAllowedOrigin(origin) ? { ok: true, origin } : { ok: false, code: 'origin_not_allowed', message: 'This page origin may not use the firmware proxy.' };
  }
  if (request.headers.get('sec-fetch-site') === 'same-origin') return { ok: true, origin: null };
  return { ok: false, code: 'origin_required', message: 'The firmware proxy only answers the emulator page (no Origin header, and the request is not marked same-origin).' };
}

// ---- responses ---------------------------------------------------------------------------------------------

function baseHeaders(origin) {
  const headers = new Headers({
    'Cache-Control': 'private, no-store',
    'X-Content-Type-Options': 'nosniff',
    'Referrer-Policy': 'no-referrer',
    Vary: 'Origin',
  });
  if (origin) {
    headers.set('Access-Control-Allow-Origin', origin);
    headers.set('Access-Control-Expose-Headers', 'X-Firmware-Source');
  }
  return headers;
}

function failure(status, code, message, origin = null, extra = {}) {
  const headers = baseHeaders(origin);
  headers.set('Content-Type', 'application/json; charset=utf-8');
  return new Response(JSON.stringify({ error: code, message, ...extra }), { status, headers });
}

function preflight(origin) {
  const headers = baseHeaders(origin);
  headers.set('Access-Control-Allow-Methods', 'GET');
  headers.set('Access-Control-Max-Age', '600');
  return new Response(null, { status: 204, headers });
}

// ---- the upstream fetch ------------------------------------------------------------------------------------

const REDIRECT_STATUSES = new Set([301, 302, 303, 307, 308]);

function isTimeout(error) {
  return !!error && (error.name === 'TimeoutError' || (error.name === 'AbortError' && error.cause && error.cause.name === 'TimeoutError'));
}

/** Reads at most MAX_BYTES; resolves to {bytes} or {failure: [status, code, message, extra]}. */
async function readBody(response) {
  const declared = response.headers.get('content-length');
  if (declared !== null && /^\d+$/.test(declared) && Number(declared) > MAX_BYTES) {
    await response.body?.cancel().catch(() => {});
    return { failure: [502, 'upstream_too_large', `The upstream file is larger than ${MAX_BYTES} bytes (Content-Length).`] };
  }
  if (!response.body) return { failure: [502, 'not_srec', 'The upstream answered with an empty body.'] };
  const reader = response.body.getReader();
  const chunks = [];
  let total = 0;
  let checked = false;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    total += value.byteLength;
    if (total > MAX_BYTES) {
      await reader.cancel().catch(() => {});
      return { failure: [502, 'upstream_too_large', `The upstream file is larger than ${MAX_BYTES} bytes.`] };
    }
    chunks.push(value);
    if (!checked && total >= 2) {
      checked = true;
      const head = chunks.length === 1 ? chunks[0] : concat(chunks, total);
      if (head[0] !== 0x53 || head[1] !== 0x30) {
        await reader.cancel().catch(() => {});
        return { failure: [502, 'not_srec', 'The upstream file is not an S-record file (it does not start with S0).'] };
      }
    }
  }
  if (!checked) return { failure: [502, 'not_srec', 'The upstream file is not an S-record file (it does not start with S0).'] };
  return { bytes: concat(chunks, total) };
}

function concat(chunks, total) {
  const bytes = new Uint8Array(total);
  let at = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, at);
    at += chunk.byteLength;
  }
  return bytes;
}

/**
 * The request handler. `options.fetch` and `options.timeoutMs` exist for the tests (and the dev server uses the
 * defaults); `GET` and `OPTIONS` below are the exports Vercel calls.
 */
export async function handle(request, options = {}) {
  try {
    return await route(request, options);
  } catch (_) {
    return failure(500, 'internal_error', 'The firmware proxy failed unexpectedly.'); // nothing is logged: no request details
  }
}

async function route(request, options) {
  const fetchImpl = options.fetch || globalThis.fetch;
  const timeoutMs = options.timeoutMs ?? UPSTREAM_TIMEOUT_MS;
  const method = request.method.toUpperCase();
  if (method !== 'GET' && method !== 'OPTIONS') {
    const headers = baseHeaders(null);
    headers.set('Allow', 'GET, OPTIONS');
    headers.set('Content-Type', 'application/json; charset=utf-8');
    return new Response(JSON.stringify({ error: 'method_not_allowed', message: 'Use GET.' }), { status: 405, headers });
  }

  const caller = identifyCaller(request);
  if (!caller.ok) return failure(403, caller.code, caller.message);
  if (method === 'OPTIONS') return preflight(caller.origin);

  const values = new URL(request.url).searchParams.getAll('url');
  if (values.length !== 1) return failure(400, 'bad_request', 'Exactly one url parameter is required.', caller.origin);
  const upstream = parseUpstream(values[0]);
  if (!upstream.ok) return failure(400, 'upstream_not_allowed', upstream.message, caller.origin, { reason: upstream.reason });

  // One deadline for the whole exchange: every redirect hop and the body. (An explicit timer rather than
  // AbortSignal.timeout(): that one is unref'd and would not keep a quiet process alive; this one is cleared below.)
  const controller = new AbortController();
  const signal = controller.signal;
  const timer = setTimeout(() => controller.abort(new DOMException('The upstream did not answer in time.', 'TimeoutError')), timeoutMs);
  let current = upstream.url;
  try {
    for (let hops = 0; ; hops += 1) {
      const response = await fetchImpl(current, {
        method: 'GET',
        redirect: 'manual',
        credentials: 'omit',
        headers: { 'User-Agent': USER_AGENT, Accept: 'text/plain, */*;q=0.1', 'Accept-Encoding': 'identity' },
        signal,
      });
      if (REDIRECT_STATUSES.has(response.status)) {
        await response.body?.cancel().catch(() => {});
        if (hops >= MAX_REDIRECTS) return failure(502, 'too_many_redirects', `The upstream redirected more than ${MAX_REDIRECTS} times.`, caller.origin);
        const location = response.headers.get('location');
        let next = null;
        try {
          next = location ? parseUpstream(new URL(location, current).href) : null;
        } catch (_) { /* an unparsable Location is refused below */ }
        if (!next || !next.ok) return failure(502, 'redirect_not_allowed', 'The upstream redirected somewhere that is not an allowed address.', caller.origin);
        current = next.url;
        continue;
      }
      if (response.status !== 200) {
        await response.body?.cancel().catch(() => {});
        return failure(502, 'upstream_status', `The upstream answered HTTP ${response.status}.`, caller.origin, { upstreamStatus: response.status });
      }
      const body = await readBody(response);
      if (body.failure) return failure(body.failure[0], body.failure[1], body.failure[2], caller.origin);
      const headers = baseHeaders(caller.origin);
      headers.set('Content-Type', 'text/plain; charset=us-ascii');
      headers.set('Content-Length', String(body.bytes.byteLength));
      headers.set('X-Firmware-Source', current);
      return new Response(body.bytes, { status: 200, headers });
    }
  } catch (error) {
    if (isTimeout(error) || signal.aborted) return failure(504, 'upstream_timeout', `The upstream did not answer within ${Math.round(timeoutMs / 1000)} seconds.`, caller.origin);
    return failure(502, 'upstream_unreachable', 'The upstream could not be reached.', caller.origin);
  } finally {
    clearTimeout(timer);
  }
}

export function GET(request) {
  return handle(request);
}

export function OPTIONS(request) {
  return handle(request);
}
