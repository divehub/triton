#!/usr/bin/env python3
"""Finds the equivalent of a TRITON firmware address in another release by matching normalized disassembly.

This is how the NEPTUN entries of `ReleaseAddresses` (crates/ngc/src/firmware.rs) were proven. Standard library only.

    release_match.py [options] sites ROLE ADDRESS [SPAN]     find the code that accesses the RAM address in TRITON and the
                                                            identical code (and its literal) in the other release
    release_match.py [options] range ROLE START END          find the instruction range START..END of TRITON in the other
                                                            release and print the literal pairs
    release_match.py [options] align ROLE TRITON NEPTUN N    side by side alignment of N instructions
    release_match.py [options] bytes RELEASE ROLE HEX        addresses of a byte sequence in an image

ROLE is main|handset; addresses are hexadecimal. Options: --cli PATH (ngc-cli, default target/release/ngc-cli),
--firmware DIR (the directory that holds the release directories; default NGC_FIRMWARE_DIR, else the repository
`firmware` directory, which you supply yourself), --other ID (default NEPTUN-5.8-65.3), --cache DIR (disassembly cache,
default the system temp directory).

The disassembly of every image comes from `ngc-cli disasm`. A line is normalized by masking what a relocation changes: the
offset of `ldr rX, [pc, #n]` (the literal is compared separately), the target of `bl` and the displacement of branches
(kept relative). A site is *proven* when a window of 25 or more instructions around the access matches exactly once.
"""
import argparse
import difflib
import os
import re
import subprocess
import sys
import tempfile

BASE = 0x08004000
LINE = re.compile(r"^([0-9a-f]{8}):\s+((?:[0-9a-f]{4} ?)+)\s+(.*)$")
ACCESS = re.compile(r"(?:ldr|str)(?:b|h|sb|sh)?(?:\.w)? r\d+, \[(r\d+|sp|lr)(?:, #(-?(?:0x)?[0-9a-f]+))?\]")


class Setup:
    def __init__(self, args):
        self.cli = args.cli
        self.firmware = args.firmware
        self.other = args.other
        self.cache = args.cache


def names(setup, release_id):
    tag = release_id.split("-")[0]
    return {"main": f"ngc_main_5.8_{tag}.srec", "handset": f"ngc_handset_65.3_{tag}.srec"}


class Image:
    def __init__(self, setup, release_id, role):
        srec = os.path.join(setup.firmware, release_id, names(setup, release_id)[role])
        cached = os.path.join(setup.cache, f"disasm-{release_id}-{role}.txt")
        binary = os.path.join(setup.cache, f"image-{release_id}-{role}")
        if not os.path.exists(cached):
            os.makedirs(setup.cache, exist_ok=True)
            subprocess.run([setup.cli, "extract-bin", f"--{role}", srec, "--out-dir", binary, "--force"], check=True, capture_output=True)
            size = os.path.getsize(os.path.join(binary, role, "firmware.bin"))
            text = subprocess.run([setup.cli, "disasm", "--board", role, f"--{role}", srec, hex(BASE), str(size // 2)], check=True, capture_output=True, text=True).stdout
            with open(cached, "w") as handle:
                handle.write(text)
        self.data = open(os.path.join(binary, role, "firmware.bin"), "rb").read()
        self.lines, self.index = [], {}
        for raw in open(cached):
            match = LINE.match(raw.rstrip("\n"))
            if match:
                address = int(match.group(1), 16)
                self.index[address] = len(self.lines)
                self.lines.append((address, match.group(3).strip()))

    def word(self, address):
        offset = address - BASE
        return int.from_bytes(self.data[offset:offset + 4], "little") if 0 <= offset <= len(self.data) - 4 else None

    def literal(self, i):
        """(register, value) of an `ldr rN, [pc, #off]` line."""
        address, text = self.lines[i]
        match = re.match(r"ldr(?:\.w)? (r\d+|sp|lr), \[pc, #(-?(?:0x)?[0-9a-f]+)\]$", text)
        if not match:
            return None
        raw = match.group(2)
        offset = int(raw, 16) if not raw.lstrip("-").startswith("0x") else int(raw, 0)
        value = self.word(((address + 4) & ~3) + offset)
        return (match.group(1), value) if value is not None else None

    def normalize(self, i):
        address, text = self.lines[i]
        text = re.sub(r"\[pc, #-?(?:0x)?[0-9a-f]+\]", "[pc, #L]", text)
        match = re.match(r"(bl|blx) 0x[0-9a-f]+$", text)
        if match:
            return match.group(1) + " F"
        match = re.match(r"(b[a-z]*(?:\.w)?|cbn?z r\d+,|adr(?:\.w)? r\d+,) 0x([0-9a-f]+)$", text)
        if match:
            return f"{match.group(1)} <{int(match.group(2), 16) - address:+d}>"
        return text


def immediate(raw):
    if raw is None:
        return 0
    return int(raw, 0) if "0x" in raw else int(raw, 10)


def access_sites(image, target, spread=4095):
    """(ldr line, access line, base value, immediate) of the accesses to RAM address `target`."""
    sites = []
    for i in range(len(image.lines)):
        literal = image.literal(i)
        if not literal or literal[1] is None or not target - spread <= literal[1] <= target:
            continue
        for j in range(i + 1, min(i + 12, len(image.lines))):
            match = ACCESS.match(image.lines[j][1])
            if match and match.group(1) == literal[0]:
                if literal[1] + immediate(match.group(2)) == target:
                    sites.append((i, j, literal[1], immediate(match.group(2))))
                break
    return sites


def find(normalized, window):
    first = window[0]
    n = len(window)
    return [k for k, line in enumerate(normalized) if line == first and normalized[k:k + n] == window]


def cmd_sites(setup, role, target, span):
    triton, other = Image(setup, "TRITON-5.8-65.3", role), Image(setup, setup.other, role)
    normalized = [other.normalize(i) for i in range(len(other.lines))]
    sites = access_sites(triton, target)
    print(f"{role} TRITON 0x{target:08x}: {len(sites)} access site(s)")
    candidates = {}
    for i, j, value, imm in sites:
        for before in (span, 2 * span):
            lo, hi = max(0, i - before), min(len(triton.lines), j + span + 1)
            hits = find(normalized, [triton.normalize(k) for k in range(lo, hi)])
            print(f"  site 0x{triton.lines[j][0]:08x} (literal 0x{value:08x}+{imm:#x}) window {hi - lo}: {len(hits)} hit(s)")
            for h in hits[:3]:
                literal = other.literal(h + (i - lo))
                match = ACCESS.match(other.lines[h + (j - lo)][1])
                if literal and match:
                    candidate = literal[1] + immediate(match.group(2))
                    print(f"      other 0x{other.lines[h + (j - lo)][0]:08x} -> 0x{candidate:08x}")
                    if len(hits) == 1:
                        candidates[candidate] = candidates.get(candidate, 0) + 1
            if len(hits) == 1:
                break
    print("candidates from unique windows:", {hex(k): v for k, v in candidates.items()})


def cmd_range(setup, role, start, end):
    triton, other = Image(setup, "TRITON-5.8-65.3", role), Image(setup, setup.other, role)
    normalized = [other.normalize(i) for i in range(len(other.lines))]
    lo, hi = triton.index[start], triton.index[end]
    hits = find(normalized, [triton.normalize(k) for k in range(lo, hi + 1)])
    print(f"{role}: TRITON 0x{start:08x}..0x{end:08x} ({hi - lo + 1} lines): {len(hits)} hit(s)")
    for h in hits[:6]:
        print(f"  other 0x{other.lines[h][0]:08x}")
        for offset in range(hi - lo + 1):
            a, b = triton.literal(lo + offset), other.literal(h + offset)
            if a and b:
                print(f"    0x{triton.lines[lo + offset][0]:08x} =0x{a[1]:08x}  ->  0x{other.lines[h + offset][0]:08x} =0x{b[1]:08x}")


def cmd_align(setup, role, triton_start, other_start, count):
    triton, other = Image(setup, "TRITON-5.8-65.3", role), Image(setup, setup.other, role)
    ti, oi = triton.index[triton_start], other.index[other_start]
    left = [triton.normalize(k) for k in range(ti, ti + count)]
    right = [other.normalize(k) for k in range(oi, oi + count)]

    def show(image, k):
        address, text = image.lines[k]
        literal = image.literal(k)
        return f"{address:08x} {text}" + (f" [=0x{literal[1]:08x}]" if literal and literal[1] is not None else "")

    same = 0
    for tag, i1, i2, j1, j2 in difflib.SequenceMatcher(None, left, right, autojunk=False).get_opcodes():
        for d in range(max(i2 - i1, j2 - j1)):
            a = show(triton, ti + i1 + d) if d < i2 - i1 else ""
            b = show(other, oi + j1 + d) if d < j2 - j1 else ""
            print(f"{' ' if tag == 'equal' else '*'} {a:<58} | {b}")
        same += i2 - i1 if tag == "equal" else 0
    print(f"identical normalized lines: {same} of {count}")


def cmd_bytes(setup, release_id, role, hex_bytes):
    image = Image(setup, release_id, role)
    needle, pos, found = bytes.fromhex(hex_bytes), 0, []
    while (pos := image.data.find(needle, pos)) >= 0:
        if pos % 2 == 0:
            found.append(BASE + pos)
        pos += 1
    print(f"{release_id} {role}: {len(found)} match(es): " + ", ".join(f"0x{a:08x}" for a in found[:20]))


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    root = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
    parser.add_argument("--cli", default=os.path.join(root, "target", "release", "ngc-cli"))
    parser.add_argument("--firmware", default=os.environ.get("NGC_FIRMWARE_DIR") or os.path.join(root, "firmware"))
    parser.add_argument("--other", default="NEPTUN-5.8-65.3")
    parser.add_argument("--cache", default=os.path.join(tempfile.gettempdir(), "ngc-release-match"))
    parser.add_argument("command", choices=["sites", "range", "align", "bytes"])
    parser.add_argument("rest", nargs="+")
    args = parser.parse_args()
    setup, rest = Setup(args), args.rest
    if args.command == "sites":
        cmd_sites(setup, rest[0], int(rest[1], 16), int(rest[2]) if len(rest) > 2 else 12)
    elif args.command == "range":
        cmd_range(setup, rest[0], int(rest[1], 16), int(rest[2], 16))
    elif args.command == "align":
        cmd_align(setup, rest[0], int(rest[1], 16), int(rest[2], 16), int(rest[3]))
    else:
        cmd_bytes(setup, rest[0], rest[1], rest[2])


if __name__ == "__main__":
    sys.exit(main())
