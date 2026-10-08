#!/usr/bin/env python3
"""Cross-check the armv7m-vfp decoder against GNU objdump (development aid).

Enumerates every 32-bit Thumb coprocessor-space encoding with CP10/CP11
(hw1 = 0xEC00..0xEFFF and 0xFC00..0xFFFF, hw2 with coprocessor 10/11: 16.8 M
encodings), disassembles them with `arm-none-eabi-objdump` (an independent
implementation of the Arm encoding tables) and compares with the decoder and
debug disassembler of this crate:

* every encoding the crate decodes must disassemble to the same instruction;
* every encoding the crate rejects (`Undefined`) must be one GNU also rejects,
  flags as UNPREDICTABLE, or that FPv4-SP does not implement (double-precision
  data processing, Armv8 additions, Advanced SIMD moves, other VFP system
  registers); anything else is reported.

Requires `arm-none-eabi-objdump` (not needed by `cargo test`). Standard library only.

Usage: python3 objdump_crosscheck.py [--work DIR] [--quick]
"""
import argparse
import os
import re
import struct
import subprocess
import sys
from collections import Counter, defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
WASM_DIR = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
CARGO = os.path.join(WASM_DIR, "cargo")
OBJDUMP = os.environ.get("OBJDUMP", "/opt/homebrew/bin/arm-none-eabi-objdump")

LINE = re.compile(r"^\s*([0-9a-f]+):\t((?:[0-9a-f]{4} ?)+)\s*\t(.*)$")
RANGE = re.compile(r"\{([sd])(\d+)-([sd])(\d+)\}")


def hw2_list():
    return [h for h in range(0x10000) if (h & 0x0E00) == 0x0A00]


def write_input(path, quick):
    hw2s = hw2_list()
    # Coprocessor space proper (0xEC00-0xEEFF, 0xFC00-0xFEFF) plus the
    # 0xEFxx / 0xFFxx Advanced SIMD space, which FPv4-SP must reject entirely.
    prefixes = list(range(0xEC00, 0xF000)) + list(range(0xFC00, 0x10000))
    if quick:
        prefixes = prefixes[::7]
    with open(path, "wb") as f:
        for hw1 in prefixes:
            f.write(b"".join(struct.pack("<HH", hw1, hw2) for hw2 in hw2s))
    return prefixes, hw2s


def expand_range(m):
    t1, a, t2, b = m.group(1), int(m.group(2)), m.group(3), int(m.group(4))
    assert t1 == t2
    return "{" + ",".join(f"{t1}{i}" for i in range(a, b + 1)) + "}"


def hex_imm(m):
    v = int(m.group(1))
    return f"#-0x{-v:x}" if v < 0 else f"#0x{v:x}"


def normalise(mnem, ops, comment):
    """GNU objdump text -> the crate's disassembly layout."""
    mnem = mnem.lower()
    ops = ops.replace(" ", "").lower()
    ops = ops.replace("apsr_nzcv", "apsr")
    # GNU uses the APCS aliases for r10..r12.
    ops = re.sub(r"\bsl\b", "r10", ops)
    ops = re.sub(r"\bfp\b", "r11", ops)
    ops = re.sub(r"\bip\b", "r12", ops)
    ops = RANGE.sub(expand_range, ops)
    if mnem == "vmov.f32" and "#" in ops:
        m = re.search(r"@ (0x[0-9a-f]{8})", comment)
        ops = ops.split("#")[0] + m.group(1)
    elif mnem.startswith("vcmp") and ops.endswith("#0.0"):
        ops = ops[:-3] + "0"
    else:
        ops = re.sub(r"#(-?\d+)\b", hex_imm, ops)
    ops = ops.replace(",#-0x0]", "]").replace(",#0x0]", "]")
    if mnem in ("vldr", "vstr"):
        mnem += ".64" if ops.startswith("d") else ".32"
    return f"{mnem} {ops}".strip()


def parse_objdump(chunk_path):
    """Returns {index: (kind, text)} for a chunk file; kind in insn/unpred/undef."""
    out = subprocess.run(
        [OBJDUMP, "-D", "-b", "binary", "-m", "arm", "-M", "force-thumb", chunk_path],
        capture_output=True, text=True, check=True).stdout
    res = {}
    for line in out.splitlines():
        m = LINE.match(line)
        if not m:
            continue
        addr = int(m.group(1), 16)
        text = m.group(3)
        idx = addr // 4
        if addr % 4 == 2:
            continue  # second halfword of an undefined encoding printed as two .short
        if "<UNDEFINED>" in text or text.lstrip().startswith("."):
            res[idx] = ("undef", text)
            continue
        comment = ""
        if "@" in text:
            text, comment = text.split("@", 1)
            comment = "@" + comment
        unpred = False
        if ";" in text:
            text, c2 = text.split(";", 1)
            unpred = "UNPREDICTABLE" in c2 or "UNDEFINED" in c2
        parts = text.split("\t", 1)
        mnem = parts[0].strip()
        ops = parts[1].strip() if len(parts) > 1 else ""
        res[idx] = ("unpred" if unpred else "insn", normalise(mnem, ops, comment) if not unpred else f"{mnem} {ops}")
    return res


COPROC_MNEMONICS = ("ldc", "stc", "mcr", "mrc", "mcrr", "mrrc", "cdp")
DREG_HIGH = re.compile(r"\bd(1[6-9]|2\d|3[01])\b")
SREG_BAD = re.compile(r"\bs(-1|3[2-9]|[4-9]\d|\d{3,})\b")


def reason_unsupported(text, hw2=0, hw1=0):
    """Why FPv4-SP rejects an encoding GNU accepts (None = unexplained)."""
    mn = text.split(" ")[0]
    ops = text[len(mn):]
    base = mn.split(".")[0]
    if base.rstrip("2l") in COPROC_MNEMONICS and re.match(r"\s*1[01],", ops):
        return "generic coprocessor instruction on CP10/CP11 (not a VFP encoding)"
    if mn.startswith("vcvt") and "#-0x" in ops:
        return "fixed-point conversion with negative fraction bits (UNPREDICTABLE)"
    if mn in ("vcmp.f32", "vcmpe.f32") and ops.endswith("#0") and hw2 & 0xF:
        return "VCMP #0.0 with non-zero SBZ bits (UNPREDICTABLE; GNU ignores them)"
    if mn == "vmov.32" and hw2 & 0xF:
        return "VMOV scalar with non-zero SBZ bits (UNPREDICTABLE; GNU ignores them)"
    if base == "vmov" and "s32" in ops:
        return "VMOV Sm,Sm1 with m == 31 (UNPREDICTABLE)"
    if base == "vmov" and re.search(r"\bpc\b", ops):
        return "VMOV with PC as a core register (UNPREDICTABLE)"
    if mn == "vmov" and re.match(r"\s*(r\d+|sp|lr),\1,[sd]", ops):
        return "VMOV Rt,Rt2,.. with Rt == Rt2 (UNPREDICTABLE)"
    if mn == "vmov.32" and hw1 & 0x0090 == 0x0090 and hw2 & 0xF == 0:
        # hw1 bit 7 (U) set together with bit 4 (L): scalar to core register.
        return "VMOV.32 scalar to core with U=1 (UNDEFINED per Arm ARM; GNU ignores U)"
    if mn in ("vmrs", "vmsr") and "nzcvqc" in ops:
        return "Armv8.1-M FPSCR_nzcvqc access"
    if mn == "vmsr" and re.search(r"\bpc\b", ops):
        return "VMSR with PC (UNPREDICTABLE)"
    if re.search(r"\{\}", ops):
        return "empty register list (UNPREDICTABLE)"
    if base in ("vldm", "vldmia", "vldmdb", "vstm", "vstmia", "vstmdb", "vpush", "vpop") and (
            (hw2 >> 8) & 0xF == 0xB and (hw2 & 0xFF) >> 1 > 16):
        return "doubleword register list longer than 16 (UNPREDICTABLE)"
    if base in ("fldmiax", "fldmdbx", "fstmiax", "fstmdbx"):
        return "FLDMX/FSTMX (deprecated odd word count)"
    if base in ("vlldm", "vlstm", "vscclrm"):
        return "Armv8-M floating-point context instruction"
    if mn.startswith(("vsel", "vmaxnm", "vminnm", "vrint", "vcvta", "vcvtn", "vcvtp", "vcvtm", "vins", "vmovx", "vjcvt")):
        return "Armv8 floating-point addition"
    if ".f64" in mn or (re.search(r"\bd\d+,", ops) and base in ("vadd", "vsub", "vmul", "vdiv", "vnmul", "vmla", "vmls", "vnmla", "vnmls", "vfma", "vfms", "vfnma", "vfnms", "vabs", "vneg", "vsqrt", "vcmp", "vcmpe") ):
        return "double-precision data processing"
    if mn.startswith("vdup") or re.match(r"vmov\.(8|16|s8|s16|u8|u16)$", mn):
        return "Advanced SIMD move"
    if mn in ("vmrs", "vmsr") and "fpscr" not in ops:
        return "VFP system register other than FPSCR"
    if DREG_HIGH.search(ops):
        return "uses D16-D31 (FPv4-SP has D0-D15 only)"
    if base in ("vstr", "vstm", "vstmia", "vstmdb", "vldm", "vldmia", "vldmdb", "vpush", "vpop") and (
            "[pc" in ops or re.match(r"pc\b", ops.lstrip()) or ops.lstrip().startswith(("pc!", "pc,"))):
        return "PC as base register (UNPREDICTABLE in Thumb)"
    if base in ("vldm", "vldmia", "vldmdb", "vstm", "vstmia", "vstmdb", "vpush", "vpop") and (
            SREG_BAD.search(ops) or "d-1" in ops or "overflowreg" in ops):
        return "empty / oversized register list (UNPREDICTABLE)"
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--work", default=os.path.join(WASM_DIR, "target", "fpu", "objdump-crosscheck"))
    ap.add_argument("--quick", action="store_true", help="1/7 of the hw1 values")
    ap.add_argument("--keep", action="store_true")
    ap.add_argument("--examples", type=int, default=3)
    args = ap.parse_args()
    os.makedirs(args.work, exist_ok=True)
    in_path = os.path.join(args.work, "in.bin")
    out_path = os.path.join(args.work, "ours.txt")

    print("writing encodings ...", flush=True)
    prefixes, hw2s = write_input(in_path, args.quick)
    n = len(prefixes) * len(hw2s)
    print(f"{n} encodings", flush=True)

    env = dict(os.environ, NGC_VFP_DUMP_IN=in_path, NGC_VFP_DUMP_OUT=out_path)
    print("running our decoder ...", flush=True)
    subprocess.run(
        [CARGO, "test", "-p", "armv7m-vfp", "--release", "--target-dir", "target/fpu",
         "--test", "objdump_dump", "--", "--ignored", "objdump_dump"],
        env=env, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    counts = Counter()
    examples = defaultdict(list)

    def note(key, ex):
        counts[key] += 1
        if len(examples[key]) < args.examples:
            examples[key].append(ex)

    per = len(hw2s)
    group = 8  # hw1 values per objdump invocation
    chunk_path = os.path.join(args.work, "chunk.bin")
    with open(in_path, "rb") as fin, open(out_path) as fours:
        for g in range(0, len(prefixes), group):
            hw1s = prefixes[g:g + group]
            data = fin.read(4 * per * len(hw1s))
            with open(chunk_path, "wb") as f:
                f.write(data)
            gnu = parse_objdump(chunk_path)
            for k in range(per * len(hw1s)):
                hw1, hw2 = struct.unpack_from("<HH", data, 4 * k)
                ours = fours.readline().rstrip("\n")
                gk, gt = gnu.get(k, ("undef", "<missing>"))
                enc = f"{hw1:04x} {hw2:04x}"
                if hw1 & 0x0300 == 0x0300:
                    # 0xEFxx / 0xFFxx: Advanced SIMD data processing, not a coprocessor encoding.
                    if ours == "-":
                        counts["Advanced SIMD space (0xEFxx/0xFFxx): rejected"] += 1
                    else:
                        note("ours accepts Advanced SIMD space (bug)", (enc, ours, gt))
                    continue
                if ours == "=":
                    note("ours NotVfp in CP10/11 sweep (bug)", (enc, ours, gt))
                elif ours == "-":
                    if gk in ("undef", "unpred"):
                        counts["both reject / GNU unpredictable"] += 1
                        if gk == "unpred":
                            counts["  (GNU: unpredictable)"] += 1
                        continue
                    why = reason_unsupported(gt, hw2, hw1)
                    if why:
                        counts[f"unsupported on FPv4-SP: {why}"] += 1
                    else:
                        note(f"UNEXPLAINED: ours Undefined, GNU `{gt.split(' ')[0]}`", (enc, ours, gt))
                else:
                    if gk == "insn":
                        if ours == gt:
                            counts["identical"] += 1
                        else:
                            note("MISMATCH text", (enc, ours, gt))
                    elif gk == "unpred":
                        note("ours decodes, GNU says UNPREDICTABLE", (enc, ours, gt))
                    else:
                        note("ours decodes, GNU says undefined", (enc, ours, gt))
            if (g // group) % 16 == 0:
                print(f"  hw1 {hw1s[0]:04x}.. done", flush=True)

    report = []

    def emit(s=""):
        print(s)
        report.append(s)

    emit()
    emit(f"{n} encodings checked against {OBJDUMP}")
    for key, c in sorted(counts.items()):
        emit(f"{c:10d}  {key}")
    bad = 0
    for key, exs in sorted(examples.items()):
        emit(f"\n{key}: {counts[key]}")
        for enc, ours, gt in exs:
            emit(f"    {enc}: ours `{ours}` / GNU `{gt}`")
        if key.startswith(("MISMATCH", "UNEXPLAINED", "ours decodes", "ours NotVfp", "ours accepts")):
            bad += counts[key]
    emit("\nresult: " + ("OK" if bad == 0 else f"{bad} discrepancies to review"))
    with open(os.path.join(args.work, "report.txt"), "w") as f:
        f.write("\n".join(report) + "\n")
    if not args.keep:
        for p in (in_path, out_path, chunk_path):
            try:
                os.remove(p)
            except OSError:
                pass
    return 0 if bad == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
