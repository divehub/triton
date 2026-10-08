// Build-time configuration of the page. This committed default configures nothing: "Load from URLs" is then
// unavailable (no firmware proxy), and choosing or dropping the two SREC files works as always.
//
// The GitHub Pages build (deploy/build_site.py) does not use this file: it publishes a generated config.js with the
// address from the FIRMWARE_PROXY_URL repository variable (for example https://<project>.vercel.app/api/firmware, the
// Vercel function of deploy/). The address is validated by firmware-url.js (configuredProxyUrl) and its origin is
// added to the connect-src of the published index.html. See deploy/README.md.
export const FIRMWARE_PROXY_URL = null;
