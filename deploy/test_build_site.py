#!/usr/bin/env python3
"""Tests of build_site.py, the GitHub Pages assembly (Python standard library only; no network, no firmware).

    python3 deploy/test_build_site.py

Temporary sites are built from copies of the real source files below target/ (ignored by Git) and removed afterwards.
"""
import contextlib
import io
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import build_site  # noqa: E402

SREC = b"S00600004844521B\r\nS1130000285F245F2212226A000424290008237C2A\r\nS9030000FC\r\n"
HAVE_ENGINE = (build_site.WEB / build_site.ENGINE_FILE).is_file()
PROXY = "https://firmware-proxy.example/api/firmware"
NODE = shutil.which("node")


def fresh_web(root):
    """A copy of the real web/ files that the site uses (plus serve.py) under `root`, with a stub engine module."""
    web = root / "web"
    (web / "pkg").mkdir(parents=True)
    for name in (*build_site.SITE_FILES, "serve.py"):
        shutil.copyfile(build_site.WEB / name, web / name)
    (web / build_site.ENGINE_FILE).write_bytes(b"\0asm\x01\0\0\0")
    return web


class TempTree(unittest.TestCase):
    def setUp(self):
        target = build_site.ROOT / "target"
        target.mkdir(exist_ok=True)
        self._tmp = tempfile.TemporaryDirectory(dir=target, prefix="site-test-")
        self.root = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)
        self.web = fresh_web(self.root)


class FirmwareDetection(unittest.TestCase):
    def test_extensions_are_refused_whatever_the_content(self):
        for name in ("a.srec", "A.SREC", "x.s19", "x.s28", "x.s37", "x.mot", "x.bin", "x.hex", "x.elf", "x.apk", "x.zip", "app.js.map", "pkg/x.tar", "x.gz"):
            self.assertIsNotNone(build_site.looks_like_firmware(name, b"harmless"), name)

    def test_content_that_looks_like_an_srec_is_refused(self):
        self.assertIn("starts with S0", build_site.looks_like_firmware("index.html", SREC))
        self.assertIn("starts with S0", build_site.looks_like_firmware("notes.txt", b"\xef\xbb\xbf  \r\n" + SREC))
        self.assertIn("S-record lines", build_site.looks_like_firmware("data.json", b"{}\n" + SREC))
        self.assertIsNotNone(build_site.looks_like_firmware("pkg/ngc_wasm.wasm", SREC))

    def test_ordinary_files_pass(self):
        for name, data in (("app.js", b"import x from './x.js';\n"), ("index.html", b"<!doctype html>"), ("style.css", b"body{}"),
                           ("pkg/ngc_wasm.wasm", b"\0asm\x01\0\0\0" + b"S0" * 10), ("note.txt", b"Some words\nS1 is a name")):
            self.assertIsNone(build_site.looks_like_firmware(name, data), name)

    def test_oversized_and_non_wasm_files_are_refused(self):
        self.assertIn("limit", build_site.looks_like_firmware("big.js", b"x" * (build_site.MAX_FILE_BYTES + 1)))
        self.assertIn("WebAssembly", build_site.looks_like_firmware("pkg/ngc_wasm.wasm", b"not wasm"))


class ProxyAddress(unittest.TestCase):
    def test_empty_means_no_proxy(self):
        for empty in (None, "", "   ", "\n"):
            self.assertIsNone(build_site.normalize_proxy_url(empty))

    def test_accepted_forms_become_the_canonical_endpoint(self):
        canonical = {
            "https://firmware-proxy.example/api/firmware": PROXY,
            "https://firmware-proxy.example": PROXY,
            "https://firmware-proxy.example/": PROXY,
            "  https://FIRMWARE-Proxy.Example/api/firmware\n": PROXY,
            "https://firmware-proxy.example:443/api/firmware": PROXY,
            "https://ngc-proxy.vercel.app": "https://ngc-proxy.vercel.app/api/firmware",
            "https://proxy.example.com:8443": "https://proxy.example.com:8443/api/firmware",
            "https://xn--bcher-kva.example/api/firmware": "https://xn--bcher-kva.example/api/firmware",
        }
        for given, expected in canonical.items():
            self.assertEqual(build_site.normalize_proxy_url(given), expected, given)
            self.assertEqual(build_site.proxy_origin(expected) + "/api/firmware", expected)

    def test_everything_else_is_refused(self):
        for bad in (
            "http://firmware-proxy.example/api/firmware", "firmware-proxy.example", "//firmware-proxy.example/api/firmware", "/api/firmware",
            "ftp://firmware-proxy.example/api/firmware", "javascript:alert(1)", "https://user@firmware-proxy.example/api/firmware",
            "https://user:pw@firmware-proxy.example/api/firmware", "https://firmware-proxy.example/api/firmware?x=1", "https://firmware-proxy.example/api/firmware#x",
            "https://firmware-proxy.example/?x=1", "https://firmware-proxy.example/api", "https://firmware-proxy.example/api/firmware/",
            "https://firmware-proxy.example/other", "https://firmware-proxy.example:0/api/firmware", "https://firmware-proxy.example:99999/api/firmware",
            "https://127.0.0.1/api/firmware", "https://[::1]/api/firmware", "https://192.168.1.5:8443", "https://localhost", "https://localhost:8443/api/firmware",
            "https://app.localhost", "https://singlelabel", "https://exa mple.com", "https://example..com", "https://-bad.example", "https://example.com./api/firmware",
            "https://bücher.example", "https://" + "a" * 300 + ".example",
        ):
            with self.assertRaises(build_site.SiteError, msg=bad):
                build_site.normalize_proxy_url(bad)


class ContentSecurityPolicy(TempTree):
    def test_the_real_configuration_is_consistent(self):
        csp = build_site.check_config()
        self.assertIn("connect-src 'self' http://127.0.0.1:* http://localhost:*;", csp)
        for strict in ("default-src 'none'", "script-src 'self' 'wasm-unsafe-eval'", "style-src 'self'", "worker-src 'self'", "base-uri 'none'", "form-action 'none'"):
            self.assertIn(strict, csp)
        self.assertNotIn("divehub", csp, "the source policy names no remote origin: the built page adds the proxy origin")

    def test_the_policies_must_agree(self):
        build_site.check_config(self.web)
        page = (self.web / "index.html").read_text()
        (self.web / "index.html").write_text(page.replace("connect-src 'self' ", "connect-src 'self' https://elsewhere.example "))
        with self.assertRaisesRegex(build_site.SiteError, "differs between serve.py and the index.html"):
            build_site.check_config(self.web)

    def test_an_unexpected_base_connect_src_is_refused(self):
        for name in ("serve.py", "index.html"):
            path = self.web / name
            path.write_text(path.read_text().replace("connect-src 'self' ", "connect-src 'self' https://elsewhere.example "))
        with self.assertRaisesRegex(build_site.SiteError, "unexpected connect-src"):
            build_site.check_config(self.web)

    def test_the_proxy_origin_is_added_to_connect_src_only(self):
        base = build_site.check_config()
        self.assertEqual(build_site.csp_with_proxy(base, None), base)
        configured = build_site.csp_with_proxy(base, PROXY)
        self.assertEqual(configured, base.replace("connect-src 'self' ", "connect-src 'self' https://firmware-proxy.example "))
        self.assertEqual(build_site.connect_sources(configured), ["'self'", "https://firmware-proxy.example", "http://127.0.0.1:*", "http://localhost:*"])
        with_port = build_site.csp_with_proxy(base, "https://proxy.example.com:8443/api/firmware")
        self.assertIn("connect-src 'self' https://proxy.example.com:8443 http://127.0.0.1:*", with_port)
        self.assertEqual(build_site.csp_with_proxy(configured, PROXY), configured, "adding twice changes nothing")
        # Everything except connect-src is unchanged.
        strip = lambda text: [part for part in text.split("; ") if not part.startswith("connect-src")]  # noqa: E731
        self.assertEqual(strip(configured), strip(base))

    def test_render_index_html_changes_only_the_meta_policy(self):
        source = (self.web / "index.html").read_bytes()
        self.assertEqual(build_site.render_index_html(source, None), source)
        rendered = build_site.render_index_html(source, PROXY)
        self.assertNotEqual(rendered, source)
        before, after = source.decode(), rendered.decode()
        self.assertEqual(before.replace("connect-src 'self' ", "connect-src 'self' https://firmware-proxy.example "), after)
        with self.assertRaisesRegex(build_site.SiteError, "no Content-Security-Policy"):
            build_site.render_index_html(b"<html></html>", PROXY)

    def test_config_js_text(self):
        self.assertIn("FIRMWARE_PROXY_URL = null;", build_site.render_config_js(None).decode())
        self.assertIn(f'FIRMWARE_PROXY_URL = "{PROXY}";', build_site.render_config_js(PROXY).decode())
        self.assertTrue((build_site.WEB / "config.js").read_text().rstrip().endswith("FIRMWARE_PROXY_URL = null;"), "the committed default configures nothing")


class PlanAndOutput(TempTree):
    def plan(self, proxy=None):
        return build_site.plan_files(self.web, proxy)

    def test_the_plan_is_exactly_the_allowlist(self):
        names = [name for name, _ in self.plan()]
        self.assertEqual(sorted(names), sorted([*build_site.SITE_FILES, build_site.ENGINE_FILE]))
        for forbidden in ("serve.py", "build.py", "README.md", "test-ui.mjs", "test-node.mjs", "fake-dom.mjs", "bench-node.mjs", "pkg/build-info.json",
                          "api/firmware.mjs", "vercel.json", "package.json", "test-proxy.mjs", "dev-proxy.mjs", "build_site.py", "build_proxy.py"):
            self.assertNotIn(forbidden, names)
        self.assertFalse([name for name in names if name.endswith((".srec", ".s19", ".bin", ".hex", ".map", ".md", ".py", ".mjs"))])

    def test_a_new_module_must_be_added_to_the_allowlist_on_purpose(self):
        (self.web / "extra.js").write_text("export {};\n")
        with self.assertRaisesRegex(build_site.SiteError, "not on the allowlist.*extra.js"):
            self.plan()

    def test_missing_files_and_links_are_refused(self):
        (self.web / "pkg" / "ngc_wasm.wasm").unlink()
        with self.assertRaisesRegex(build_site.SiteError, "does not exist.*web/build.py"):
            self.plan()
        (self.web / "pkg" / "ngc_wasm.wasm").write_bytes(b"\0asm\x01\0\0\0")
        (self.web / "zip.js").unlink()
        (self.web / "zip.js").symlink_to(self.web / "dom.js")
        with self.assertRaisesRegex(build_site.SiteError, "symbolic link"):
            self.plan()

    def test_firmware_smuggled_into_an_allowed_name_is_refused(self):
        (self.web / "style.css").write_bytes(SREC)
        with self.assertRaisesRegex(build_site.SiteError, "refusing to publish.*style.css"):
            self.plan()

    def test_the_proxy_address_only_changes_config_js_and_the_page_policy(self):
        plain = dict(self.plan())
        configured = dict(self.plan(PROXY))
        changed = sorted(name for name in plain if plain[name] != configured[name])
        self.assertEqual(changed, ["config.js", "index.html"])
        self.assertIn(b"FIRMWARE_PROXY_URL = null;", plain["config.js"])
        self.assertIn(PROXY.encode(), configured["config.js"])
        self.assertNotIn(b"firmware-proxy.example", plain["index.html"])
        self.assertIn(b"https://firmware-proxy.example http://127.0.0.1:*", configured["index.html"])

    def test_imports_must_resolve_inside_the_site(self):
        build_site.check_imports(self.plan())
        build_site.check_imports(self.plan(PROXY))
        (self.web / "entry.js").write_text((self.web / "entry.js").read_text() + "\nimport { x } from './not-published.js';\n")
        with self.assertRaisesRegex(build_site.SiteError, "entry.js refers to ./not-published.js"):
            build_site.check_imports(self.plan())
        (self.web / "entry.js").write_text("export {};\n")
        (self.web / "worker.js").write_text("const url = new URL('./pkg/other.wasm', import.meta.url);\n")
        with self.assertRaisesRegex(build_site.SiteError, "worker.js refers to ./pkg/other.wasm"):
            build_site.check_imports(self.plan())

    def test_assemble_replaces_the_output_and_matches_the_plan_byte_for_byte(self):
        out = build_site.ROOT / "target" / self.root.name / "site"
        out.mkdir(parents=True)
        (out / "stale.srec").write_bytes(SREC)
        (out / "old").mkdir()
        (out / "old" / "x.js").write_text("old")
        plan = self.plan(PROXY)
        build_site.assemble(plan, out)
        names = build_site.verify_output(plan, out)
        self.assertEqual(names, {name for name, _ in plan})
        self.assertFalse((out / "stale.srec").exists() or (out / "old").exists())
        rows = build_site.describe(out, names)
        self.assertTrue(all(len(digest) == 64 for _, _, digest in rows))
        self.assertEqual(build_site.check_built(out, PROXY, self.web), None)
        with self.assertRaisesRegex(build_site.SiteError, "published Content-Security-Policy|unexpected connect-src|config.js"):
            build_site.check_built(out, None, self.web)
        # An extra file appearing afterwards is detected.
        (out / "extra.txt").write_text("x")
        with self.assertRaisesRegex(build_site.SiteError, "extra.*extra.txt"):
            build_site.verify_output(plan, out)

    def test_a_tampered_policy_in_the_output_is_detected(self):
        out = build_site.ROOT / "target" / self.root.name / "site"
        plan = self.plan(PROXY)
        build_site.assemble(plan, out)
        page = (out / "index.html").read_text()
        (out / "index.html").write_text(page.replace("connect-src 'self' ", "connect-src 'self' https://elsewhere.example "))
        with self.assertRaisesRegex(build_site.SiteError, "published Content-Security-Policy"):
            build_site.check_built(out, PROXY, self.web)

    def test_the_output_directory_must_be_below_target(self):
        for bad in (build_site.ROOT, build_site.ROOT / "web", build_site.ROOT.parent, Path("/"), Path(tempfile.gettempdir()), build_site.ROOT / "target"):
            with self.assertRaisesRegex(build_site.SiteError, "must be below"):
                build_site.clean_output(bad)


@unittest.skipUnless(NODE, "needs node")
class PageAgreesWithTheBuild(unittest.TestCase):
    """The address the build writes is exactly one that web/firmware-url.js accepts (and the page refuses the rest)."""

    def accepted_by_page(self, values):
        code = ("import { configuredProxyUrl } from " + json.dumps((build_site.WEB / "firmware-url.js").as_uri())
                + "; console.log(JSON.stringify(process.argv.slice(1).map(configuredProxyUrl)));")
        result = subprocess.run([NODE, "--input-type=module", "-e", code, *values], capture_output=True, text=True, check=True)
        return json.loads(result.stdout)

    def test_canonical_addresses_are_accepted_by_the_page(self):
        built = [build_site.normalize_proxy_url(value) for value in (
            "https://firmware-proxy.example", "https://Firmware-Proxy.example:443/api/firmware", "https://proxy.example.com:8443/", "https://xn--bcher-kva.example")]
        self.assertEqual(self.accepted_by_page(built), built)

    def test_the_generated_config_js_exports_the_address(self):
        with tempfile.TemporaryDirectory(dir=build_site.ROOT / "target" if (build_site.ROOT / "target").is_dir() else None) as scratch:
            for proxy in (None, PROXY):
                path = Path(scratch) / "config.mjs"
                path.write_bytes(build_site.render_config_js(proxy))
                code = "import { FIRMWARE_PROXY_URL } from " + json.dumps(path.as_uri()) + "; console.log(JSON.stringify(FIRMWARE_PROXY_URL));"
                result = subprocess.run([NODE, "--input-type=module", "-e", code], capture_output=True, text=True, check=True)
                self.assertEqual(json.loads(result.stdout), proxy)


@unittest.skipUnless(HAVE_ENGINE, "needs web/pkg/ngc_wasm.wasm (python3 web/build.py)")
class RealRun(unittest.TestCase):
    def run_main(self, *extra):
        target = build_site.ROOT / "target"
        with tempfile.TemporaryDirectory(dir=target, prefix="site-run-") as scratch:
            out = Path(scratch) / "site"
            with contextlib.redirect_stdout(io.StringIO()) as printed:
                self.assertEqual(build_site.main(["--no-build", "--out", str(out), *extra]), 0)
            files = sorted(path.relative_to(out).as_posix() for path in out.rglob("*") if path.is_file())
            self.assertEqual(files, sorted([*build_site.SITE_FILES, build_site.ENGINE_FILE]))
            self.assertEqual((out / "pkg" / "ngc_wasm.wasm").read_bytes(), (build_site.WEB / "pkg" / "ngc_wasm.wasm").read_bytes())
            return printed.getvalue(), (out / "index.html").read_text(), (out / "config.js").read_text()

    def test_main_assembles_the_real_site_without_a_proxy(self):
        printed, page, config = self.run_main("--firmware-proxy-url", "")
        self.assertIn("sha256", printed)
        self.assertIn("pkg/ngc_wasm.wasm", printed)
        self.assertIn("not configured", printed)
        self.assertIn("FIRMWARE_PROXY_URL = null;", config)
        self.assertEqual(page, (build_site.WEB / "index.html").read_text())

    def test_main_assembles_the_real_site_with_a_proxy(self):
        printed, page, config = self.run_main("--firmware-proxy-url", "https://firmware-proxy.example")
        self.assertIn(f"firmware proxy: {PROXY}", printed)
        self.assertIn(f'FIRMWARE_PROXY_URL = "{PROXY}";', config)
        self.assertIn("connect-src 'self' https://firmware-proxy.example http://127.0.0.1:* http://localhost:*;", page)

    def test_a_bad_proxy_address_fails_the_build(self):
        with contextlib.redirect_stderr(io.StringIO()) as errors:
            self.assertEqual(build_site.main(["--no-build", "--firmware-proxy-url", "http://firmware-proxy.example"]), 1)
        self.assertIn("https://", errors.getvalue())


if __name__ == "__main__":
    unittest.main(verbosity=2)
