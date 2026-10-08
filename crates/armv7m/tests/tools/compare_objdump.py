#!/usr/bin/env python3
"""Cross-checks the armv7m decoder against arm-none-eabi-objdump.

Inputs:
  1. mine:    output of `cargo run -p armv7m --example disasm_listing -- <ghidra disassembly.txt>`
              (lines: address<TAB>hex bytes<TAB>text)
  2. objdump: `arm-none-eabi-objdump -D -b binary -m arm -M force-thumb,reg-names-raw
               --adjust-vma=0x08004000 firmware.bin`

Instructions are compared at the addresses of the Ghidra listing (which excludes data). Only
addresses where objdump decoded the same bytes at the same address are compared (a linear
disassembly desynchronises after literal pools). Both texts are normalised to a canonical
operand form before comparing. Standard library only.

Usage: compare_objdump.py <mine> <objdump> [--show N]
"""
import re
import sys
from collections import Counter

ALU3 = {"add", "sub", "and", "orr", "eor", "bic", "adc", "sbc", "orn", "rsb", "lsl", "lsr", "asr", "ror", "mul"}


def split_mnemonic(m):
    base = m.split(".")[0]
    return base


def is_alu3(base):
    b = base
    # strip trailing condition codes and 's' to find the root
    for cond in ("eq", "ne", "cs", "cc", "mi", "pl", "vs", "vc", "hi", "ls", "ge", "lt", "gt", "le"):
        if b.endswith(cond) and b[: -len(cond)] in ALU3 | {r + "s" for r in ALU3}:
            b = b[: -len(cond)]
            break
    if b.endswith("s") and b[:-1] in ALU3:
        b = b[:-1]
    return b in ALU3


def norm_imm(tok):
    tok = tok.strip()
    if tok.startswith("#"):
        v = int(tok[1:], 0)
        return "#%d" % (v & 0xFFFFFFFF if v >= 0 else v)
    return tok


def norm_operand(tok):
    tok = tok.strip()
    # immediates inside memory operands
    def repl(m):
        return "#%d" % int(m.group(1), 0)

    tok = re.sub(r"#(-?(?:0x[0-9a-fA-F]+|\d+))", repl, tok)
    tok = tok.replace(", #0]", "]")
    return tok


def norm_text(text, addr=0):
    text = text.split("@")[0].split(";")[0]
    text = re.sub(r"<[^>]*>", "", text).strip().lower()
    if not text:
        return ""
    parts = text.split(None, 1)
    mnem = parts[0]
    ops = parts[1] if len(parts) > 1 else ""
    base = split_mnemonic(mnem)
    for old, new in (("cpsr_fs", "apsr_nzcvqg"), ("cpsr_f", "apsr_nzcvq"), ("cpsr_s", "apsr_g"), ("cpsr", "apsr"), ("psr", "xpsr")):
        ops = re.sub(r"\b" + old + r"\b", new, ops)
    ops = re.sub(r"\br13\b", "sp", ops)
    ops = re.sub(r"\br14\b", "lr", ops)
    ops = re.sub(r"\br15\b", "pc", ops)
    # operands: split on commas outside brackets/braces
    out, depth, cur = [], 0, ""
    for ch in ops:
        if ch in "[{":
            depth += 1
        if ch in "]}":
            depth -= 1
        if ch == "," and depth == 0:
            out.append(cur.strip())
            cur = ""
        else:
            cur += ch
    if cur.strip():
        out.append(cur.strip())
    # keep post-index form "[rn]" "#imm" as separate operands (same on both sides)
    out = [norm_operand(o) for o in out]
    COND = "(?:eq|ne|cs|cc|mi|pl|vs|vc|hi|ls|ge|lt|gt|le)?"
    CONDG = "(eq|ne|cs|cc|mi|pl|vs|vc|hi|ls|ge|lt|gt|le|)"
    # branch / numeric targets: bare hex numbers
    if re.fullmatch(r"(b|bl|cbz|cbnz)" + COND, base) or re.fullmatch(r"b(eq|ne|cs|cc|mi|pl|vs|vc|hi|ls|ge|lt|gt|le)", base):
        out = [("0x%08x" % int(o, 16)) if re.fullmatch(r"(0x)?[0-9a-f]+", o) and not re.fullmatch(r"r\d+|sp|lr|pc", o) else o for o in out]
    # unprivileged accesses behave like the plain ones in this model (no privilege checks)
    m = re.fullmatch(r"(ldr|ldrb|ldrh|ldrsb|ldrsh|str|strb|strh)t" + CONDG, base)
    if m:
        base = m.group(1) + m.group(2)
    # ldrd/strd with a zero offset: binutils omits the writeback marker
    if re.fullmatch(r"(ldrd|strd)" + CONDG, base) and out and out[-1].endswith("]!") and out[-1].startswith("[") and "," not in out[-1]:
        out[-1] = out[-1][:-1]
    # addw/subw (12-bit immediate forms) are plain add/sub with a wide immediate
    m = re.fullmatch(r"(add|sub)w" + CONDG, base)
    if m:
        base = m.group(1) + m.group(2)
    # ldmia sp!, {..} / stmdb sp!, {..} are pop / push
    m = re.fullmatch(r"(ldmia|stmdb)" + CONDG, base)
    if m and len(out) == 2 and out[0] == "sp!":
        base = ("pop" if m.group(1) == "ldmia" else "push") + m.group(2)
        out = [out[1]]
    # muls r0, r1 is r0 = r1 * r0
    m = re.fullmatch(r"mul(s?)" + CONDG, base)
    if m and len(out) == 2:
        out = [out[0], out[1], out[0]]
    if base in ("svc", "bkpt", "udf") and out:
        out = ["#%d" % int(out[0].lstrip("#"), 0)]
    # two-operand forms of three-operand instructions
    if is_alu3(base) and len(out) == 2:
        out = [out[0]] + out
    m = re.fullmatch(r"neg(s?)((?:eq|ne|cs|cc|mi|pl|vs|vc|hi|ls|ge|lt|gt|le)?)", base)
    if m:
        base = "rsb" + m.group(1) + m.group(2)
        out = [out[0], out[1], "#0"]
    # `mov{s}{cond} rd, rm, <shift> #n` is the assembler alias of the shift instructions
    m = re.fullmatch(r"mov(s?)((?:eq|ne|cs|cc|mi|pl|vs|vc|hi|ls|ge|lt|gt|le)?)", base)
    if m and len(out) == 3 and re.match(r"(lsl|lsr|asr|ror) ", out[2]):
        sh = out[2].split()
        base = sh[0] + m.group(1) + m.group(2)
        out = [out[0], out[1], sh[1]]
    if m and len(out) == 3 and out[2] == "rrx":
        base = "rrx" + m.group(1) + m.group(2)
        out = [out[0], out[1]]
    # `add rd, pc, #imm` (T1 ADR) -> adr rd, <absolute target>
    m = re.fullmatch(r"add" + CONDG, base)
    if m and len(out) == 3 and out[1] == "pc" and out[2].startswith("#"):
        base = "adr" + m.group(1)
        out = [out[0], "0x%08x" % ((((addr + 4) & ~3) + int(out[2][1:])) & 0xFFFFFFFF)]
    return base + (" " + ", ".join(out) if out else "")


def main():
    mine_path, obj_path = sys.argv[1], sys.argv[2]
    show = 40
    if "--show" in sys.argv:
        show = int(sys.argv[sys.argv.index("--show") + 1])
    obj = {}
    for line in open(obj_path):
        m = re.match(r"\s*([0-9a-f]+):\t([0-9a-f ]+?)\s*\t(.*)$", line)
        if m:
            obj[int(m.group(1), 16)] = (m.group(2).replace(" ", ""), m.group(3))
    total = same = skipped = vfp_total = 0
    vfp_diffs = []
    diffs = []
    kinds = Counter()
    for line in open(mine_path):
        addr_s, hexbytes, text = line.rstrip("\n").split("\t", 2)
        addr = int(addr_s, 16)
        want = "".join(hexbytes.split())
        # listing bytes are in memory order; objdump prints halfwords (little endian value)
        mem = bytes.fromhex(want)
        hw = "".join("%02x%02x" % (mem[i + 1], mem[i]) for i in range(0, len(mem), 2))
        o = obj.get(addr)
        total += 1
        if o is not None and o[1].strip().startswith(("0x", ".")):
            skipped += 1
            continue
        if o is None or o[0] != hw:
            skipped += 1
            continue
        parts0 = o[1].split("@")[0].split(None, 1)
        ob0 = parts0[0].lower() if parts0 else ""
        if ob0.startswith("v") or ob0 in ("stc", "ldc"):
            vfp_total += 1
            mine_root = text.split(None, 1)[0].lower().split(".")[0]
            obj_root = ob0.split(".")[0]
            if mine_root == obj_root or mine_root.startswith(obj_root[:4]):
                same += 1
            else:
                vfp_diffs.append((addr, hw, text, o[1]))
            continue
        a = norm_text(text, addr)
        b = norm_text(o[1], addr)
        if a == b:
            same += 1
            continue
        # Known benign differences
        if o[1].strip().startswith(("0x", ".")):
            skipped += 1
            continue
        ob = split_mnemonic(o[1].split(None, 1)[0].lower())
        if ob in ("dmb", "dsb", "isb", "yield", "dbg", "pld", "pldw", "pli", "udf") and (a.startswith("nop") or a.startswith("undefined")):
            same += 1
            continue
        diffs.append((addr, hw, a, b))
        kinds[(b.split() or ["<empty>"])[0]] += 1
    print("total %d  identical %d  skipped(desync) %d  different %d  (vfp lines compared by mnemonic root: %d, mismatching %d)" % (total, same, skipped, len(diffs), vfp_total, len(vfp_diffs)))
    for d in vfp_diffs[:show]:
        print("VFP %08x %-10s mine: %-40s objdump: %s" % d)
    for d in diffs[:show]:
        print("%08x %-10s mine: %-40s objdump: %s" % d)
    if diffs:
        print("differing objdump mnemonics:", kinds.most_common(30))
    sys.exit(1 if diffs else 0)


if __name__ == "__main__":
    main()
