#!/usr/bin/env python3
"""Derives testdata/instruction-starts-<role>.txt from a Ghidra disassembly listing.

The listing (`address | bytes | asm` lines, produced in the separate Renode-based analysis workspace, which is not
public, by a static-analysis export of the original SREC images) is not part of this repository and the committed
output contains no instruction bytes
and no mnemonics: only one bit per halfword saying "an instruction that Ghidra listed starts here".
`crates/armv7m/tests/static_decode.rs` decodes the bytes at those addresses from the user's local SREC.

usage: instruction_starts.py LISTING ROLE OUT   (ROLE = main | handset; standard library only)
"""
import sys

BASE = 0x0800_4000
SPANS = {"main": 0x0803_1938, "handset": 0x080B_074C}


def main() -> int:
    if len(sys.argv) != 4 or sys.argv[2] not in SPANS:
        print(__doc__)
        return 2
    listing, role, out = sys.argv[1:]
    halfwords = (SPANS[role] - BASE) // 2
    bits = bytearray((halfwords + 7) // 8)
    count = 0
    with open(listing, encoding="utf-8") as handle:
        for line in handle:
            if line.startswith("#") or not line.strip():
                continue
            parts = [p.strip() for p in line.split("|", 2)]
            if len(parts) < 3:
                continue
            try:
                address = int(parts[0], 16)
            except ValueError:
                continue
            if len(parts[1].split()) < 2:
                continue
            index = (address - BASE) // 2
            if address % 2 or not 0 <= index < halfwords:
                raise SystemExit(f"address {address:#x} outside the image span")
            bits[index // 8] |= 1 << (index % 8)
            count += 1
    with open(out, "w", encoding="ascii", newline="\n") as handle:
        handle.write(f"# {role}: instruction starts that Ghidra listed in the {role} image (no bytes, no mnemonics).\n")
        handle.write(f"# base 0x{BASE:08x}, {halfwords} halfwords, {count} starts; bit i of byte j = halfword 8*j+i, 64 hex digits per line.\n")
        text = bits.hex()
        for start in range(0, len(text), 64):
            handle.write(text[start:start + 64] + "\n")
    print(f"{role}: {count} instruction starts -> {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
