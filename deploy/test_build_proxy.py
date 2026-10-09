#!/usr/bin/env python3
"""Tests of build_proxy.py, the Vercel upload directory (Python standard library only; no network, no Vercel).

    python3 deploy/test_build_proxy.py

Temporary trees are copies of the real deploy/ files below target/ (ignored by Git) and removed afterwards.
"""
import contextlib
import io
import json
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import build_proxy  # noqa: E402
import build_site  # noqa: E402

SREC = b"S00600004844521B\r\nS1130000285F245F2212226A000424290008237C2A\r\nS9030000FC\r\n"


class TempTree(unittest.TestCase):
    def setUp(self):
        target = build_site.ROOT / "target"
        target.mkdir(exist_ok=True)
        self._tmp = tempfile.TemporaryDirectory(dir=target, prefix="proxy-test-")
        self.root = Path(self._tmp.name)
        self.addCleanup(self._tmp.cleanup)
        self.deploy = self.root / "deploy"
        (self.deploy / "api").mkdir(parents=True)
        for name in build_proxy.PROXY_FILES:
            shutil.copyfile(build_proxy.DEPLOY / name, self.deploy / name)


class Configuration(TempTree):
    def test_the_real_configuration_is_accepted(self):
        build_proxy.check_config()

    def test_vercel_json_hosts_only_the_function(self):
        config = json.loads((build_proxy.DEPLOY / "vercel.json").read_text())
        self.assertEqual(sorted(config["functions"]), ["api/firmware.mjs"])
        self.assertGreaterEqual(config["functions"]["api/firmware.mjs"]["maxDuration"], 21, "longer than the proxy's 20 s upstream deadline")
        by_source = {rule["source"]: {h["key"]: h["value"] for h in rule["headers"]} for rule in config["headers"]}
        self.assertEqual(by_source["/(.*)"], {"X-Content-Type-Options": "nosniff", "Referrer-Policy": "no-referrer"})
        self.assertEqual(by_source["/api/(.*)"], {"Cache-Control": "private, no-store"})
        self.assertFalse([key for key in build_proxy.FORBIDDEN_VERCEL_KEYS if key in config])
        text = (build_proxy.DEPLOY / "vercel.json").read_text()
        self.assertNotIn("Content-Security-Policy", text, "the page (and its CSP) lives on GitHub Pages")
        self.assertEqual(json.loads((build_proxy.DEPLOY / "package.json").read_text())["engines"], {"node": "24.x"})

    def test_forbidden_vercel_settings_are_refused(self):
        original = json.loads((self.deploy / "vercel.json").read_text())
        for key, value in (("rewrites", [{"source": "/(.*)", "destination": "/index.html"}]), ("redirects", []), ("routes", []), ("builds", [])):
            config = dict(original, **{key: value})
            (self.deploy / "vercel.json").write_text(json.dumps(config))
            with self.assertRaisesRegex(build_site.SiteError, f"must not set {key}"):
                build_proxy.check_config(self.deploy)
        config = json.loads(json.dumps(original))
        config["headers"].append({"source": "/api/(.*)", "headers": [{"key": "Access-Control-Allow-Origin", "value": "*"}]})
        (self.deploy / "vercel.json").write_text(json.dumps(config))
        with self.assertRaisesRegex(build_site.SiteError, "sets its own CORS headers"):
            build_proxy.check_config(self.deploy)

    def test_the_function_entry_and_the_manifest_are_checked(self):
        config = json.loads((self.deploy / "vercel.json").read_text())
        del config["functions"]
        (self.deploy / "vercel.json").write_text(json.dumps(config))
        with self.assertRaisesRegex(build_site.SiteError, "no functions entry"):
            build_proxy.check_config(self.deploy)
        config["functions"] = {"api/firmware.mjs": {"maxDuration": 10}}
        (self.deploy / "vercel.json").write_text(json.dumps(config))
        with self.assertRaisesRegex(build_site.SiteError, "maxDuration"):
            build_proxy.check_config(self.deploy)
        shutil.copyfile(build_proxy.DEPLOY / "vercel.json", self.deploy / "vercel.json")
        manifest = json.loads((self.deploy / "package.json").read_text())
        manifest["dependencies"] = {"left-pad": "1.0.0"}
        (self.deploy / "package.json").write_text(json.dumps(manifest))
        with self.assertRaisesRegex(build_site.SiteError, "must not declare dependencies"):
            build_proxy.check_config(self.deploy)

    def test_the_function_must_not_import_anything(self):
        path = self.deploy / "api" / "firmware.mjs"
        path.write_text("import left from 'left-pad';\n" + path.read_text())
        with self.assertRaisesRegex(build_site.SiteError, "must not import"):
            build_proxy.check_config(self.deploy)
        path.write_text("const x = require('left-pad');\n")
        with self.assertRaisesRegex(build_site.SiteError, "must not import"):
            build_proxy.check_config(self.deploy)


class PlanAndOutput(TempTree):
    def test_the_plan_is_exactly_the_allowlist(self):
        self.assertEqual([name for name, _ in build_proxy.plan_files(self.deploy)], ["api/firmware.mjs", "vercel.json", "package.json"])

    def test_missing_links_and_firmware_are_refused(self):
        (self.deploy / "package.json").write_bytes(SREC)
        with self.assertRaisesRegex(build_site.SiteError, "refusing to upload.*package.json"):
            build_proxy.plan_files(self.deploy)
        (self.deploy / "package.json").unlink()
        with self.assertRaisesRegex(build_site.SiteError, "does not exist"):
            build_proxy.plan_files(self.deploy)
        (self.deploy / "package.json").symlink_to(self.deploy / "vercel.json")
        with self.assertRaisesRegex(build_site.SiteError, "symbolic link"):
            build_proxy.plan_files(self.deploy)

    def test_assemble_replaces_the_output_keeps_the_vercel_link_and_matches_the_plan(self):
        out = build_site.ROOT / "target" / self.root.name / "proxy"
        out.mkdir(parents=True)
        (out / ".vercel").mkdir()
        (out / ".vercel" / "project.json").write_text("{}")
        (out / "stale.srec").write_bytes(SREC)
        (out / "index.html").write_text("<html></html>")
        (out / "pkg").mkdir()
        (out / "pkg" / "ngc_wasm.wasm").write_bytes(b"\0asm\x01\0\0\0")
        plan = build_proxy.plan_files(self.deploy)
        build_proxy.assemble(plan, out)
        names = build_proxy.verify_output(plan, out)
        self.assertEqual(names, {"api/firmware.mjs", "vercel.json", "package.json"})
        self.assertTrue((out / ".vercel" / "project.json").is_file(), "the project link survives a rebuild")
        self.assertFalse((out / "stale.srec").exists() or (out / "index.html").exists() or (out / "pkg").exists(), "no static site is uploaded")
        (out / "extra.txt").write_text("x")
        with self.assertRaisesRegex(build_site.SiteError, "extra.*extra.txt"):
            build_proxy.verify_output(plan, out)

    def test_the_output_directory_must_be_below_target(self):
        for bad in (build_site.ROOT, build_site.ROOT / "deploy", build_site.ROOT.parent, Path("/"), Path(tempfile.gettempdir()), build_site.ROOT / "target"):
            with self.assertRaisesRegex(build_site.SiteError, "must be below"):
                build_site.clean_output(bad, keep=(".vercel",))

    def test_main_assembles_the_real_proxy(self):
        target = build_site.ROOT / "target"
        with tempfile.TemporaryDirectory(dir=target, prefix="proxy-run-") as scratch:
            out = Path(scratch) / "proxy"
            with contextlib.redirect_stdout(io.StringIO()) as printed:
                self.assertEqual(build_proxy.main(["--out", str(out)]), 0)
            self.assertIn("sha256", printed.getvalue())
            files = sorted(path.relative_to(out).as_posix() for path in out.rglob("*") if path.is_file())
            self.assertEqual(files, ["api/firmware.mjs", "package.json", "vercel.json"])
            for name in files:
                self.assertEqual((out / name).read_bytes(), (build_proxy.DEPLOY / name).read_bytes())


if __name__ == "__main__":
    unittest.main(verbosity=2)
