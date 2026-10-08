#!/usr/bin/env node
// Local stand-in for Vercel: serves api/firmware.mjs (the very same GET / OPTIONS exports) on 127.0.0.1.
//
//   node deploy/dev-proxy.mjs [--port 8775] [--quiet]
//
// Then serve the page (python3 web/serve.py --port 8774) and open
//   http://127.0.0.1:8774/?firmware-proxy=http://127.0.0.1:8775/api/firmware
// The proxy fetches the named file from api.multi3s.com / web.archive.org on request, keeps nothing and sends it to
// the page. Node built-ins only; binds to loopback; answers only /api/firmware and only for Host 127.0.0.1 / localhost
// (a guard against DNS rebinding). The CORS rules are the production ones: pages on any localhost / 127.0.0.1 port are
// allowed.

import http from 'node:http';
import { pathToFileURL } from 'node:url';
import { GET, OPTIONS } from './api/firmware.mjs';

export const ROUTE = '/api/firmware';

function reply(res, status, body, extra = {}) {
  const payload = Buffer.from(body);
  res.writeHead(status, { 'Content-Type': 'application/json; charset=utf-8', 'Content-Length': payload.length, 'Cache-Control': 'no-store', 'X-Content-Type-Options': 'nosniff', ...extra });
  res.end(payload);
}

/**
 * The server (not yet listening). `handlers` defaults to the production exports; the tests pass handlers that use a
 * stubbed upstream fetch. `onRequest(method, status, bytes)` is called after each answer (no addresses).
 */
export function createDevServer({ handlers = { GET, OPTIONS }, onRequest = () => {} } = {}) {
  const server = http.createServer(async (req, res) => {
    const done = (status, bytes) => onRequest(req.method, status, bytes);
    try {
      const port = server.address().port;
      const host = req.headers.host || '';
      if (host !== `127.0.0.1:${port}` && host !== `localhost:${port}`) {
        reply(res, 421, JSON.stringify({ error: 'misdirected', message: 'Unexpected Host header.' }));
        return done(421, 0);
      }
      const url = new URL(req.url, `http://${host}`);
      if (url.pathname !== ROUTE) {
        reply(res, 404, JSON.stringify({ error: 'not_found', message: `Only ${ROUTE} is served.` }));
        return done(404, 0);
      }
      const handler = handlers[req.method];
      if (!handler) {
        reply(res, 405, JSON.stringify({ error: 'method_not_allowed', message: 'Use GET.' }), { Allow: 'GET, OPTIONS' });
        return done(405, 0);
      }
      const response = await handler(new Request(url, { method: req.method, headers: req.headers }));
      const body = Buffer.from(await response.arrayBuffer());
      res.writeHead(response.status, Object.fromEntries(response.headers));
      res.end(body);
      return done(response.status, body.length);
    } catch (_) {
      if (!res.headersSent) reply(res, 500, JSON.stringify({ error: 'internal_error', message: 'The dev proxy failed unexpectedly.' }));
      else res.end();
      return done(500, 0);
    }
  });
  return server;
}

function main(argv) {
  let port = 8775;
  let quiet = false;
  for (let i = 0; i < argv.length; i += 1) {
    if (argv[i] === '--port') port = Number(argv[++i]);
    else if (argv[i] === '--quiet') quiet = true;
    else {
      console.error(`unknown argument ${argv[i]}\nusage: node dev-proxy.mjs [--port N] [--quiet]`);
      process.exit(2);
    }
  }
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    console.error('--port must be an integer from 1 to 65535');
    process.exit(2);
  }
  const server = createDevServer({ onRequest: quiet ? undefined : (method, status, bytes) => console.log(`${method} ${ROUTE} -> ${status}${bytes ? ` (${bytes} bytes)` : ''}`) });
  server.on('error', (error) => {
    console.error(error.code === 'EADDRINUSE' ? `loopback port ${port} is already in use; choose another with --port` : `server error: ${error.message}`);
    process.exit(1);
  });
  server.listen(port, '127.0.0.1', () => {
    console.log(`NGC firmware dev proxy: http://127.0.0.1:${port}${ROUTE}  (Ctrl+C to stop)`);
    console.log(`page: python3 web/serve.py --port <page port>, then open http://127.0.0.1:<page port>/?firmware-proxy=http://127.0.0.1:${port}${ROUTE}`);
  });
  const stop = () => server.close(() => process.exit(0));
  process.on('SIGINT', stop);
  process.on('SIGTERM', stop);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main(process.argv.slice(2));
