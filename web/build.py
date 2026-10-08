#!/usr/bin/env python3
"""Build the WebAssembly engine and stage it for the web app.

Runs `./cargo build -p ngc-wasm --release --target wasm32-unknown-unknown` (the repository's cargo wrapper: a
toolchain in tools/rust if there is one, otherwise cargo from PATH; no crates.io dependencies) and copies the result
to `web/pkg/ngc_wasm.wasm`, which is ignored by Git. Python standard library only.

    python3 web/build.py                # release build, copy to web/pkg/
    python3 web/build.py --target-dir target/mine
    python3 web/build.py --no-build     # only copy an already built module
"""
import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

WEB = Path(__file__).resolve().parent
ROOT = WEB.parent
CARGO = ROOT / "cargo"
TRIPLE = "wasm32-unknown-unknown"


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--target-dir", default="target/web", help="cargo target directory relative to the repository root (default: target/web)")
    parser.add_argument("--debug", action="store_true", help="build the unoptimised dev profile instead of release (much slower in the browser)")
    parser.add_argument("--no-build", action="store_true", help="skip cargo and copy the module that is already in the target directory")
    parser.add_argument("--no-copy", action="store_true", help="build only; do not copy to web/pkg/")
    args = parser.parse_args()

    profile = "debug" if args.debug else "release"
    if not args.no_build:
        command = [str(CARGO), "build", "-p", "ngc-wasm", "--target", TRIPLE, "--target-dir", args.target_dir]
        if not args.debug:
            command.insert(command.index("--target"), "--release")
        print("+", " ".join(command), flush=True)
        started = time.monotonic()
        result = subprocess.run(command)
        if result.returncode != 0:
            return result.returncode
        print(f"cargo finished in {time.monotonic() - started:.1f} s")

    target = Path(args.target_dir)
    built = (target if target.is_absolute() else ROOT / target) / TRIPLE / profile / "ngc_wasm.wasm"
    if not built.is_file():
        print(f"error: {built} does not exist; run without --no-build", file=sys.stderr)
        return 1
    data = built.read_bytes()
    digest = hashlib.sha256(data).hexdigest()
    print(f"module: {built} ({len(data)} bytes, SHA-256 {digest})")
    if args.no_copy:
        return 0

    pkg = WEB / "pkg"
    pkg.mkdir(exist_ok=True)
    temporary = pkg / "ngc_wasm.wasm.tmp"
    shutil.copyfile(built, temporary)
    os.replace(temporary, pkg / "ngc_wasm.wasm")
    cargo_version = subprocess.run([str(CARGO), "--version"], capture_output=True, text=True).stdout.strip()
    info = {
        "source": str(built.relative_to(ROOT)) if ROOT in built.parents else built.name,
        "bytes": len(data),
        "sha256": digest,
        "profile": profile,
        "cargo": cargo_version,
        "built": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }
    (pkg / "build-info.json").write_text(json.dumps(info, indent=2) + "\n")
    print(f"staged web/pkg/ngc_wasm.wasm; serve it with: python3 {WEB / 'serve.py'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
