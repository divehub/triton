#!/usr/bin/env python3
"""Serve the NGC WebAssembly emulator web app on loopback (Python standard library only).

    python3 web/serve.py [--port 8770] [--wasm path/to/ngc_wasm.wasm]

Then open http://127.0.0.1:8770. The server binds to 127.0.0.1 only, serves the static files of this directory
with `Cache-Control: no-store` and the correct `application/wasm` type, and serves the engine at
`/pkg/ngc_wasm.wasm` from (first match) --wasm, `web/pkg/` (see build.py) or the newest module in the cargo
target directories. It never serves firmware (the page asks the user for the two SREC files of a TRITON or NEPTUN
release), except for the opt-in loopback development aid --dev-firmware DIR.
"""
import argparse
import http.server
import socket
import sys
from pathlib import Path
from urllib.parse import unquote, urlparse

WEB = Path(__file__).resolve().parent
ROOT = WEB.parent
TYPES = {
    ".html": "text/html; charset=utf-8",
    ".js": "text/javascript; charset=utf-8",
    ".css": "text/css; charset=utf-8",
    ".wasm": "application/wasm",
    ".svg": "image/svg+xml",
    ".txt": "text/plain; charset=utf-8",
}
# The same policy as the source index.html <meta>; the worker script is governed by this header (not by the page).
# connect-src also names loopback dev proxies ("Load from URLs" with ?firmware-proxy=, firmware-url.js). The deployed
# site (GitHub Pages) cannot send headers: deploy/build_site.py adds the configured firmware proxy origin to the
# connect-src of the built index.html <meta> and checks that the rest of the policy is identical to this one.
CSP = ("default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data: blob:; "
       "connect-src 'self' http://127.0.0.1:* http://localhost:*; worker-src 'self'; base-uri 'none'; form-action 'none'")
WASM_NAME = "ngc_wasm.wasm"


def cargo_candidates():
    """Release modules in the cargo target directories (one per work package), newest first."""
    found = []
    for target in (ROOT / "target").glob("*"):
        path = target / "wasm32-unknown-unknown" / "release" / WASM_NAME
        if path.is_file():
            found.append(path)
    return sorted(found, key=lambda path: path.stat().st_mtime, reverse=True)


def resolve_wasm(explicit):
    if explicit:
        path = Path(explicit).resolve()
        return path if path.is_file() else None
    staged = WEB / "pkg" / WASM_NAME
    if staged.is_file():
        return staged
    candidates = cargo_candidates()
    return candidates[0] if candidates else None


FIRMWARE_NAMES = {
    "ngc_main_5.8_TRITON.srec", "ngc_handset_65.3_TRITON.srec",
    "ngc_main_5.8_NEPTUN.srec", "ngc_handset_65.3_NEPTUN.srec",
}


def make_handler(wasm_path, port, firmware_dirs=()):
    allowed_hosts = {f"127.0.0.1:{port}", f"localhost:{port}", f"[::1]:{port}"}

    class Handler(http.server.BaseHTTPRequestHandler):
        server_version = "ngc-web"

        def reply(self, status, body, content_type="text/plain; charset=utf-8", head=False):
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("X-Content-Type-Options", "nosniff")
            self.send_header("Referrer-Policy", "no-referrer")
            self.send_header("Cross-Origin-Resource-Policy", "same-origin")
            self.send_header("Content-Security-Policy", CSP)
            self.end_headers()
            if not head:
                self.wfile.write(body)

        def locate(self, path):
            name = unquote(urlparse(path).path)
            if name in ("", "/"):
                name = "/index.html"
            if name == f"/pkg/{WASM_NAME}":
                return wasm_path
            if name.startswith("/dev-firmware/"):
                # Opt-in development aid (--dev-firmware): exactly the known SREC file names, nothing else.
                leaf = name[len("/dev-firmware/"):]
                if leaf in FIRMWARE_NAMES:
                    for firmware_dir in firmware_dirs:
                        if (firmware_dir / leaf).is_file():
                            return firmware_dir / leaf
                return None
            parts = [part for part in name.split("/") if part]
            if len(parts) != 1 or parts[0].startswith("."):
                return None  # only the files of this directory (no sub-directories, no dot files)
            candidate = WEB / parts[0]
            if candidate.suffix not in TYPES or not candidate.is_file():
                return None
            return candidate

        def serve(self, head):
            if self.headers.get("Host", "") not in allowed_hosts:
                return self.reply(421, b"Unexpected Host header", head=head)
            target = self.locate(self.path)
            if target is None:
                if self.path.split("?")[0] == f"/pkg/{WASM_NAME}":
                    return self.reply(404, b"The engine has not been built: run python3 web/build.py", head=head)
                return self.reply(404, b"Not found", head=head)
            self.reply(200, target.read_bytes(), TYPES.get(target.suffix, "application/octet-stream"), head=head)

        def do_GET(self):
            self.serve(False)

        def do_HEAD(self):
            self.serve(True)

        def log_message(self, *args):
            if "--quiet" not in sys.argv:
                super().log_message(*args)

    return Handler


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--port", type=int, default=8770, help="loopback port (default 8770)")
    parser.add_argument("--wasm", help="serve this module at /pkg/ngc_wasm.wasm instead of the staged or built one")
    parser.add_argument("--quiet", action="store_true", help="do not log requests")
    parser.add_argument("--dev-firmware", metavar="DIR", action="append", default=[],
                        help="DEVELOPMENT AID, off by default: also serve the original SREC files of DIR (the four known TRITON / NEPTUN names, "
                        "nothing else) at /dev-firmware/<name>, which the page loads when opened as /?dev-firmware (TRITON) or "
                        "/?dev-firmware=neptun (for automated browser checks; normal use asks for the files). May be repeated to serve "
                        "several release directories.")
    args = parser.parse_args()
    if not 1 <= args.port <= 65535:
        parser.error("port must be between 1 and 65535")
    firmware_dirs = []
    for directory in args.dev_firmware:
        firmware_dir = Path(directory).resolve()
        if not firmware_dir.is_dir():
            parser.error(f"--dev-firmware: {firmware_dir} is not a directory")
        firmware_dirs.append(firmware_dir)
        found = sorted(name for name in FIRMWARE_NAMES if (firmware_dir / name).is_file())
        print(f"development aid: serving {', '.join(found) or 'no known SREC file'} from {firmware_dir} at /dev-firmware/ (loopback only)")
    with socket.socket() as probe:
        probe.settimeout(0.2)
        if probe.connect_ex(("127.0.0.1", args.port)) == 0:
            parser.error(f"loopback port {args.port} is already in use; choose another with --port")
    wasm = resolve_wasm(args.wasm)
    if wasm:
        print(f"engine: {wasm} ({wasm.stat().st_size} bytes)")
    else:
        print("warning: no ngc_wasm.wasm found; run python3 web/build.py first", file=sys.stderr)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", args.port), make_handler(wasm, args.port, firmware_dirs))
    server.daemon_threads = True
    print(f"NGC WebAssembly emulator: http://127.0.0.1:{args.port}  (Ctrl+C to stop)", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
