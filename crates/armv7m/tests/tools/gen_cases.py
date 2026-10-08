#!/usr/bin/env python3
"""Builds tests/generated/cases.rs from the case specifications in tests/cases/*.txt.

Each case line is `name | asm | init | expect`:
  asm     assembly statements separated by ';' (GNU syntax, unified Thumb, cortex-m4);
          data directives such as `.word 0x1234` are allowed (literal pools).
  init    space separated `key=value` pairs applied before the program runs
  expect  space separated `key=value` pairs checked after `steps` instructions

keys: r0..r12 sp lr          registers
      flags                  NZCV nibble (N=8, Z=4, C=2, V=1)
      q                      Q flag (0/1)
      ge                     GE[3:0]
      steps                  instructions to execute (init only; default = number of statements)
      pc                     expected PC (`+N` = code base + N)
      w[ADDR] h[ADDR] b[ADDR]  memory word/halfword/byte (init and expect)
      cfsr hfsr              fault status (expect only)
      ipsr xpsr              program status (expect only)
      control                CONTROL register (init and expect)
      cpacr fpccr fpdscr     SCB registers (init: written through the PPB; expect: read back)
      fpscr s0..s31          floating-point state (init and expect)
Numbers are hexadecimal (0x...) or decimal; negative numbers are accepted.

The generator assembles every case with arm-none-eabi-as, so encodings are authoritative;
the committed output means `cargo test` needs no toolchain. Standard library only.

Usage: gen_cases.py            (run from anywhere; writes tests/generated/cases.rs)
"""
import glob
import os
import re
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
TESTS = os.path.dirname(HERE)
HEADER = ".syntax unified\n.thumb\n.cpu cortex-m4\n.fpu fpv4-sp-d16\n.text\n"
REGS = {f"r{i}": i for i in range(13)}
REGS.update({"sp": 13, "lr": 14})


def num(tok):
    return int(tok, 0) & 0xFFFFFFFF


def assemble(asm, name):
    src = HEADER + "\n".join(s.strip() for s in asm.split(";")) + "\n"
    with tempfile.TemporaryDirectory() as d:
        s = os.path.join(d, "t.s")
        o = os.path.join(d, "t.o")
        b = os.path.join(d, "t.bin")
        open(s, "w").write(src)
        r = subprocess.run(["arm-none-eabi-as", "-mthumb", "-mcpu=cortex-m4", "-mfpu=fpv4-sp-d16", "-o", o, s], capture_output=True, text=True)
        if r.returncode != 0:
            sys.exit(f"{name}: assembler failed:\n{r.stderr}\n{src}")
        subprocess.run(["arm-none-eabi-objcopy", "-O", "binary", "-j", ".text", o, b], check=True)
        data = open(b, "rb").read()
    if len(data) % 2:
        data += b"\0"
    return [int.from_bytes(data[i : i + 2], "little") for i in range(0, len(data), 2)]


def parse_pairs(text, name):
    items = []
    for tok in text.split():
        if "=" not in tok:
            sys.exit(f"{name}: bad token '{tok}'")
        k, v = tok.split("=", 1)
        items.append((k, v))
    return items


def count_statements(asm):
    n = 0
    for s in asm.split(";"):
        s = s.strip()
        if not s or s.startswith("."):
            continue
        if s.endswith(":"):
            continue
        n += 1
    return n


def rust_pairs(items, name):
    regs, mem, extra = [], [], []
    for k, v in items:
        if k in REGS:
            regs.append((REGS[k], num(v)))
        elif k in ("flags", "q", "ge", "steps", "cfsr", "hfsr", "ipsr", "xpsr", "control", "cpacr", "fpccr", "fpdscr", "fpscr") or re.fullmatch(r"s(\d|[12]\d|3[01])", k):
            extra.append((k, num(v)))
        elif k == "pc":
            extra.append(("pc", v))
        else:
            m = re.fullmatch(r"([whb])\[(0x[0-9a-fA-F]+|\d+)\]", k)
            if not m:
                sys.exit(f"{name}: unknown key '{k}'")
            mem.append((m.group(1), num(m.group(2)), num(v)))
    return regs, mem, extra


def emit_case(f, name, asm, init, expect):
    code = assemble(asm, name)
    steps = count_statements(asm)
    ireg, imem, iextra = rust_pairs(parse_pairs(init, name), name)
    ereg, emem, eextra = rust_pairs(parse_pairs(expect, name), name)
    for k, v in iextra:
        if k == "steps":
            steps = v
    f.write("    Case {\n")
    f.write(f'        name: "{name}",\n        asm: "{asm.replace(chr(34), chr(39))}",\n')
    f.write("        code: &[" + ", ".join(f"0x{c:04x}" for c in code) + "],\n")
    f.write(f"        steps: {steps},\n")
    f.write("        init_regs: &[" + ", ".join(f"({r}, 0x{v:x})" for r, v in ireg) + "],\n")
    f.write("        init_mem: &[" + ", ".join(f"(b'{k}', 0x{a:x}, 0x{v:x})" for k, a, v in imem) + "],\n")
    f.write("        init_misc: &[" + ", ".join(f'("{k}", {v})' for k, v in iextra if k != "steps") + "],\n")
    f.write("        expect_regs: &[" + ", ".join(f"({r}, 0x{v:x})" for r, v in ereg) + "],\n")
    f.write("        expect_mem: &[" + ", ".join(f"(b'{k}', 0x{a:x}, 0x{v:x})" for k, a, v in emem) + "],\n")
    parts = []
    for k, v in eextra:
        if k == "pc":
            if v.startswith("+"):
                parts.append(f'("pc_rel", {int(v[1:], 0)})')
            else:
                parts.append(f'("pc", {num(v)})')
        else:
            parts.append(f'("{k}", {v})')
    f.write("        expect_misc: &[" + ", ".join(parts) + "],\n")
    f.write("    },\n")


def gen_snippets():
    """tests/cases/snippets.snip: `NAME | asm` -> tests/generated/snippets.rs (pub const NAME: &[u16])."""
    path = os.path.join(TESTS, "cases", "snippets.snip")
    if not os.path.exists(path):
        return
    out = os.path.join(TESTS, "generated", "snippets.rs")
    names = set()
    with open(out, "w") as f:
        f.write("// @generated by tests/tools/gen_cases.py from tests/cases/snippets.snip - do not edit.\n")
        f.write("#![allow(dead_code)]\n\n")
        for ln, line in enumerate(open(path), 1):
            if line.lstrip().startswith("#") or not line.strip():
                continue
            parts = [p.strip() for p in line.split(" // ", 1)[0].split("|", 1)]
            if len(parts) != 2 or not re.fullmatch(r"[A-Z][A-Z0-9_]*", parts[0]):
                sys.exit(f"{path}:{ln}: expected `NAME | asm`")
            if parts[0] in names:
                sys.exit(f"duplicate snippet {parts[0]}")
            names.add(parts[0])
            code = assemble(parts[1], parts[0])
            f.write(f"/// `{parts[1]}`\n")
            f.write(f"pub const {parts[0]}: &[u16] = &[" + ", ".join(f"0x{c:04x}" for c in code) + "];\n")
    print(f"wrote {out}: {len(names)} snippets")


def main():
    gen_snippets()
    cases = []
    for path in sorted(glob.glob(os.path.join(TESTS, "cases", "*.txt"))):
        for ln, line in enumerate(open(path), 1):
            if line.lstrip().startswith("#"):
                continue
            line = line.split(" // ", 1)[0].strip()
            if not line:
                continue
            parts = [p.strip() for p in line.split("|")]
            if len(parts) != 4:
                sys.exit(f"{path}:{ln}: expected 4 '|' separated fields, got {len(parts)}: {line}")
            cases.append((os.path.basename(path), *parts))
    names = set()
    os.makedirs(os.path.join(TESTS, "generated"), exist_ok=True)
    out = os.path.join(TESTS, "generated", "cases.rs")
    with open(out, "w") as f:
        f.write("// @generated by tests/tools/gen_cases.py from tests/cases/*.txt - do not edit.\n")
        f.write("// Encodings come from arm-none-eabi-as.\n\n")
        f.write("pub struct Case {\n    pub name: &'static str,\n    pub asm: &'static str,\n    pub code: &'static [u16],\n    pub steps: u64,\n")
        f.write("    pub init_regs: &'static [(usize, u32)],\n    pub init_mem: &'static [(u8, u32, u32)],\n    pub init_misc: &'static [(&'static str, u32)],\n")
        f.write("    pub expect_regs: &'static [(usize, u32)],\n    pub expect_mem: &'static [(u8, u32, u32)],\n    pub expect_misc: &'static [(&'static str, u32)],\n}\n\n")
        f.write("pub const CASES: &[Case] = &[\n")
        for fname, name, asm, init, expect in cases:
            if name in names:
                sys.exit(f"duplicate case name {name}")
            names.add(name)
            emit_case(f, name, asm, init, expect)
        f.write("];\n")
    print(f"wrote {out}: {len(cases)} cases")


if __name__ == "__main__":
    main()
