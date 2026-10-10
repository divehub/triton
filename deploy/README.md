# NGC WebAssembly emulator: publishing (GitHub Pages site and Vercel firmware proxy)

The emulator is published in two independent parts:

* **The static page** (`../web/`) on **GitHub Pages** at **https://triton.divehub.ai**. It is built and deployed by the workflow `../.github/workflows/pages.yml` on every push to `main`. `build_site.py` assembles the exact directory that is published.
* **The firmware proxy** (`api/firmware.mjs`), a **Vercel Function on its own address**, which lets the page load the two original S-record files from URLs. A Pages site cannot run server code, and the sources send no CORS headers, so the proxy is a separate project. `build_proxy.py` assembles the exact directory that is uploaded to Vercel. The proxy is **optional**: without it the page works with chosen or dropped files, and "Load from URLs" says that it is not configured yet.

The firmware is never part of the site and is never stored by the proxy. The page asks the proxy for one file at a time; the proxy fetches it from its original location on demand and passes it through. Nothing in this directory deploys itself; the commands that contact Vercel or change GitHub settings are listed below for you to run.

| File | Role |
| --- | --- |
| `api/firmware.mjs` | The Vercel Function: `GET /api/firmware?url=<address>` and the CORS preflight. Node.js runtime, Web-standard `Request` / `Response`, no dependencies, no logging. |
| `vercel.json` | Settings of the proxy-only Vercel project: framework "Other", no build, the function's `maxDuration`, `nosniff` / `no-referrer` headers, `no-store` for `/api/*`. No static site, no rewrites. |
| `package.json` | Pins Node.js 24.x; declares no dependencies and no scripts (there is no install or build step on Vercel). |
| `build_site.py` | Assembles the **Pages site** `../target/pages-site/` (ignored by Git): an allowlist of the page and its modules, `pkg/ngc_wasm.wasm`, a generated `config.js` and `index.html` with the proxy origin in `connect-src`. Refuses anything outside the allowlist or anything that looks like firmware. |
| `build_proxy.py` | Assembles the **Vercel upload directory** `../target/vercel-proxy/` (ignored by Git): exactly `api/firmware.mjs`, `vercel.json` and `package.json`. Refuses anything else, firmware-like content, imports in the function and routing / build settings in `vercel.json`. |
| `dev-proxy.mjs` | The same function on `127.0.0.1` for local testing without Vercel. |
| `test-proxy.mjs`, `test_build_site.py`, `test_build_proxy.py` | Tests: the proxy and its dev server (stubbed upstream, no network), the Pages assembly (proxy address, CSP, allowlist), the Vercel assembly and `vercel.json`. |

## What the proxy does

`GET /api/firmware?url=<encoded address>` returns the bytes of one S-record file, or a short JSON error.

* **Allowlist** (the only thing that decides what is fetched; the address is matched character for character, nothing is decoded or normalized first):
  * `https://api.multi3s.com/static/<name>.srec`
  * `https://web.archive.org/web/<14 digits>[id_]/https://api.multi3s.com/static/<name>.srec` (fetched in the raw `<timestamp>id_` form)

  `<name>` is 1 to 64 of `A-Za-z0-9_-`. Everything else is `400 upstream_not_allowed` with a reason (`scheme`, `host`, `port`, `userinfo`, `query`, `fragment`, `encoded`, `path`, `archive-form`, ...): other hosts and IP literals, ports, user names, query strings, fragments, `http:`, percent-encoding, other paths, longer names, upper-case schemes or hosts. The page applies the same list first (`../web/firmware-url.js`) and `test-proxy.mjs` checks that both agree on every case.
* **Fetch.** `GET` only, `redirect: 'manual'`, at most 3 redirects and only when each `Location` matches the allowlist again (a Wayback redirect to another capture is followed and normalized; anything else is `502 redirect_not_allowed`). No cookies, no credentials, nothing from the caller forwarded: the upstream sees only `User-Agent: ngc-firmware-proxy/1.0 (...)`, `Accept` and `Accept-Encoding: identity`. One 20 s deadline covers the whole exchange.
* **Size and content.** At most 4 MiB (Vercel's response limit is about 4.5 MB): refused by `Content-Length` first and again while reading, and the body must start with ASCII `S0` (checked as soon as two bytes have arrived), otherwise `502`. The response is buffered (never more than 4 MiB) and then sent, so errors are always clean JSON and never a truncated download.
* **Response.** `200` with `Content-Type: text/plain; charset=us-ascii`, `Cache-Control: private, no-store` (no CDN or browser copy: the deployment never holds or redistributes the firmware), `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `Vary: Origin`, and `X-Firmware-Source` (the address finally fetched). No upstream header is passed on. Errors: `application/json; charset=utf-8`, `{"error": "<code>", "message": "..."}` (never echoing the request), the same protections. Codes: `upstream_not_allowed` 400, `bad_request` 400, `origin_not_allowed` / `origin_required` 403, `method_not_allowed` 405, `upstream_status` / `upstream_too_large` / `not_srec` / `too_many_redirects` / `redirect_not_allowed` / `upstream_unreachable` 502, `upstream_timeout` 504.
* **CORS.** `Access-Control-Allow-Origin` is echoed (with `Vary: Origin`) only for `https://triton.divehub.ai` and `http://localhost[:port]` / `http://127.0.0.1[:port]` (any port, `http` only). The preflight (`OPTIONS`) answers the same way: `GET` only, no credentials, no extra request headers, `Max-Age` 600. A disallowed `Origin` gets `403`. A request without `Origin` is served only when `Sec-Fetch-Site: same-origin`; anything else is `403 origin_required`. The page on Pages is a different origin from the proxy, so its requests carry `Origin: https://triton.divehub.ai` and are echoed. The `github.io` address of the Pages site is **not** in the list: use the custom domain.
* **Privacy.** The function logs nothing (no `console` calls, no storage, no environment). Vercel's own request log records each request's path and query, which includes the address asked for, for the retention period of your plan.

## What it does not do

* **It is not authentication.** The `Origin` / `Sec-Fetch-Site` rule keeps other *websites* from using your proxy from their visitors' browsers (hotlinking) and nothing more: any non-browser client can send any `Origin`. The upstream allowlist is the real guard.
* No rate limiting. Each request costs one function invocation and up to about 2.7 MB of outbound and inbound transfer (the handset image is 2.1 MB). If abuse appears, add a rate-limit rule for `/api/firmware` in the Vercel Firewall (dashboard, project, Firewall).
* It does not verify what it fetches. The Wayback Machine returns whatever it archived; the page's verification (SHA-256, record structure, vector table, board role, known release) decides what is accepted, exactly as for files you choose by hand.
* No caching, no mirror, no copy of the firmware anywhere, by design.

## Threat model and limits

What a caller can make the function do: fetch `https://api.multi3s.com/static/<name>.srec`, or an archived copy of such a file from `web.archive.org` (up to 3 allowlisted redirects), and receive at most 4 MiB of it, if it starts with `S0`. It cannot reach other hosts, ports, paths or private addresses, cannot supply headers, cookies or a body, and cannot read the function's environment (there is none). Redirects cannot lead anywhere outside the list. DNS is the platform's: the two host names are trusted third parties; a compromise of either could serve different bytes, which the page's verification would refuse unless they pass as a known release.

Residual risks: use of your function allowance by someone with a forged `Origin` (bounded by the 4 MiB / 20 s limits and addressable with a Firewall rate limit), and availability of the two upstreams (the Wayback Machine is rate-limited and sometimes slow; the 20 s deadline turns that into `504 upstream_timeout`).

## Local testing (no Vercel, no account)

```sh
node deploy/test-proxy.mjs                 # proxy, allowlist, CORS, redirects, size, SREC check, dev server
python3 deploy/test_build_site.py          # Pages assembly: proxy address, CSP, allowlist, firmware refusal
python3 deploy/test_build_proxy.py         # Vercel assembly and vercel.json rules
node web/test-ui.mjs                       # includes the "Load from URLs" page tests

node deploy/dev-proxy.mjs --port 8775                  # the function on http://127.0.0.1:8775/api/firmware
python3 web/serve.py --port 8774                       # the page
# open http://127.0.0.1:8774/?firmware-proxy=http://127.0.0.1:8775/api/firmware
```

A loopback page (`serve.py`) uses the proxy named by `?firmware-proxy=http://127.0.0.1:<port>/api/firmware`, which is honored only on a loopback page and only for loopback addresses. `?main-url=<address>&handset-url=<address>` pre-fills the fields (it never starts a download). The dev proxy answers only `/api/firmware` and only for Host `127.0.0.1` / `localhost`; it makes real requests to `api.multi3s.com` / `web.archive.org` when the page asks.

## The Pages site

```sh
python3 deploy/build_site.py                                   # runs web/build.py (./cargo), then assembles target/pages-site/
python3 deploy/build_site.py --no-build                        # assemble from the existing web/pkg/ngc_wasm.wasm
python3 deploy/build_site.py --firmware-proxy-url https://<project>.vercel.app/api/firmware
```

Without `--firmware-proxy-url` the script reads the `FIRMWARE_PROXY_URL` environment variable (the workflow sets it from the repository variable of the same name); empty or unset means **no proxy**. An origin alone (`https://<project>.vercel.app`) is accepted and completed with `/api/firmware`. The address must be `https` with a plain DNS host name (no IP address, no `localhost`, no user name, query or fragment).

The result, `target/pages-site/`, contains only: `index.html`, `style.css`, `config.js`, the page modules (`app.js`, `conditions.js`, `deco.js`, `dom.js`, `emulator.js`, `engine.js`, `entry.js`, `faults.js`, `firmware-url.js`, `keys.js`, `lcd.js`, `releases.js`, `replay.js`, `runtime.js`, `sensors.js`, `storage.js`, `worker-client.js`, `worker.js`, `zip.js`, and the dive game's `game.css` and `game*.js`), the four recorded MAV clips of the dive game (`mav-oxygen-onset.wav`, `mav-oxygen-loop.wav`, `mav-diluent-onset.wav`, `mav-diluent-loop.wav`: the only audio, RIFF/WAVE files of at most 256 KiB each; source and license in `licenses/README.md`) and `pkg/ngc_wasm.wasm`. Never tests, `serve.py`, `build.py`, READMEs, the proxy sources, firmware, scratch files or source maps. The script prints every file with its size and SHA-256 and fails when a file outside the allowlist would be copied, when an import does not resolve inside the result, when anything looks like firmware (`.srec` / `.s19` / `.bin` / `.hex` and similar, content starting with `S0`, S-record lines), or when the Content-Security-Policy of the page and `serve.py` differ.

What the proxy address changes in the published files, and nothing else:

* `config.js` becomes `export const FIRMWARE_PROXY_URL = "https://<host>/api/firmware";` (or `null`). The committed `../web/config.js` is `null`.
* `index.html` gets the proxy **origin** added to `connect-src` of its Content-Security-Policy `<meta>` (right after `'self'`; the loopback entries stay for `?firmware-proxy=` on local pages). GitHub Pages cannot send response headers, so the `<meta>` tag is the only policy; the worker script, which a header would cover, has none there.
* The page never falls back to a same-origin `/api/firmware` (there is none on Pages), and ignores `?firmware-proxy=` on every page that is not on loopback.

The assets are not content-hashed and GitHub Pages serves them with `Cache-Control: max-age=600`, so for up to ten minutes after a deployment a browser can combine a cached `ngc_wasm.wasm` with a new `engine.js` (or the reverse); a hard reload fixes it.

### GitHub setup (done once)

```sh
gh api repos/divehub/triton/pages                      # the current Pages configuration (GET only)
```

Pages is configured with `build_type: workflow` (the site is built and deployed by `pages.yml`, not from a branch) and the custom domain `triton.divehub.ai` (domain verified, certificate approved, HTTPS enforced when this was written). The DNS side is a `CNAME` record `triton.divehub.ai` -> `divehub.github.io` at whoever hosts `divehub.ai`. No `CNAME` file is added to the site: with a workflow build the custom domain comes from the Pages settings and a `CNAME` file in the artifact is ignored. The workflow needs the repository's Actions to be enabled and the `github-pages` environment to allow deployments from `main` (the default).

### The proxy address

```sh
gh variable set FIRMWARE_PROXY_URL --repo divehub/triton --body https://<project>.vercel.app/api/firmware
gh workflow run pages.yml --repo divehub/triton        # rebuild and redeploy with it (a push to main does the same)
gh variable delete FIRMWARE_PROXY_URL --repo divehub/triton   # back to "not configured"
```

The workflow passes `vars.FIRMWARE_PROXY_URL` to `build_site.py`; a bad value fails the build before anything is deployed.

## The Vercel proxy project (commands for you to run)

```sh
python3 deploy/build_proxy.py                      # -> target/vercel-proxy/ (api/firmware.mjs, vercel.json, package.json)
cd target/vercel-proxy

vercel login                                       # once per machine
vercel link                                        # creates .vercel/ here; create a new project (for example "triton-firmware-proxy"), framework "Other", no build command
vercel deploy                                      # a preview deployment; prints its *.vercel.app address
vercel deploy --prod                               # the production deployment; prints the production address
```

The proxy's address is `https://<production host>/api/firmware`; that is the value of `FIRMWARE_PROXY_URL` (the production host is printed by `vercel deploy --prod`, and listed under the project's Domains in the dashboard). A custom domain for the proxy is optional (`vercel domains add <domain> <project-name>`, `vercel domains verify <domain>`).

Check it before setting the variable (a refusal is the proof that the proxy and its CORS rule work; no firmware is fetched):

```sh
curl -si -H 'Origin: https://triton.divehub.ai' 'https://<production host>/api/firmware?url=https%3A%2F%2Fexample.com%2Fx.srec'
# expect: HTTP 400, {"error":"upstream_not_allowed",...} and access-control-allow-origin: https://triton.divehub.ai
```

Notes before you deploy:

* `target/vercel-proxy/.vercel/` (the project link) is kept when `build_proxy.py` runs again.
* Vercel's Deployment Protection can ask for a Vercel login on deployments. The proxy must be reachable by browsers without one: check Project, Settings, Deployment Protection for the **production** deployment (a preview protected by default is fine). A protected endpoint answers `401` without CORS headers and the page reports that the proxy could not be reached.
* Vercel's Hobby plan is for non-commercial use; check which plan the company account is on.
* The deployment is not expected to serve any firmware at any path: the upload directory contains none, and the proxy responses are never cached.

## What was checked offline, and what was not

Checked here: the unit and integration tests above; that the Pages directory and the upload directory contain exactly their allowlists; that every proxy address the build accepts is accepted by the page and that the published policy equals `serve.py`'s plus the proxy origin; `vercel.json` against the schema compiled into Vercel CLI 60.0.1 and the `@vercel/node` 15.0.0 handler shape (`api/firmware.mjs` with `export function GET(request)` / `OPTIONS(request)`) when the function was first written. The proxy-only `vercel.json` (no CSP, no static headers) was not run through the CLI again.

Not checkable offline: that Vercel's current platform accepts Node.js `24.x` in `engines`, the exact `maxDuration` ceiling of your plan (30 s is below every plan's limit), that `null` for `framework` / `buildCommand` / `installCommand` selects "Other" with no build and no static output (with no `scripts`, no dependencies and only `api/`, auto-detection gives the same result), the precedence between header rules that match the same path (the `/api/*` rule repeats the function's own `Cache-Control` value so either way wins identically), the real response of `api.multi3s.com` to a request from Vercel's network, and the GitHub Pages deployment itself (first run of the workflow). After the first deployment, set the variable, load both example addresses on the production page and confirm the B1 battery screen.
