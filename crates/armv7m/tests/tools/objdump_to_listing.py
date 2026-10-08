#!/usr/bin/env python3
"""Converts `arm-none-eabi-objdump -d` output of an object file into the Ghidra listing format
(`address | bytes | text`) consumed by the `disasm_listing` example. Standard library only.

Usage: objdump_to_listing.py <objdump.txt> > listing.txt
"""
import re
import sys

for line in open(sys.argv[1]):
    m = re.match(r"\s*([0-9a-f]+):\t([0-9a-f ]+?)\s*\t(.*)$", line)
    if not m:
        continue
    addr = int(m.group(1), 16)
    hws = m.group(2).split()
    data = []
    for hw in hws:
        data += [hw[2:4], hw[0:2]]  # objdump prints halfword values; memory is little endian
    print("%08x | %s | %s" % (addr, " ".join(data), m.group(3).split("@")[0].strip()))
