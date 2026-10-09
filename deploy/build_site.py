#!/usr/bin/env python3
"""Assemble the static site that GitHub Pages publishes (Python standard library only).

    python3 deploy/build_site.py                     # build the engine with web/build.py, then assemble
    python3 deploy/build_site.py --no-build          # assemble from the already built web/pkg/ngc_wasm.wasm
    python3 deploy/build_site.py --out DIR           # another directory below target/
    python3 deploy/build_site.py --firmware-proxy-url https://<host>/api/firmware

The result is `target/pages-site/` (ignored by Git): the page and its modules, the engine module `pkg/ngc_wasm.wasm`
and a generated `config.js`, and nothing else: no tests, `serve.py`, `build.py`, README files, scratch files, source
maps, proxy sources or firmware. Every file must be on the allowlist below, every relative import must resolve inside
the result, and nothing may look like firmware (an SREC / binary extension, or content that starts with S0 or holds
S-record lines). The script prints each file with its size and SHA-256 and exits non-zero on any violation.

The firmware proxy (deploy/api/firmware.mjs) is hosted elsewhere (Vercel, see deploy/README.md), so the page needs
its address at build time. It comes from `--firmware-proxy-url`, else from the FIRMWARE_PROXY_URL environment variable
(the Pages workflow sets that from the repository variable of the same name). An origin alone
(`https://project.vercel.app`) is accepted and completed with `/api/firmware`. With no value there is no proxy: the
generated config.js says `null` and "Load from URLs" is unavailable on the page (it never falls back to a same-origin
/api/firmware). With a value, config.js carries the address and the origin is added to `connect-src` of the Content-
Security-Policy `<meta>` in the published index.html; the rest of the policy is checked to be identical to serve.py's.

It never contacts any server.
"""
import argparse
import ast
import hashlib
import ipaddress
import json
import os
import posixpath
import re
import shutil
import subprocess
import sys
from pathlib import Path
from urllib.parse import urlsplit

DEPLOY = Path(__file__).resolve().parent
ROOT = DEPLOY.parent
WEB = ROOT / "web"
DEFAULT_OUT = ROOT / "target" / "pages-site"

# Files taken from web/ (the page and its modules; the engine module is added from web/pkg/). config.js and index.html
# are published in a generated / adapted form.
SITE_FILES = (
    "index.html", "style.css", "game.css", "config.js", "app.js", "conditions.js", "deco.js", "dom.js", "emulator.js", "engine.js",
    "entry.js", "faults.js", "firmware-url.js", "game.js", "game-gas.js", "game-logic.js", "game-water.js", "keys.js", "lcd.js", "releases.js", "replay.js",
    "runtime.js", "sensors.js", "storage.js", "worker-client.js", "worker.js", "zip.js",
)
ENGINE_FILE = "pkg/ngc_wasm.wasm"
PROXY_PATH = "/api/firmware"

# Anything with these extensions is refused wherever it comes from (firmware, images of it, archives of it).
FIRMWARE_EXTENSIONS = {".srec", ".s19", ".s28", ".s37", ".mot", ".bin", ".hex", ".elf", ".apk", ".aab", ".zip", ".tar", ".gz", ".map"}
TEXT_EXTENSIONS = {".html", ".js", ".mjs", ".css", ".json", ".txt"}
SREC_LINE = re.compile(rb"^S[0-9][0-9A-Fa-f]{2}[0-9A-Fa-f]{4,}\r?$", re.MULTILINE)
WASM_MAGIC = b"\0asm"
MAX_FILE_BYTES = 8 * 1024 * 1024
BASE_CONNECT_SRC = ("'self'", "http://127.0.0.1:*", "http://localhost:*")
HOST_RE = re.compile(r"^[a-z0-9](?:[a-z0-9-]*[a-z0-9])?(?:\.[a-z0-9](?:[a-z0-9-]*[a-z0-9])?)+$")


class SiteError(Exception):
    """A violation of the publishing rules."""


def looks_like_firmware(name, data):
    """A reason why (name, content) must not be published as site content, or None."""
    suffix = Path(name).suffix.lower()
    if suffix in FIRMWARE_EXTENSIONS:
        return f"{name}: extension {suffix} is never published"
    if len(data) > MAX_FILE_BYTES:
        return f"{name}: {len(data)} bytes is larger than the {MAX_FILE_BYTES} byte limit for site files"
    head = data.lstrip(b"\xef\xbb\xbf \t\r\n")[:2]
    if head == b"S0":
        return f"{name}: content starts with S0 (an S-record file)"
    if suffix in TEXT_EXTENSIONS and len(SREC_LINE.findall(data)) >= 2:
        return f"{name}: contains S-record lines"
    if suffix == ".wasm" and not data.startswith(WASM_MAGIC):
        return f"{name}: not a WebAssembly module"
    return None


# ---- the firmware proxy address ------------------------------------------------------------------------------

def normalize_proxy_url(value):
    """The canonical `https://<host>[:port]/api/firmware` of a configured value, or None when `value` is empty.

    Accepts the full endpoint or just the origin (optionally with a trailing slash). Refuses anything else: other
    schemes, user names, queries, fragments, other paths, IP addresses, `localhost` and single-label host names.
    The result is exactly what web/firmware-url.js `configuredProxyUrl` accepts.
    """
    text = (value or "").strip()
    if not text:
        return None
    if len(text) > 256 or not re.fullmatch(r"[\x21-\x7e]+", text):
        raise SiteError("the firmware proxy address must be a short ASCII https address without spaces")
    try:
        parts = urlsplit(text)
        port = parts.port
    except ValueError as error:
        raise SiteError(f"the firmware proxy address is not valid: {error}") from None
    if parts.scheme != "https":
        raise SiteError("the firmware proxy address must start with https://")
    if parts.username is not None or parts.password is not None or "@" in parts.netloc:
        raise SiteError("the firmware proxy address must not contain a user name or password")
    if parts.query or parts.fragment or "?" in text or "#" in text:
        raise SiteError("the firmware proxy address must not contain a query or a fragment")
    if parts.path not in ("", "/", PROXY_PATH):
        raise SiteError(f"the firmware proxy address must be an origin or end in {PROXY_PATH} (got path {parts.path!r})")
    host = (parts.hostname or "").lower()
    if not host or not HOST_RE.match(host):
        raise SiteError("the firmware proxy address needs a plain DNS host name (ASCII letters, digits, '-' and dots)")
    try:
        ipaddress.ip_address(host)
    except ValueError:
        pass
    else:
        raise SiteError("the firmware proxy address must use a host name, not an IP address")
    if host == "localhost" or host.endswith(".localhost"):
        raise SiteError("the firmware proxy address must not be localhost (loopback dev proxies use ?firmware-proxy= instead)")
    if port is not None and not 1 <= port <= 65535:
        raise SiteError("the firmware proxy address has an invalid port")
    shown_port = f":{port}" if port not in (None, 443) else ""
    return f"https://{host}{shown_port}{PROXY_PATH}"


def proxy_origin(proxy_url):
    return proxy_url[: -len(PROXY_PATH)]


def render_config_js(proxy_url):
    value = json.dumps(proxy_url) if proxy_url else "null"
    return (
        "// Generated by deploy/build_site.py for the published site. Do not edit; web/config.js is the committed default.\n"
        "// The firmware proxy address comes from the FIRMWARE_PROXY_URL repository variable (null = no proxy: \"Load from URLs\"\n"
        "// is unavailable).\n"
        f"export const FIRMWARE_PROXY_URL = {value};\n"
    ).encode("utf-8")


# ---- the Content-Security-Policy -----------------------------------------------------------------------------

CSP_META = re.compile(r'(http-equiv="Content-Security-Policy"\s+content=")([^"]+)(")')


def read_csp_from_serve(serve_py):
    """The CSP string of serve.py, read with `ast` (the file is not imported or run)."""
    tree = ast.parse(serve_py.read_text(encoding="utf-8"))
    for node in tree.body:
        if isinstance(node, ast.Assign) and any(isinstance(target, ast.Name) and target.id == "CSP" for target in node.targets):
            return ast.literal_eval(node.value)
    raise SiteError("serve.py has no CSP constant")


def read_csp_from_html(index_html):
    match = CSP_META.search(index_html.read_text(encoding="utf-8"))
    if not match:
        raise SiteError("index.html has no Content-Security-Policy <meta>")
    return match.group(2)


def connect_sources(csp):
    found = re.findall(r"(?:^|;)\s*connect-src ([^;]+)", csp)
    if len(found) != 1:
        raise SiteError("the Content-Security-Policy must have exactly one connect-src")
    return found[0].split()


def check_config(web_dir=WEB):
    """serve.py and the source index.html <meta> must carry the same CSP, and connect-src must be the known base."""
    served = read_csp_from_serve(web_dir / "serve.py")
    page = read_csp_from_html(web_dir / "index.html")
    if served != page:
        raise SiteError("the Content-Security-Policy differs between serve.py and the index.html <meta>:\n"
                        f"  serve.py:   {served}\n  index.html: {page}")
    if tuple(connect_sources(served)) != BASE_CONNECT_SRC:
        raise SiteError(f"unexpected connect-src in the CSP: {' '.join(connect_sources(served))}")
    return served


def csp_with_proxy(csp, proxy_url):
    """The page policy with the proxy origin added to connect-src (right after 'self'); unchanged without a proxy."""
    if not proxy_url:
        return csp
    sources = connect_sources(csp)
    origin = proxy_origin(proxy_url)
    if origin in sources:
        return csp
    updated = " ".join([sources[0], origin, *sources[1:]])
    return csp.replace("connect-src " + " ".join(sources), "connect-src " + updated, 1)


def render_index_html(source_html, proxy_url):
    text = source_html.decode("utf-8")
    match = CSP_META.search(text)
    if not match:
        raise SiteError("index.html has no Content-Security-Policy <meta>")
    updated = csp_with_proxy(match.group(2), proxy_url)
    return (text[: match.start(2)] + updated + text[match.end(2):]).encode("utf-8")


# ---- the plan ------------------------------------------------------------------------------------------------

def plan_files(web_dir=WEB, proxy_url=None):
    """[(published path, bytes)] after checking the allowlist; raises SiteError on any violation."""
    unlisted = sorted(path.name for path in web_dir.glob("*.js") if path.name not in SITE_FILES)
    if unlisted:
        raise SiteError(f"web/ has JavaScript modules that are not on the allowlist (add them to SITE_FILES in build_site.py "
                        f"if the page needs them): {', '.join(unlisted)}")
    plan = []
    for name in (*SITE_FILES, ENGINE_FILE):
        source = web_dir / name
        if not source.is_file():
            hint = " (run web/build.py, or drop --no-build)" if name == ENGINE_FILE else ""
            raise SiteError(f"{name}: {source} does not exist{hint}")
        if source.is_symlink():
            raise SiteError(f"{name}: {source} is a symbolic link")
        data = source.read_bytes()
        reason = looks_like_firmware(name, data)
        if reason:
            raise SiteError(f"refusing to publish: {reason}")
        if name == "config.js":
            data = render_config_js(proxy_url)
        elif name == "index.html":
            data = render_index_html(data, proxy_url)
        plan.append((name, data))
    return plan


def check_imports(plan):
    """Every relative import, worker URL, script and stylesheet reference must resolve to a published file."""
    published = {name for name, _ in plan}
    problems = []
    for name, data in plan:
        if Path(name).suffix not in {".js", ".html", ".css"}:
            continue
        text = data.decode("utf-8")
        references = re.findall(r"""\bfrom\s+['"](\.[^'"]+)['"]""", text)
        references += re.findall(r"""\bimport\(\s*['"](\.[^'"]+)['"]""", text)
        references += re.findall(r"""new URL\(\s*['"](\.[^'"]+)['"]\s*,\s*import\.meta\.url""", text)
        if name.endswith(".html"):
            # Values with a scheme (data: icons, https: links) are not site files.
            references += re.findall(r'<script[^>]*\ssrc="([^":]+)"', text)
            references += re.findall(r'<link[^>]*\shref="([^":]+)"', text)
        for reference in references:
            target = posixpath.normpath(posixpath.join(posixpath.dirname(name), reference))
            if target not in published:
                problems.append(f"{name} refers to {reference}, which is not published")
    if problems:
        raise SiteError("unresolved references:\n  " + "\n  ".join(problems))


def clean_output(out, keep=()):
    """Empties `out` except the entries named in `keep`; refuses directories that are not below <repository>/target/."""
    target_root = (ROOT / "target").resolve()
    resolved = out.resolve()
    if resolved == target_root or target_root not in resolved.parents:
        raise SiteError(f"the output directory must be below {target_root} (got {resolved})")
    out.mkdir(parents=True, exist_ok=True)
    for entry in out.iterdir():
        if entry.name in keep:
            continue
        if entry.is_dir() and not entry.is_symlink():
            shutil.rmtree(entry)
        else:
            entry.unlink()


def assemble(plan, out):
    clean_output(out)
    for name, data in plan:
        destination = out / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(data)


def verify_output(plan, out):
    """The output holds exactly the planned files, byte for byte, and nothing else."""
    expected = {name for name, _ in plan}
    found = {path.relative_to(out).as_posix() for path in out.rglob("*") if path.is_file()}
    if found != expected:
        raise SiteError(f"the output differs from the plan: extra {sorted(found - expected)}, missing {sorted(expected - found)}")
    for name, data in plan:
        if (out / name).read_bytes() != data:
            raise SiteError(f"{name} was not written byte for byte")
        reason = looks_like_firmware(name, data)
        if reason:
            raise SiteError(f"refusing to publish: {reason}")
    return expected


def check_built(out, proxy_url, web_dir=WEB):
    """The published page policy is serve.py's policy plus (at most) the proxy origin in connect-src, and config.js agrees."""
    built = read_csp_from_html(out / "index.html")
    expected = csp_with_proxy(read_csp_from_serve(web_dir / "serve.py"), proxy_url)
    if built != expected:
        raise SiteError("the published Content-Security-Policy is not serve.py's policy plus the proxy origin:\n"
                        f"  published: {built}\n  expected:  {expected}")
    sources = connect_sources(built)
    extra = [source for source in sources if source not in BASE_CONNECT_SRC]
    if extra != ([proxy_origin(proxy_url)] if proxy_url else []):
        raise SiteError(f"unexpected connect-src entries in the published page: {extra}")
    config = (out / "config.js").read_text(encoding="utf-8")
    if f"FIRMWARE_PROXY_URL = {json.dumps(proxy_url) if proxy_url else 'null'};" not in config:
        raise SiteError("the published config.js does not carry the firmware proxy address")


def describe(out, names):
    rows = []
    for name in sorted(names):
        data = (out / name).read_bytes()
        rows.append((name, len(data), hashlib.sha256(data).hexdigest()))
    return rows


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", default=str(DEFAULT_OUT), help="output directory below target/ (default: target/pages-site)")
    parser.add_argument("--no-build", action="store_true", help="do not run web/build.py; use the existing web/pkg/ngc_wasm.wasm")
    parser.add_argument("--firmware-proxy-url", default=os.environ.get("FIRMWARE_PROXY_URL", ""),
                        help="address of the firmware proxy (default: $FIRMWARE_PROXY_URL; empty = no proxy)")
    args = parser.parse_args(argv)
    out = Path(args.out)
    if not out.is_absolute():
        out = Path.cwd() / out
    try:
        proxy_url = normalize_proxy_url(args.firmware_proxy_url)
        check_config()
        if not args.no_build:
            print("+ python3 web/build.py", flush=True)
            result = subprocess.run([sys.executable, str(WEB / "build.py")])
            if result.returncode != 0:
                raise SiteError(f"web/build.py failed with exit status {result.returncode}")
        plan = plan_files(WEB, proxy_url)
        check_imports(plan)
        assemble(plan, out)
        names = verify_output(plan, out)
        check_built(out, proxy_url)
    except SiteError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    rows = describe(out, names)
    width = max(len(name) for name, _, _ in rows)
    print(f"assembled {out} ({len(rows)} files, {sum(size for _, size, _ in rows)} bytes):")
    for name, size, digest in rows:
        print(f"  {name:<{width}}  {size:>9}  sha256 {digest}")
    csp = read_csp_from_html(out / "index.html")
    print(f"firmware proxy: {proxy_url or 'not configured (Load from URLs is unavailable)'}")
    print(f"connect-src: {' '.join(connect_sources(csp))}")
    print("Content-Security-Policy is consistent with serve.py; no firmware-like file found.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
