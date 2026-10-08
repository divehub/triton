#!/usr/bin/env python3
"""Assemble the directory that is uploaded to Vercel: the firmware proxy function and nothing else (Python standard library only).

    python3 deploy/build_proxy.py                  # -> target/vercel-proxy/
    python3 deploy/build_proxy.py --out DIR        # another directory below target/

The Vercel project hosts only the proxy (api/firmware.mjs, vercel.json, package.json); the emulator page itself is a
static site on GitHub Pages (deploy/build_site.py). The result is `target/vercel-proxy/` (ignored by Git) and contains
exactly those three files. The script refuses anything else, anything that looks like firmware, a function that
imports modules (there are no dependencies and no install step), and a vercel.json / package.json that add routing,
a build or dependencies. An existing `.vercel/` directory (the project link written by `vercel link`) in the output
directory is kept; everything else is replaced. It prints each file with its size and SHA-256.

It never contacts Vercel.
"""
import argparse
import json
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import build_site  # noqa: E402  (shared rules: firmware detection, output directory handling)
from build_site import SiteError  # noqa: E402

DEPLOY = build_site.DEPLOY
ROOT = build_site.ROOT
DEFAULT_OUT = ROOT / "target" / "vercel-proxy"
PROXY_FILES = ("api/firmware.mjs", "vercel.json", "package.json")
FORBIDDEN_VERCEL_KEYS = ("rewrites", "redirects", "routes", "builds", "env", "build", "crons", "cleanUrls")


def check_config(deploy_dir=DEPLOY):
    """vercel.json and package.json describe a dependency-free function and nothing else."""
    config = json.loads((deploy_dir / "vercel.json").read_text(encoding="utf-8"))
    entry = config.get("functions", {}).get("api/firmware.mjs")
    if not entry:
        raise SiteError("vercel.json has no functions entry for api/firmware.mjs")
    if entry.get("maxDuration", 0) < 21:
        raise SiteError("vercel.json: maxDuration of api/firmware.mjs must exceed the proxy's 20 s upstream deadline")
    for key in FORBIDDEN_VERCEL_KEYS:
        if key in config:
            raise SiteError(f"vercel.json must not set {key}: the project only hosts the function")
    for rule in config.get("headers", []):
        for header in rule.get("headers", []):
            if header.get("key", "").lower() in {"access-control-allow-origin", "content-security-policy"}:
                raise SiteError(f"vercel.json must not set {header['key']}: the function sets its own CORS headers")
    manifest = json.loads((deploy_dir / "package.json").read_text(encoding="utf-8"))
    for key in ("dependencies", "devDependencies", "optionalDependencies", "scripts"):
        if manifest.get(key):
            raise SiteError(f"package.json must not declare {key}: the project has no install or build step")
    source = (deploy_dir / "api" / "firmware.mjs").read_text(encoding="utf-8")
    if re.search(r"^\s*import\s|\brequire\(|\bimport\(", source, re.MULTILINE):
        raise SiteError("api/firmware.mjs must not import anything: it has no dependencies")


def plan_files(deploy_dir=DEPLOY):
    """[(published path, bytes)] after checking the allowlist; raises SiteError on any violation."""
    plan = []
    for name in PROXY_FILES:
        source = deploy_dir / name
        if not source.is_file():
            raise SiteError(f"{name}: {source} does not exist")
        if source.is_symlink():
            raise SiteError(f"{name}: {source} is a symbolic link")
        data = source.read_bytes()
        reason = build_site.looks_like_firmware(name, data)
        if reason:
            raise SiteError(f"refusing to upload: {reason}")
        plan.append((name, data))
    return plan


def assemble(plan, out):
    build_site.clean_output(out, keep=(".vercel",))
    for name, data in plan:
        destination = out / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_bytes(data)


def verify_output(plan, out):
    """The output holds exactly the planned files, byte for byte, and nothing else (besides `.vercel/`)."""
    expected = {name for name, _ in plan}
    found = {path.relative_to(out).as_posix() for path in out.rglob("*") if path.is_file() and ".vercel" not in path.relative_to(out).parts}
    if found != expected:
        raise SiteError(f"the output differs from the plan: extra {sorted(found - expected)}, missing {sorted(expected - found)}")
    for name, data in plan:
        if (out / name).read_bytes() != data:
            raise SiteError(f"{name} was not written byte for byte")
    return expected


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", default=str(DEFAULT_OUT), help="output directory below target/ (default: target/vercel-proxy)")
    args = parser.parse_args(argv)
    out = Path(args.out)
    if not out.is_absolute():
        out = Path.cwd() / out
    try:
        check_config()
        plan = plan_files()
        assemble(plan, out)
        names = verify_output(plan, out)
    except SiteError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    rows = build_site.describe(out, names)
    width = max(len(name) for name, _, _ in rows)
    print(f"assembled {out} ({len(rows)} files, {sum(size for _, size, _ in rows)} bytes):")
    for name, size, digest in rows:
        print(f"  {name:<{width}}  {size:>9}  sha256 {digest}")
    print("next (run by you, see deploy/README.md): vercel link, then vercel deploy --prod, in that directory")
    return 0


if __name__ == "__main__":
    sys.exit(main())
