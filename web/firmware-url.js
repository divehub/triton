// Loading the original S-record files from URLs: the address allowlist, the choice of the firmware proxy and the
// fetch with its error classification (the entry screen's "Load from URLs").
//
// The sources (api.multi3s.com and the Wayback Machine) send no CORS headers, so the page cannot read them directly;
// it asks a small proxy (deploy/api/firmware.mjs, a Vercel Function hosted on its own address, separate from this
// static site) that fetches one file on demand and stores nothing. The proxy address is a build-time setting
// (config.js, written by deploy/build_site.py from the FIRMWARE_PROXY_URL repository variable); without it the page
// has no proxy and "Load from URLs" is unavailable. It never falls back to a same-origin /api/firmware.
// The allowlist here is the page-side copy of the proxy's: the proxy decides what is really fetched, this copy only
// gives early, specific feedback and shows the exact address that will be requested. deploy/test-proxy.mjs checks
// that both agree.
//
// Pure ES module (no DOM, no worker globals): the Node tests import it.

export const PROXY_PATH = '/api/firmware';
export const MAX_FETCH_BYTES = 4 * 1024 * 1024;
export const FETCH_TIMEOUT_MS = 45_000;
export const MAX_URL_LENGTH = 256;

const NAME = '[A-Za-z0-9_-]{1,64}';
/** A regular expression source that matches `literal` in any letter case (for schemes and host names only). */
const anyCase = (literal) => literal.replace(/[a-z.]/gi, (c) => (c === '.' ? '\\.' : `[${c.toLowerCase()}${c.toUpperCase()}]`));
const HTTPS = anyCase('https');
const API = anyCase('api.multi3s.com');
const ARCHIVE_HOST = anyCase('web.archive.org');
// Only the scheme and the hosts are matched in any case; paths, names, "id_" and ".srec" are case-sensitive upstream
// and never changed. The canonical address is rebuilt from the captured parts, so what is requested is exactly what
// the proxy accepts.
const ORIGIN_RE = new RegExp(`^${HTTPS}://${API}/static/(${NAME})\\.srec$`);
const ARCHIVE_RE = new RegExp(`^${HTTPS}://${ARCHIVE_HOST}/web/(\\d{14})(id_)?/${HTTPS}://${API}/static/(${NAME})\\.srec$`);
const HOSTS = new Set(['api.multi3s.com', 'web.archive.org']);

/** Why `text` is not an accepted address, as [reason, message]. */
function explainRejection(text) {
  if (text === '') return ['empty', 'Enter an address.'];
  if (text.length > MAX_URL_LENGTH) return ['too-long', `The address is longer than ${MAX_URL_LENGTH} characters.`];
  if (/[^\x21-\x7e]/.test(text)) return ['characters', 'The address contains spaces, control or non-ASCII characters.'];
  if (text.includes('%')) return ['encoded', 'Percent-encoded characters are not accepted.'];
  if (text.includes('\\')) return ['characters', 'Backslashes are not accepted.'];
  const parts = /^([A-Za-z][A-Za-z0-9+.-]*):\/\/([^/?#]*)([^]*)$/.exec(text);
  if (!parts) return ['not-url', 'This is not an absolute https:// address.'];
  const [, scheme, authority, rest] = parts;
  if (scheme.toLowerCase() !== 'https') return ['scheme', 'Only https:// addresses are accepted (this one is not https).'];
  if (authority.includes('@')) return ['userinfo', 'User names and passwords in an address are not accepted.'];
  const host = authority.replace(/:\d*$/, '').toLowerCase();
  if (host !== authority.toLowerCase()) return ['port', 'Ports are not accepted in the address.'];
  if (!HOSTS.has(host)) return ['host', 'Only api.multi3s.com and web.archive.org addresses are accepted (not other hosts or IP addresses).'];
  if (rest.includes('?')) return ['query', 'Query strings are not accepted.'];
  if (rest.includes('#')) return ['fragment', 'Fragments (#…) are not accepted.'];
  if (host === 'api.multi3s.com') return ['path', 'The path must be /static/NAME.srec, where NAME is 1 to 64 letters, digits, "_" or "-".'];
  return ['archive-form', 'A Wayback Machine address must look like https://web.archive.org/web/TIMESTAMP/https://api.multi3s.com/static/NAME.srec (14-digit timestamp).'];
}

/**
 * Validates one address and returns the exact address the proxy will be asked for.
 *   https://api.multi3s.com/static/<name>.srec
 *   https://web.archive.org/web/<14 digits>[id_]/https://api.multi3s.com/static/<name>.srec   (-> `<14 digits>id_`)
 * Surrounding white space is ignored. {ok: true, kind, name, file, url, changed} or {ok: false, reason, message}.
 */
export function normalizeFirmwareUrl(input) {
  const text = String(input ?? '').trim();
  if (text.length <= MAX_URL_LENGTH) {
    let match = ORIGIN_RE.exec(text);
    if (match) {
      const url = `https://api.multi3s.com/static/${match[1]}.srec`;
      return { ok: true, kind: 'origin', name: match[1], file: `${match[1]}.srec`, url, changed: url !== text };
    }
    match = ARCHIVE_RE.exec(text);
    if (match) {
      const [, timestamp, , name] = match;
      const url = `https://web.archive.org/web/${timestamp}id_/https://api.multi3s.com/static/${name}.srec`;
      return { ok: true, kind: 'archive', name, timestamp, file: `${name}.srec`, url, changed: url !== text };
    }
  }
  const [reason, message] = explainRejection(text);
  return { ok: false, reason, message };
}

// ---- which proxy ---------------------------------------------------------------------------------------------

const LOOPBACK_HOSTS = new Set(['127.0.0.1', 'localhost', '[::1]', '::1']);

/** A dev proxy address accepted for `?firmware-proxy=`: http://127.0.0.1:<port>/api/firmware (or localhost), nothing else. */
export function loopbackProxyUrl(value) {
  let url;
  try {
    url = new URL(String(value));
  } catch (_) {
    return null;
  }
  const ok = url.protocol === 'http:' && (url.hostname === '127.0.0.1' || url.hostname === 'localhost')
    && !url.username && !url.password && url.pathname === PROXY_PATH && !url.search && !url.hash;
  return ok ? `${url.origin}${PROXY_PATH}` : null;
}

/**
 * The firmware proxy address a site was built with (config.js): exactly `https://<host>[:port]/api/firmware` in its
 * canonical form (lower-case host, no default port, no user name, query or fragment). Returns it, or null.
 */
export function configuredProxyUrl(value) {
  if (typeof value !== 'string' || value === '' || value.length > MAX_URL_LENGTH) return null;
  let url;
  try {
    url = new URL(value);
  } catch (_) {
    return null;
  }
  const ok = url.protocol === 'https:' && url.hostname !== '' && !url.username && !url.password
    && url.pathname === PROXY_PATH && !url.search && !url.hash;
  return ok && value === `${url.origin}${PROXY_PATH}` ? value : null;
}

/**
 * Which proxy this page uses (see deploy/README.md):
 *   * a page served from localhost / 127.0.0.1 (serve.py) may name a loopback dev proxy with
 *     `?firmware-proxy=http://127.0.0.1:<port>/api/firmware` (honoured only on a loopback page, and only for
 *     loopback addresses);
 *   * otherwise the proxy the site was built with (`configured`, from config.js: FIRMWARE_PROXY_URL);
 *   * otherwise none: "Load from URLs" is not available. A relative /api/firmware (same origin) is never assumed,
 *     because the static site and the proxy are hosted separately.
 * @param {{hostname?: string}} location window.location (or a stand-in)
 * @param {string} [search] the query string
 * @param {string|null} [configured] FIRMWARE_PROXY_URL of config.js
 * @returns {{endpoint: string|null, kind: 'configured'|'dev'|'none', label: string, warning?: string}}
 */
export function proxyEndpoint(location, search = '', configured = null) {
  const loopbackPage = LOOPBACK_HOSTS.has(String(location.hostname || '').toLowerCase());
  const requested = new URLSearchParams(search).get('firmware-proxy');
  const warnings = [];
  if (requested !== null) {
    const dev = loopbackPage ? loopbackProxyUrl(requested) : null;
    if (dev) return { endpoint: dev, kind: 'dev', label: dev };
    warnings.push(loopbackPage
      ? 'The firmware-proxy parameter was ignored: it must be http://127.0.0.1:<port>/api/firmware (or localhost).'
      : 'The firmware-proxy parameter was ignored: it is only used when this page is served from localhost or 127.0.0.1.');
  }
  const url = configuredProxyUrl(configured);
  if (configured !== null && configured !== undefined && !url) warnings.push('The firmware proxy address this site was built with is not valid, so it is not used.');
  const warning = warnings.length ? { warning: warnings.join(' ') } : {};
  if (url) return { endpoint: url, kind: 'configured', label: url, ...warning };
  return { endpoint: null, kind: 'none', label: 'not configured', ...warning };
}

// ---- fetching --------------------------------------------------------------------------------------------------

export class FirmwareFetchError extends Error {
  /** @param {string} code one of: network, proxy-missing, proxy-forbidden, not-allowed, http, too-large, not-srec, timeout, aborted, proxy-error */
  constructor(code, message, details = {}) {
    super(message);
    this.name = 'FirmwareFetchError';
    this.code = code;
    Object.assign(this, details);
  }
}

function mib(count) {
  return `${(count / (1024 * 1024)).toFixed(0)} MiB`;
}

async function jsonBody(response) {
  if (!/json/i.test(response.headers.get('content-type') || '')) return null;
  try {
    return await response.json();
  } catch (_) {
    return null;
  }
}

/** Turns the proxy's non-200 answer into an error with a message for the user. */
async function proxyFailure(response, label) {
  const info = await jsonBody(response);
  const status = response.status;
  if (!info || typeof info.error !== 'string') {
    if (status === 404 || status === 405 || status === 501) {
      return new FirmwareFetchError('proxy-missing', `No firmware proxy answered at ${label} (HTTP ${status}). The proxy address this site was built with may be wrong or the proxy not deployed; when testing locally, start deploy/dev-proxy.mjs and open the page with ?firmware-proxy=http://127.0.0.1:<port>/api/firmware.`, { status });
    }
    return new FirmwareFetchError('proxy-error', `The firmware proxy at ${label} answered HTTP ${status} with something that is not its error format.`, { status });
  }
  const detail = typeof info.message === 'string' ? info.message : info.error;
  switch (info.error) {
    case 'upstream_not_allowed':
      return new FirmwareFetchError('not-allowed', `The proxy refused this address: ${detail}`, { status });
    case 'origin_not_allowed':
    case 'origin_required':
      return new FirmwareFetchError('proxy-forbidden', `The firmware proxy at ${label} does not serve this page (HTTP ${status}: ${detail}).`, { status });
    case 'upstream_status':
      return new FirmwareFetchError('http', `The source answered HTTP ${info.upstreamStatus}.`, { status, upstreamStatus: info.upstreamStatus });
    case 'upstream_too_large':
      return new FirmwareFetchError('too-large', `The file is larger than ${mib(MAX_FETCH_BYTES)}, which no firmware image is.`, { status });
    case 'not_srec':
      return new FirmwareFetchError('not-srec', 'The source did not return an S-record file (the data does not start with S0). A web page, an error page or another kind of file was returned.', { status });
    case 'upstream_timeout':
      return new FirmwareFetchError('timeout', `The source did not answer in time. ${detail}`, { status });
    default:
      return new FirmwareFetchError('proxy-error', `The firmware proxy reported: ${detail}`, { status });
  }
}

/**
 * Fetches one S-record file through the proxy. Resolves to {bytes: Uint8Array, source: string|null} (`source` is the
 * address the proxy finally fetched, after redirects) or throws a FirmwareFetchError with a user-ready message.
 *
 * @param {object} options
 * @param {string} options.endpoint the proxy endpoint (the configured absolute https address or a loopback dev proxy address)
 * @param {string} options.url an address returned by `normalizeFirmwareUrl`
 * @param {string} [options.label] how to name the proxy in messages
 * @param {typeof fetch} [options.fetchImpl]
 * @param {AbortSignal} [options.signal]
 * @param {(loaded: number, total: number|null) => void} [options.onProgress]
 */
export async function fetchFirmware({ endpoint, url, label = endpoint, fetchImpl = globalThis.fetch, signal, onProgress, maxBytes = MAX_FETCH_BYTES }) {
  let response;
  try {
    response = await fetchImpl(`${endpoint}?url=${encodeURIComponent(url)}`, { method: 'GET', credentials: 'omit', cache: 'no-store', referrerPolicy: 'no-referrer', signal });
  } catch (error) {
    if (signal && signal.aborted) throw new FirmwareFetchError('aborted', 'The request was stopped.');
    throw new FirmwareFetchError('network', `Could not reach the firmware proxy (${label}). Check the connection; when testing locally, start deploy/dev-proxy.mjs and open this page with ?firmware-proxy=http://127.0.0.1:<port>/api/firmware.`, { cause: error });
  }
  if (!response.ok) throw await proxyFailure(response, label);

  const declared = Number(response.headers.get('content-length'));
  const total = Number.isFinite(declared) && declared > 0 ? declared : null;
  if (total !== null && total > maxBytes) {
    try { await response.body?.cancel(); } catch (_) { /* ignore */ }
    throw new FirmwareFetchError('too-large', `The file is ${total} bytes, larger than ${mib(maxBytes)}, which no firmware image is.`);
  }
  const chunks = [];
  let loaded = 0;
  try {
    if (response.body && typeof response.body.getReader === 'function') {
      const reader = response.body.getReader();
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        loaded += value.byteLength;
        if (loaded > maxBytes) {
          await reader.cancel().catch(() => {});
          throw new FirmwareFetchError('too-large', `The file is larger than ${mib(maxBytes)}, which no firmware image is.`);
        }
        chunks.push(value);
        if (onProgress) onProgress(loaded, total);
      }
    } else {
      const buffer = new Uint8Array(await response.arrayBuffer());
      if (buffer.byteLength > maxBytes) throw new FirmwareFetchError('too-large', `The file is larger than ${mib(maxBytes)}, which no firmware image is.`);
      chunks.push(buffer);
      loaded = buffer.byteLength;
      if (onProgress) onProgress(loaded, total);
    }
  } catch (error) {
    if (error instanceof FirmwareFetchError) throw error;
    if (signal && signal.aborted) throw new FirmwareFetchError('aborted', 'The request was stopped.');
    throw new FirmwareFetchError('network', `The connection to the firmware proxy (${label}) broke while the file was being received.`, { cause: error });
  }
  const bytes = new Uint8Array(loaded);
  let at = 0;
  for (const chunk of chunks) {
    bytes.set(chunk, at);
    at += chunk.byteLength;
  }
  if (bytes.length < 2 || bytes[0] !== 0x53 || bytes[1] !== 0x30) {
    throw new FirmwareFetchError('not-srec', 'The received data is not an S-record file (it does not start with S0).');
  }
  return { bytes, source: response.headers.get('x-firmware-source') };
}
