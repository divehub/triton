#!/usr/bin/env node
// Tests of the firmware proxy (api/firmware.mjs) and its local dev server, with a stubbed upstream `fetch`: no network,
// no firmware. Run:
//
//   node deploy/test-proxy.mjs
//
// Covered: the upstream allowlist (accept and reject matrix, and agreement with the page-side copy in
// web/firmware-url.js), redirect validation, the request the upstream sees, the size cap (header and stream), the SREC
// check, timeouts, the response headers (no-store, nosniff, no upstream headers), CORS for every origin case, the
// same-origin rule, the preflight, the error JSON and the dev server.

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

import * as proxy from './api/firmware.mjs';
import { createDevServer } from './dev-proxy.mjs';
import { normalizeFirmwareUrl } from '../web/firmware-url.js';

const { GET, OPTIONS, MAX_BYTES, MAX_REDIRECTS, USER_AGENT, handle, isAllowedOrigin, parseUpstream } = proxy;
const here = path.dirname(fileURLToPath(import.meta.url));

const SITE = 'https://triton.divehub.ai';
const NAME = 'pvlL3Iilv4o_Tu5lggngZAUt';
const ORIGIN_URL = `https://api.multi3s.com/static/${NAME}.srec`;
const ARCHIVE = `https://web.archive.org/web/20261008041333/${ORIGIN_URL}`;
const ARCHIVE_RAW = `https://web.archive.org/web/20261008041333id_/${ORIGIN_URL}`;
const SREC_TEXT = 'S00600004844521B\r\nS9030000FC\r\n';

// ---- helpers ------------------------------------------------------------------------------------------------

/** A request as the browser would send it to the function. `headers` replaces the defaults (Origin: the site). */
function call(upstream, { headers = { origin: SITE }, method = 'GET', query } = {}) {
  const url = new URL(`${SITE}/api/firmware`);
  if (upstream !== undefined) url.searchParams.set('url', upstream);
  for (const [key, value] of query || []) url.searchParams.append(key, value);
  return new Request(url, { method, headers });
}

const reply = (body, { status = 200, headers = {} } = {}) => new Response(body, { status, headers });
const redirect = (location, status = 302) => new Response(null, { status, headers: { location } });

/** An upstream `fetch` stub: `responder(url, init, n)`; every call is recorded. */
function upstream(responder) {
  const calls = [];
  const fetch = async (url, init) => {
    calls.push({ url: String(url), init });
    return responder(String(url), init, calls.length);
  };
  return { fetch, calls };
}

const okUpstream = (body = SREC_TEXT, headers = {}) => upstream(() => reply(body, { headers }));

/**
 * A body of `total` bytes (starting with S0) delivered in `chunk`-sized pieces; `state` shows how much was read and
 * whether the consumer canceled (the proxy must stop reading as soon as it has seen enough).
 */
function bigBody(total, { chunk = 65536, first = 'S00600004844521B\r\n' } = {}) {
  const state = { pulled: 0, canceled: false };
  const head = new TextEncoder().encode(first);
  let sent = 0;
  const body = new ReadableStream({
    pull(controller) {
      if (sent >= total) return controller.close();
      const size = Math.min(chunk, total - sent);
      const piece = new Uint8Array(size).fill(0x41);
      if (sent === 0) piece.set(head.subarray(0, size));
      sent += size;
      state.pulled += size;
      return controller.enqueue(piece);
    },
    cancel() { state.canceled = true; },
  }, { highWaterMark: 0 }); // nothing is read ahead: `pulled` counts what the consumer asked for
  return { body, state };
}

async function json(response) {
  assert.match(response.headers.get('content-type'), /^application\/json; charset=utf-8$/);
  return response.json();
}

// ---- the allowlist -------------------------------------------------------------------------------------------

const ACCEPTED = [
  [ORIGIN_URL, ORIGIN_URL],
  ['https://api.multi3s.com/static/a.srec', 'https://api.multi3s.com/static/a.srec'],
  ['https://api.multi3s.com/static/a-b_C9.srec', 'https://api.multi3s.com/static/a-b_C9.srec'],
  [`https://api.multi3s.com/static/${'Z'.repeat(64)}.srec`, `https://api.multi3s.com/static/${'Z'.repeat(64)}.srec`],
  [ARCHIVE, ARCHIVE_RAW],
  [ARCHIVE_RAW, ARCHIVE_RAW],
  ['https://web.archive.org/web/20261008041427/https://api.multi3s.com/static/rlVpEk1qk8-0r1E4vMHNAjQG.srec', 'https://web.archive.org/web/20261008041427id_/https://api.multi3s.com/static/rlVpEk1qk8-0r1E4vMHNAjQG.srec'],
];

// Every entry is refused with this reason by the proxy and by the page-side copy.
const REJECTED = {
  'api.multi3s.com/static/x.srec': 'not-url',
  'ftp://api.multi3s.com/static/x.srec': 'scheme',
  'http://api.multi3s.com/static/x.srec': 'scheme',
  'http://web.archive.org/web/20261008041333id_/https://api.multi3s.com/static/x.srec': 'scheme',
  'https://user:pw@api.multi3s.com/static/x.srec': 'userinfo',
  'https://api.multi3s.com@evil.example/static/x.srec': 'userinfo',
  'https://evil.example@api.multi3s.com/static/x.srec': 'userinfo',
  'https://api.multi3s.com:8443/static/x.srec': 'port',
  'https://api.multi3s.com:443/static/x.srec': 'port',
  'https://api.multi3s.com:/static/x.srec': 'port',
  'https://web.archive.org:8443/web/20261008041333id_/https://api.multi3s.com/static/x.srec': 'port',
  'https://evil.example/static/x.srec': 'host',
  'https://api.multi3s.com.evil.example/static/x.srec': 'host',
  'https://evilapi.multi3s.com/static/x.srec': 'host',
  'https://sub.api.multi3s.com/static/x.srec': 'host',
  'https://multi3s.com/static/x.srec': 'host',
  'https://web.archive.org.evil.example/web/20261008041333id_/https://api.multi3s.com/static/x.srec': 'host',
  'https://archive.org/web/20261008041333id_/https://api.multi3s.com/static/x.srec': 'host',
  'https://127.0.0.1/static/x.srec': 'host',
  'https://[::1]/static/x.srec': 'host',
  'https://[::ffff:7f00:1]/static/x.srec': 'host',
  'https://2130706433/static/x.srec': 'host',
  'https://0x7f000001/static/x.srec': 'host',
  'https://169.254.169.254/latest/meta-data/': 'host',
  'https://localhost/static/x.srec': 'host',
  'https://metadata.google.internal/static/x.srec': 'host',
  'https://api.multi3s.com/static/x.srec?x=1': 'query',
  'https://api.multi3s.com/static/x.srec?': 'query',
  'https://web.archive.org/web/20261008041333id_/https://api.multi3s.com/static/x.srec?a=b': 'query',
  'https://api.multi3s.com/static/x.srec#frag': 'fragment',
  'https://api.multi3s.com/static/x.srec#': 'fragment',
  'https://api.multi3s.com/static/%2e%2e/x.srec': 'encoded',
  'https://api.multi3s.com/static/%2E%2E%2Fx.srec': 'encoded',
  'https://api.multi3s.com/static/x%2esrec': 'encoded',
  'https://api.multi3s.com/static/x.srec%00': 'encoded',
  'https://api.multi3s.com/static/x.srec%0d%0aHost:evil': 'encoded',
  'https://api.multi3s.com%2fstatic/x.srec': 'encoded',
  'https://api.multi3s.com/static/../x.srec': 'path',
  'https://api.multi3s.com/static/./x.srec': 'path',
  'https://api.multi3s.com//static/x.srec': 'path',
  'https://api.multi3s.com/other/x.srec': 'path',
  'https://api.multi3s.com/static/x.bin': 'path',
  'https://api.multi3s.com/static/x.SREC': 'path',
  'https://api.multi3s.com/static/x.srec/': 'path',
  'https://api.multi3s.com/static/a/b.srec': 'path',
  'https://api.multi3s.com/static/.srec': 'path',
  'https://api.multi3s.com/static/x.srec.srec': 'path',
  'https://api.multi3s.com/static/x;.srec': 'path',
  'https://api.multi3s.com/STATIC/x.srec': 'path',
  'https://api.multi3s.com/': 'path',
  'https://api.multi3s.com': 'path',
  [`https://api.multi3s.com/static/${'a'.repeat(65)}.srec`]: 'path',
  'https://api.multi3s.com\\static\\x.srec': 'characters',
  'https://api.multi3s.com/static/x y.srec': 'characters',
  'https://api.multi3s.com/static/x.srec junk': 'characters',
  'https://api.multi3s.com/static/x.srec\r\nHost: evil': 'characters',
  'https://api.multi3s.com/static/é.srec': 'characters',
  'https://web.archive.org/web/2026/https://api.multi3s.com/static/x.srec': 'archive-form',
  'https://web.archive.org/web/202610080413333/https://api.multi3s.com/static/x.srec': 'archive-form',
  'https://web.archive.org/web/20261008041333im_/https://api.multi3s.com/static/x.srec': 'archive-form',
  'https://web.archive.org/web/20261008041333id_/https://evil.example/static/x.srec': 'archive-form',
  'https://web.archive.org/web/20261008041333/http://api.multi3s.com/static/x.srec': 'archive-form',
  'https://web.archive.org/web/20261008041333/https:/api.multi3s.com/static/x.srec': 'archive-form',
  'https://web.archive.org/web/20261008041333/https://api.multi3s.com/static/../x.srec': 'archive-form',
  'https://web.archive.org/save/https://api.multi3s.com/static/x.srec': 'archive-form',
  'https://web.archive.org/web/20261008041333/https://web.archive.org/web/20261008041333/https://api.multi3s.com/static/x.srec': 'archive-form',
  'https://web.archive.org/': 'archive-form',
  [`https://${'a'.repeat(300)}.example/x.srec`]: 'too-long',
};

test('allowlist: the two accepted forms, and the Wayback form is normalized to the raw id_ form', () => {
  for (const [input, expected] of ACCEPTED) {
    const result = parseUpstream(input);
    assert.equal(result.ok, true, input);
    assert.equal(result.url, expected, input);
    assert.equal(result.kind, input.includes('web.archive.org') ? 'archive' : 'origin');
  }
  assert.deepEqual(parseUpstream(ARCHIVE), { ok: true, kind: 'archive', name: NAME, timestamp: '20261008041333', url: ARCHIVE_RAW });
});

test('allowlist: everything else is refused, with a reason and without echoing the input', () => {
  for (const [input, reason] of Object.entries(REJECTED)) {
    const result = parseUpstream(input);
    assert.equal(result.ok, false, `${input.slice(0, 100)} must be refused`);
    assert.equal(result.reason, reason, `${input.slice(0, 100)} is refused as ${reason}`);
    assert.ok(result.message.length > 10 && !result.message.includes('evil'), 'the message is fixed text');
  }
  for (const value of [undefined, null, 42, {}, [], '']) assert.equal(parseUpstream(value).ok, false);
  assert.equal(parseUpstream('').reason, 'empty');
  // The proxy is strict about case and white space: only the page-side copy normalizes those.
  for (const input of [
    'HTTPS://api.multi3s.com/static/x.srec', 'https://API.MULTI3S.COM/static/x.srec', 'https://api.multi3s.com/static/x.srec\n', ' https://api.multi3s.com/static/x.srec',
    'https://web.archive.org/web/20261008041333ID_/https://api.multi3s.com/static/x.srec', 'https://web.archive.org/web/20261008041333/HTTPS://api.multi3s.com/static/x.srec',
  ]) {
    assert.equal(parseUpstream(input).ok, false, JSON.stringify(input));
  }
});

test('allowlist: the page-side copy (web/firmware-url.js) agrees on every case', () => {
  for (const [input, expected] of ACCEPTED) {
    const page = normalizeFirmwareUrl(input);
    assert.equal(page.ok, true, input);
    assert.equal(page.url, expected, 'the page asks for exactly the address the proxy would fetch');
    assert.equal(page.url, parseUpstream(page.url).url, 'and the proxy keeps it as it is');
  }
  for (const [input, reason] of Object.entries(REJECTED)) {
    const page = normalizeFirmwareUrl(input);
    assert.equal(page.ok, false, `page must refuse ${input.slice(0, 100)}`);
    assert.equal(page.reason, reason, `page and proxy give the same reason for ${input.slice(0, 100)}`);
  }
  // What the page normalizes (white space, case of the schemes and hosts) the proxy would refuse raw; after the
  // page's normalization it accepts. Paths, names and extensions are case-sensitive upstream and never changed.
  for (const sloppy of [' https://api.multi3s.com/static/x.srec ', 'HTTPS://API.MULTI3S.COM/static/x.srec\n', 'https://WEB.ARCHIVE.ORG/web/20261008041333id_/HTTPS://API.MULTI3S.COM/static/x.srec']) {
    const page = normalizeFirmwareUrl(sloppy);
    assert.equal(page.ok, true, sloppy);
    assert.equal(parseUpstream(sloppy).ok, false);
    assert.equal(parseUpstream(page.url).ok, true);
  }
});

// ---- request handling: allowlist at the endpoint ------------------------------------------------------------

test('endpoint: accepted addresses are fetched exactly as normalized, and only those', async () => {
  for (const [input, expected] of ACCEPTED) {
    const stub = okUpstream();
    const response = await handle(call(input), { fetch: stub.fetch });
    assert.equal(response.status, 200, input);
    assert.deepEqual(stub.calls.map((entry) => entry.url), [expected]);
    assert.equal(response.headers.get('x-firmware-source'), expected);
  }
});

test('endpoint: refused addresses get 400 and a short JSON error, and never reach the network', async () => {
  for (const [input, reason] of Object.entries(REJECTED)) {
    const stub = okUpstream();
    const response = await handle(call(input), { fetch: stub.fetch });
    assert.equal(response.status, 400, input.slice(0, 100));
    const body = await json(response);
    assert.deepEqual(Object.keys(body).sort(), ['error', 'message', 'reason']);
    assert.equal(body.error, 'upstream_not_allowed');
    assert.equal(body.reason, reason);
    assert.equal(stub.calls.length, 0, `${input.slice(0, 100)} must not be fetched`);
    assert.equal(response.headers.get('access-control-allow-origin'), SITE, 'the page can read the error');
    assert.equal(response.headers.get('cache-control'), 'private, no-store');
  }
});

test('endpoint: encoded tricks in the query string do not get around the allowlist', async () => {
  const stub = okUpstream();
  // Double encoding: the decoded parameter still contains a "%".
  const double = new Request(`${SITE}/api/firmware?url=${encodeURIComponent(encodeURIComponent(ORIGIN_URL))}`, { headers: { origin: SITE } });
  assert.equal((await handle(double, { fetch: stub.fetch })).status, 400);
  // "+" decodes to a space; a decoded newline is a control character.
  for (const raw of [`url=${ORIGIN_URL.replace('static/', 'static/+')}`, `url=${encodeURIComponent(`${ORIGIN_URL}\n`)}`, `url=${encodeURIComponent(`${ORIGIN_URL}&url=${ORIGIN_URL}`)}`]) {
    assert.equal((await handle(new Request(`${SITE}/api/firmware?${raw}`, { headers: { origin: SITE } }), { fetch: stub.fetch })).status, 400, raw);
  }
  // An unencoded address in the query string works (the page encodes, but a browser or curl may not).
  const plain = new Request(`${SITE}/api/firmware?url=${ORIGIN_URL}`, { headers: { origin: SITE } });
  assert.equal((await handle(plain, { fetch: stub.fetch })).status, 200);
  assert.equal(stub.calls.length, 1);
});

test('endpoint: exactly one url parameter is required; other parameters are ignored', async () => {
  const stub = okUpstream();
  for (const request of [call(undefined), call(''), call(ORIGIN_URL, { query: [['url', ORIGIN_URL]] }), call('x'.repeat(5000))]) {
    const response = await handle(request, { fetch: stub.fetch });
    assert.equal(response.status, 400);
    assert.match((await json(response)).error, /^(bad_request|upstream_not_allowed)$/);
  }
  assert.equal((await handle(call(undefined), { fetch: stub.fetch }).then((r) => r.json())).error, 'bad_request');
  assert.equal(stub.calls.length, 0);
  const extra = await handle(call(ORIGIN_URL, { query: [['cache', 'bust'], ['callback', 'x']] }), { fetch: stub.fetch });
  assert.equal(extra.status, 200);
  assert.deepEqual(stub.calls.map((entry) => entry.url), [ORIGIN_URL], 'extra parameters never reach the upstream');
});

// ---- what the upstream sees -----------------------------------------------------------------------------------

test('upstream request: a plain GET with a fixed User-Agent, no cookies or credentials, manual redirects and a deadline', async () => {
  const stub = okUpstream();
  const request = call(ORIGIN_URL, {
    headers: {
      origin: SITE, cookie: 'session=secret', authorization: 'Bearer secret', referer: 'https://triton.divehub.ai/page', 'x-forwarded-for': '203.0.113.9',
      'x-vercel-ip-country': 'JP', 'user-agent': 'Mozilla/5.0 (someone)', range: 'bytes=0-9', 'if-none-match': '"x"', accept: 'text/html',
    },
  });
  assert.equal((await handle(request, { fetch: stub.fetch })).status, 200);
  assert.equal(stub.calls.length, 1);
  const { init } = stub.calls[0];
  assert.equal(init.method, 'GET');
  assert.equal(init.redirect, 'manual');
  assert.equal(init.credentials, 'omit');
  assert.ok(init.signal instanceof AbortSignal);
  assert.deepEqual(Object.keys(init).sort(), ['credentials', 'headers', 'method', 'redirect', 'signal']);
  assert.deepEqual(Object.keys(init.headers).map((key) => key.toLowerCase()).sort(), ['accept', 'accept-encoding', 'user-agent']);
  assert.equal(init.headers['User-Agent'], USER_AGENT);
  assert.match(USER_AGENT, /^ngc-firmware-proxy\/\d/);
  assert.equal(init.headers['Accept-Encoding'], 'identity');
  assert.equal(init.body, undefined);
});

// ---- redirects --------------------------------------------------------------------------------------------------

test('redirects: followed manually, each Location must match the allowlist again (and is normalized)', async () => {
  // origin -> Wayback (without id_) -> file: the second request uses the normalized id_ form.
  const chain = upstream((url) => (url === ORIGIN_URL ? redirect(ARCHIVE) : reply(SREC_TEXT)));
  const done = await handle(call(ORIGIN_URL), { fetch: chain.fetch });
  assert.equal(done.status, 200);
  assert.deepEqual(chain.calls.map((entry) => entry.url), [ORIGIN_URL, ARCHIVE_RAW]);
  assert.ok(chain.calls.every((entry) => entry.init.redirect === 'manual'));
  assert.equal(done.headers.get('x-firmware-source'), ARCHIVE_RAW, 'the page is told where the file finally came from');
  // The Wayback Machine redirects to another capture, with a relative Location.
  const relative = upstream((url, init, n) => (n === 1 ? redirect('/web/20261008041400id_/https://api.multi3s.com/static/other.srec', 301) : reply(SREC_TEXT)));
  assert.equal((await handle(call(ARCHIVE), { fetch: relative.fetch })).status, 200);
  assert.equal(relative.calls[1].url, 'https://web.archive.org/web/20261008041400id_/https://api.multi3s.com/static/other.srec');
  // A relative Location on the origin host.
  const same = upstream((url, init, n) => (n === 1 ? redirect('/static/other.srec', 308) : reply(SREC_TEXT)));
  assert.equal((await handle(call(ORIGIN_URL), { fetch: same.fetch })).status, 200);
  assert.equal(same.calls[1].url, 'https://api.multi3s.com/static/other.srec');
  for (const status of [301, 302, 303, 307, 308]) {
    const kinds = upstream((url, init, n) => (n === 1 ? redirect(ORIGIN_URL.replace(NAME, 'next'), status) : reply(SREC_TEXT)));
    assert.equal((await handle(call(ORIGIN_URL), { fetch: kinds.fetch })).status, 200, `HTTP ${status}`);
  }
});

test('redirects: a Location outside the allowlist is never fetched', async () => {
  const bad = [
    'https://evil.example/static/x.srec', 'http://api.multi3s.com/static/x.srec', '//evil.example/static/x.srec',
    'https://api.multi3s.com:444/static/x.srec', 'https://api.multi3s.com/static/x.srec?x=1', 'https://api.multi3s.com/static/x.srec#f',
    'https://api.multi3s.com/static/x.bin', '/other/x.srec', '/static/../x.srec', '/static/%2e%2e/x.srec', 'https://user@api.multi3s.com/static/x.srec',
    'https://127.0.0.1/static/x.srec', 'https://169.254.169.254/latest/meta-data/', 'javascript:alert(1)', 'data:text/plain,S0', 'file:///etc/passwd',
    'https://web.archive.org/web/20261008041333im_/https://api.multi3s.com/static/x.srec', '/web/2026/https://api.multi3s.com/static/x.srec',
    'https://api.multi3s.com/static/' + 'a'.repeat(65) + '.srec', '', '   ',
  ];
  for (const location of bad) {
    const stub = upstream((url, init, n) => (n === 1 ? redirect(location) : reply(SREC_TEXT)));
    const response = await handle(call(ORIGIN_URL), { fetch: stub.fetch });
    assert.equal(response.status, 502, location);
    assert.equal((await json(response)).error, 'redirect_not_allowed', location);
    assert.equal(stub.calls.length, 1, `${location} was not followed`);
  }
  // A redirect without a Location header.
  const none = upstream(() => new Response(null, { status: 302 }));
  assert.equal((await json(await handle(call(ORIGIN_URL), { fetch: none.fetch }))).error, 'redirect_not_allowed');
  // 304 and 300 are not redirects we follow: they are plain upstream statuses.
  for (const status of [300, 304]) {
    const other = upstream(() => new Response(null, { status, headers: { location: ORIGIN_URL } }));
    const response = await handle(call(ORIGIN_URL), { fetch: other.fetch });
    assert.equal((await json(response)).error, 'upstream_status');
    assert.equal(other.calls.length, 1);
  }
});

test('redirects: at most three are followed', async () => {
  assert.equal(MAX_REDIRECTS, 3);
  const hops = (count) => upstream((url, init, n) => (n <= count ? redirect(`https://api.multi3s.com/static/hop${n}.srec`) : reply(SREC_TEXT)));
  const three = hops(3);
  assert.equal((await handle(call(ORIGIN_URL), { fetch: three.fetch })).status, 200);
  assert.equal(three.calls.length, 4);
  const four = hops(4);
  const refused = await handle(call(ORIGIN_URL), { fetch: four.fetch });
  assert.equal(refused.status, 502);
  assert.equal((await json(refused)).error, 'too_many_redirects');
  assert.equal(four.calls.length, 4, 'the fourth redirect is not followed');
  const loop = upstream(() => redirect(ORIGIN_URL));
  assert.equal((await json(await handle(call(ORIGIN_URL), { fetch: loop.fetch }))).error, 'too_many_redirects');
  assert.equal(loop.calls.length, 4);
});

// ---- statuses, size, content --------------------------------------------------------------------------------

test('upstream status: anything but 200 is a 502 with the status', async () => {
  for (const status of [204, 206, 400, 401, 403, 404, 410, 429, 500, 503]) {
    const stub = upstream(() => new Response(status === 204 ? null : 'nope', { status }));
    const response = await handle(call(ORIGIN_URL), { fetch: stub.fetch });
    assert.equal(response.status, 502, `upstream ${status}`);
    const body = await json(response);
    assert.deepEqual([body.error, body.upstreamStatus], ['upstream_status', status]);
    assert.match(body.message, new RegExp(`HTTP ${status}`));
  }
});

test('size cap: refused by Content-Length first, and while streaming; the upstream body is canceled', async () => {
  assert.equal(MAX_BYTES, 4 * 1024 * 1024);
  // By header: the body is never read.
  const header = bigBody(MAX_BYTES + 1);
  const byHeader = upstream(() => reply(header.body, { headers: { 'content-length': String(MAX_BYTES + 1) } }));
  const refused = await handle(call(ORIGIN_URL), { fetch: byHeader.fetch });
  assert.equal(refused.status, 502);
  assert.equal((await json(refused)).error, 'upstream_too_large');
  assert.equal(header.state.pulled, 0, 'nothing was read');
  assert.equal(header.state.canceled, true);
  // While streaming: no Content-Length, or a lying one.
  for (const headers of [{}, { 'content-length': '1000' }]) {
    const stream = bigBody(MAX_BYTES * 3);
    const streaming = upstream(() => reply(stream.body, { headers }));
    const response = await handle(call(ORIGIN_URL), { fetch: streaming.fetch });
    assert.equal(response.status, 502);
    assert.equal((await json(response)).error, 'upstream_too_large');
    assert.ok(stream.state.pulled <= MAX_BYTES + 3 * 65536, `stopped reading at ${stream.state.pulled} bytes`);
    assert.equal(stream.state.canceled, true);
  }
  // Exactly the cap is fine (and a Content-Length of exactly the cap too).
  const exact = bigBody(MAX_BYTES);
  const fits = await handle(call(ORIGIN_URL), { fetch: upstream(() => reply(exact.body, { headers: { 'content-length': String(MAX_BYTES) } })).fetch });
  assert.equal(fits.status, 200);
  assert.equal((await fits.arrayBuffer()).byteLength, MAX_BYTES);
  const over = bigBody(MAX_BYTES + 1);
  assert.equal((await handle(call(ORIGIN_URL), { fetch: upstream(() => reply(over.body)).fetch })).status, 502);
});

test('SREC check: the body must start with ASCII S0', async () => {
  const bytes = new TextEncoder().encode(SREC_TEXT);
  const good = await handle(call(ORIGIN_URL), { fetch: upstream(() => reply(bytes)).fetch });
  assert.equal(good.status, 200);
  assert.deepEqual(new Uint8Array(await good.arrayBuffer()), bytes, 'the bytes pass through unchanged');
  // The S and the 0 may arrive in different chunks.
  const split = upstream(() => reply(new ReadableStream({
    start(controller) {
      controller.enqueue(new TextEncoder().encode('S'));
      controller.enqueue(new TextEncoder().encode(SREC_TEXT.slice(1)));
      controller.close();
    },
  })));
  const joined = await handle(call(ORIGIN_URL), { fetch: split.fetch });
  assert.equal(joined.status, 200);
  assert.equal(await joined.text(), SREC_TEXT);
  for (const body of ['<!doctype html><title>Wayback</title>', '', 'S', 's0', ' S0', '﻿S0', 'S1', '0S', '{"error":"x"}', '\u0000S0', 'S']) {
    const response = await handle(call(ORIGIN_URL), { fetch: upstream(() => reply(body)).fetch });
    assert.equal(response.status, 502, JSON.stringify(body));
    assert.equal((await json(response)).error, 'not_srec', JSON.stringify(body));
  }
  assert.equal((await json(await handle(call(ORIGIN_URL), { fetch: upstream(() => new Response(null, { status: 200 })).fetch }))).error, 'not_srec');
  // A large non-SREC body is dropped as soon as its start is seen.
  const html = bigBody(MAX_BYTES, { first: '<html>' });
  const early = await handle(call(ORIGIN_URL), { fetch: upstream(() => reply(html.body)).fetch });
  assert.equal((await json(early)).error, 'not_srec');
  assert.ok(html.state.pulled <= 65536 * 2, `stopped after ${html.state.pulled} bytes`);
  assert.equal(html.state.canceled, true);
});

test('failures: timeouts are 504, a broken connection is 502, a throwing stub never leaks', async () => {
  // The headers never arrive.
  const silent = upstream((url, init) => new Promise((resolve, reject) => init.signal.addEventListener('abort', () => reject(init.signal.reason))));
  const started = Date.now();
  const late = await handle(call(ORIGIN_URL), { fetch: silent.fetch, timeoutMs: 30 });
  assert.equal(late.status, 504);
  assert.equal((await json(late)).error, 'upstream_timeout');
  assert.ok(Date.now() - started < 2000);
  // The headers arrive, the body stalls.
  const stalled = upstream((url, init) => reply(new ReadableStream({
    start(controller) {
      controller.enqueue(new TextEncoder().encode('S00600'));
      init.signal.addEventListener('abort', () => controller.error(init.signal.reason));
    },
  })));
  const stall = await handle(call(ORIGIN_URL), { fetch: stalled.fetch, timeoutMs: 30 });
  assert.equal(stall.status, 504);
  assert.equal((await json(stall)).error, 'upstream_timeout');
  // Network errors.
  const down = upstream(() => { throw new TypeError('fetch failed'); });
  const unreachable = await handle(call(ORIGIN_URL), { fetch: down.fetch });
  assert.equal(unreachable.status, 502);
  assert.equal((await json(unreachable)).error, 'upstream_unreachable');
  const broken = upstream(() => reply(new ReadableStream({
    start(controller) {
      controller.enqueue(new TextEncoder().encode(SREC_TEXT));
      controller.error(new Error('socket hang up'));
    },
  })));
  assert.equal((await json(await handle(call(ORIGIN_URL), { fetch: broken.fetch }))).error, 'upstream_unreachable');
  assert.equal(proxy.UPSTREAM_TIMEOUT_MS, 20_000);
});

// ---- the response -------------------------------------------------------------------------------------------------

test('response: plain ASCII text, never cached, nosniff, no referrer; nothing from the upstream headers leaks', async () => {
  const stub = okUpstream(SREC_TEXT, {
    'content-type': 'application/octet-stream', 'content-disposition': 'attachment; filename="firmware.srec"', etag: '"abc"', 'last-modified': 'Mon, 01 Jan 2024 00:00:00 GMT',
    'set-cookie': 'sid=1; Path=/', server: 'nginx', 'x-archive-orig-date': 'x', link: '<https://evil.example>; rel=preload', 'content-security-policy': "default-src 'none'",
    'cache-control': 'public, max-age=31536000', 'access-control-allow-origin': '*', age: '100', 'x-cache': 'HIT',
  });
  const response = await handle(call(ORIGIN_URL), { fetch: stub.fetch });
  assert.equal(response.status, 200);
  const headers = Object.fromEntries(response.headers);
  assert.deepEqual(headers, {
    'access-control-allow-origin': SITE,
    'access-control-expose-headers': 'X-Firmware-Source',
    'cache-control': 'private, no-store',
    'content-length': String(new TextEncoder().encode(SREC_TEXT).length),
    'content-type': 'text/plain; charset=us-ascii',
    'referrer-policy': 'no-referrer',
    vary: 'Origin',
    'x-content-type-options': 'nosniff',
    'x-firmware-source': ORIGIN_URL,
  });
  assert.equal(await response.text(), SREC_TEXT);
});

test('response: the same-origin form (no Origin header) carries no CORS headers but the same protections', async () => {
  const response = await handle(call(ORIGIN_URL, { headers: { 'sec-fetch-site': 'same-origin' } }), { fetch: okUpstream().fetch });
  assert.equal(response.status, 200);
  assert.equal(response.headers.get('access-control-allow-origin'), null);
  assert.equal(response.headers.get('cache-control'), 'private, no-store');
  assert.equal(response.headers.get('vary'), 'Origin');
  assert.equal(response.headers.get('x-content-type-options'), 'nosniff');
});

test('response: every error is small JSON with the same protections and never echoes the request', async () => {
  const secret = 'https://evil.example/static/SECRET-MARKER.srec';
  const cases = [
    () => handle(call(secret), { fetch: okUpstream().fetch }),
    () => handle(call(ORIGIN_URL, { headers: { origin: 'https://evil.example' } }), { fetch: okUpstream().fetch }),
    () => handle(call(ORIGIN_URL, { method: 'POST' }), { fetch: okUpstream().fetch }),
    () => handle(call(ORIGIN_URL), { fetch: upstream(() => new Response('x', { status: 404 })).fetch }),
    () => handle(call(ORIGIN_URL), { fetch: upstream(() => { throw new Error(`boom ${secret}`); }).fetch }),
  ];
  for (const run of cases) {
    const response = await run();
    assert.ok(response.status >= 400 && response.status < 600);
    assert.equal(response.headers.get('cache-control'), 'private, no-store');
    assert.equal(response.headers.get('x-content-type-options'), 'nosniff');
    assert.equal(response.headers.get('referrer-policy'), 'no-referrer');
    assert.equal(response.headers.get('vary'), 'Origin');
    const raw = await response.text();
    assert.ok(raw.length < 600, 'short');
    assert.ok(!raw.includes('SECRET-MARKER') && !raw.includes('boom'), 'nothing from the request or an exception is echoed');
    const body = JSON.parse(raw);
    assert.match(body.error, /^[a-z_]+$/);
    assert.equal(typeof body.message, 'string');
    assert.equal(response.headers.get('content-type'), 'application/json; charset=utf-8');
  }
});

// ---- who may call it ------------------------------------------------------------------------------------------

test('origins: the production site and localhost / 127.0.0.1 on any port are allowed', () => {
  for (const origin of [
    'https://triton.divehub.ai', 'http://localhost', 'http://localhost:8774', 'http://localhost:3000', 'http://127.0.0.1', 'http://127.0.0.1:8774',
    'http://127.0.0.1:1', 'http://127.0.0.1:65535',
  ]) assert.equal(isAllowedOrigin(origin), true, origin);
  for (const origin of [
    'https://localhost', 'https://127.0.0.1:8774', 'http://localhost.evil.example', 'http://127.0.0.1.evil.example', 'http://localhost:abc',
    'http://localhost:0', 'http://localhost:070', 'http://localhost:65536', 'http://localhost:123456', 'http://localhost:', 'http://localhost/',
    'http://[::1]:8774', 'http://0.0.0.0:8774', 'http://127.0.0.2:8774', 'http://user@localhost:8774', 'http://localhost:8774/path',
    'https://triton.divehub.ai/', 'https://triton.divehub.ai:8443', 'http://triton.divehub.ai', 'https://TRITON.divehub.ai', 'https://sub.triton.divehub.ai',
    'https://triton.divehub.ai.evil.example', 'https://eviltriton.divehub.ai', 'https://divehub.ai', 'https://evil.example', 'null', '', 'https://ngc-git-x.vercel.app',
  ]) assert.equal(isAllowedOrigin(origin), false, origin);
});

test('CORS: an allowed Origin is echoed with Vary: Origin; a disallowed one gets 403 without CORS headers', async () => {
  for (const origin of ['https://triton.divehub.ai', 'http://localhost:8774', 'http://127.0.0.1:8774', 'http://127.0.0.1', 'http://localhost']) {
    const stub = okUpstream();
    const response = await handle(call(ORIGIN_URL, { headers: { origin, 'sec-fetch-site': 'cross-site' } }), { fetch: stub.fetch });
    assert.equal(response.status, 200, origin);
    assert.equal(response.headers.get('access-control-allow-origin'), origin);
    assert.equal(response.headers.get('vary'), 'Origin');
    assert.equal(response.headers.get('access-control-allow-credentials'), null, 'credentials are never allowed');
    assert.equal(response.headers.get('access-control-allow-origin') === '*', false);
  }
  for (const origin of ['https://evil.example', 'http://triton.divehub.ai', 'https://localhost', 'http://localhost.evil.example', 'null', 'https://triton.divehub.ai.evil.example', 'http://localhost:99999', '']) {
    const stub = okUpstream();
    const response = await handle(call(ORIGIN_URL, { headers: { origin, 'sec-fetch-site': 'same-origin' } }), { fetch: stub.fetch });
    assert.equal(response.status, 403, `Origin ${JSON.stringify(origin)}`);
    assert.equal((await json(response)).error, 'origin_not_allowed');
    assert.equal(response.headers.get('access-control-allow-origin'), null);
    assert.equal(response.headers.get('vary'), 'Origin');
    assert.equal(stub.calls.length, 0, 'a refused origin never causes an upstream request');
  }
});

test('CORS: without an Origin header only Sec-Fetch-Site: same-origin is served', async () => {
  const served = await handle(call(ORIGIN_URL, { headers: { 'sec-fetch-site': 'same-origin' } }), { fetch: okUpstream().fetch });
  assert.equal(served.status, 200);
  for (const headers of [{}, { 'sec-fetch-site': 'same-site' }, { 'sec-fetch-site': 'cross-site' }, { 'sec-fetch-site': 'none' }, { 'sec-fetch-site': 'SAME-ORIGIN ' }, { 'sec-fetch-site': '' }, { 'user-agent': 'curl/8', referer: SITE }]) {
    const stub = okUpstream();
    const response = await handle(call(ORIGIN_URL, { headers }), { fetch: stub.fetch });
    assert.equal(response.status, 403, JSON.stringify(headers));
    const body = await json(response);
    assert.equal(body.error, 'origin_required');
    assert.equal(response.headers.get('access-control-allow-origin'), null);
    assert.equal(stub.calls.length, 0);
  }
});

test('CORS preflight: allowed origins get GET only, no credentials, no extra headers; everything else is refused', async () => {
  const stub = okUpstream();
  for (const origin of ['https://triton.divehub.ai', 'http://localhost:8774', 'http://127.0.0.1:8775']) {
    const response = await handle(call(undefined, { method: 'OPTIONS', headers: { origin, 'access-control-request-method': 'GET' } }), { fetch: stub.fetch });
    assert.equal(response.status, 204, origin);
    assert.equal(await response.text(), '');
    assert.deepEqual(Object.fromEntries(response.headers), {
      'access-control-allow-methods': 'GET',
      'access-control-allow-origin': origin,
      'access-control-expose-headers': 'X-Firmware-Source',
      'access-control-max-age': '600',
      'cache-control': 'private, no-store',
      'referrer-policy': 'no-referrer',
      vary: 'Origin',
      'x-content-type-options': 'nosniff',
    });
    assert.equal(response.headers.get('access-control-allow-credentials'), null);
    assert.equal(response.headers.get('access-control-allow-headers'), null, 'no custom request headers are allowed');
  }
  for (const headers of [{ origin: 'https://evil.example' }, { origin: 'http://triton.divehub.ai' }, {}, { 'sec-fetch-site': 'cross-site' }]) {
    const response = await handle(call(undefined, { method: 'OPTIONS', headers: { 'access-control-request-method': 'GET', ...headers } }), { fetch: stub.fetch });
    assert.equal(response.status, 403, JSON.stringify(headers));
    assert.equal(response.headers.get('access-control-allow-origin'), null);
  }
  assert.equal(stub.calls.length, 0, 'a preflight never causes an upstream request');
  // The exports Vercel calls behave the same.
  const exported = await OPTIONS(call(undefined, { method: 'OPTIONS', headers: { origin: SITE } }));
  assert.equal(exported.status, 204);
});

test('methods: only GET and OPTIONS', async () => {
  const stub = okUpstream();
  for (const method of ['POST', 'PUT', 'DELETE', 'PATCH', 'HEAD']) {
    const response = await handle(call(ORIGIN_URL, { method }), { fetch: stub.fetch });
    assert.equal(response.status, 405, method);
    assert.equal(response.headers.get('allow'), 'GET, OPTIONS');
    assert.equal((await json(response)).error, 'method_not_allowed');
  }
  assert.equal(stub.calls.length, 0);
});

// ---- the module as Vercel sees it -----------------------------------------------------------------------------

test('module: GET and OPTIONS are the handler exports and use the global fetch', async () => {
  assert.equal(typeof GET, 'function');
  assert.equal(typeof OPTIONS, 'function');
  for (const method of ['POST', 'PUT', 'DELETE', 'PATCH', 'HEAD', 'fetch', 'default']) assert.equal(proxy[method], undefined, `no ${method} export`);
  const stub = okUpstream();
  const original = globalThis.fetch;
  globalThis.fetch = stub.fetch;
  try {
    const response = await GET(call(ORIGIN_URL));
    assert.equal(response.status, 200);
    assert.equal(await response.text(), SREC_TEXT);
    assert.deepEqual(stub.calls.map((entry) => entry.url), [ORIGIN_URL]);
  } finally {
    globalThis.fetch = original;
  }
});

test('module: no dependencies, no logging, no storage, no environment', () => {
  const source = fs.readFileSync(path.join(here, 'api', 'firmware.mjs'), 'utf8');
  assert.doesNotMatch(source, /^\s*import\s/m, 'no imports at all');
  assert.doesNotMatch(source, /\brequire\s*\(/);
  assert.doesNotMatch(source, /\bconsole\./, 'nothing is logged');
  assert.doesNotMatch(source, /process\.env|\bfs\b|node:|@vercel\/|caches\./);
  const exported = [...source.matchAll(/^export (?:async )?(?:function|const) (\w+)/gm)].map((match) => match[1]).sort();
  assert.deepEqual(exported, ['GET', 'MAX_BYTES', 'MAX_REDIRECTS', 'OPTIONS', 'UPSTREAM_TIMEOUT_MS', 'USER_AGENT', 'handle', 'isAllowedOrigin', 'parseUpstream']);
  const manifest = path.join(here, 'package.json');
  if (fs.existsSync(manifest)) assert.equal(JSON.parse(fs.readFileSync(manifest, 'utf8')).dependencies, undefined);
});

// ---- the dev server ---------------------------------------------------------------------------------------------

function listen(handlers) {
  const log = [];
  const server = createDevServer({ handlers, onRequest: (...entry) => log.push(entry) });
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => resolve({ server, log, port: server.address().port })));
}

function rawRequest(port, { method = 'GET', pathname = '/api/firmware', headers = {} } = {}) {
  return new Promise((resolve, reject) => {
    const request = http.request({ host: '127.0.0.1', port, method, path: pathname, headers }, (response) => {
      const chunks = [];
      response.on('data', (chunk) => chunks.push(chunk));
      response.on('end', () => resolve({ status: response.statusCode, headers: response.headers, body: Buffer.concat(chunks) }));
    });
    request.on('error', reject);
    request.end();
  });
}

test('dev server: serves the production handlers on loopback only, for /api/firmware only', async () => {
  const stub = okUpstream();
  const { server, log, port } = await listen({ GET: (request) => handle(request, { fetch: stub.fetch }), OPTIONS });
  try {
    assert.equal(server.address().address, '127.0.0.1');
    const query = `?url=${encodeURIComponent(ARCHIVE)}`;
    const origin = 'http://127.0.0.1:8774';
    const ok = await rawRequest(port, { pathname: `/api/firmware${query}`, headers: { origin } });
    assert.equal(ok.status, 200);
    assert.equal(ok.body.toString(), SREC_TEXT);
    assert.equal(ok.headers['access-control-allow-origin'], origin);
    assert.equal(ok.headers['cache-control'], 'private, no-store');
    assert.equal(ok.headers['content-type'], 'text/plain; charset=us-ascii');
    assert.deepEqual(stub.calls.map((entry) => entry.url), [ARCHIVE_RAW]);
    // Preflight and disallowed origin through the HTTP layer.
    const pre = await rawRequest(port, { method: 'OPTIONS', headers: { origin: 'http://localhost:9', 'access-control-request-method': 'GET' } });
    assert.equal(pre.status, 204);
    assert.equal(pre.headers['access-control-allow-methods'], 'GET');
    const refused = await rawRequest(port, { pathname: `/api/firmware${query}`, headers: { origin: 'https://evil.example' } });
    assert.equal(refused.status, 403);
    // Other paths, methods and Host headers.
    assert.equal((await rawRequest(port, { pathname: '/' })).status, 404);
    assert.equal((await rawRequest(port, { pathname: '/api/other' })).status, 404);
    assert.equal((await rawRequest(port, { pathname: '/api/firmware/' })).status, 404);
    const post = await rawRequest(port, { method: 'POST' });
    assert.equal(post.status, 405);
    assert.equal(post.headers.allow, 'GET, OPTIONS');
    assert.equal((await rawRequest(port, { headers: { host: 'evil.example' } })).status, 421, 'DNS-rebinding guard');
    assert.equal((await rawRequest(port, { pathname: `/api/firmware${query}`, headers: { host: `localhost:${port}`, origin } })).status, 200);
    assert.ok(log.length >= 8 && log.every(([, status]) => Number.isInteger(status)));
    assert.ok(log.every((entry) => entry.length === 3 && !JSON.stringify(entry).includes('multi3s')), 'the log callback never receives an address');
  } finally {
    await new Promise((resolve) => server.close(resolve));
  }
});
