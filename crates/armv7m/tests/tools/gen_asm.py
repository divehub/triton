#!/usr/bin/env python3
"""Generates a GNU assembler source covering the ARMv7E-M integer instruction set with varied
operands (registers, immediates at their limits, shifts, condition codes inside IT blocks).

The output is assembled with arm-none-eabi-as, disassembled with objdump and compared with the
armv7m decoder by `run_asm_roundtrip.sh` / `compare_objdump.py`. Standard library only.

Usage: gen_asm.py > cases.s
"""
import itertools
import random

random.seed(12345)
out = []


def emit(s):
    out.append("    " + s)


LOW = ["r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7"]
ANY = LOW + ["r8", "r9", "r10", "r11", "r12"]
ANYLR = ANY + ["lr"]
CONDS = ["eq", "ne", "cs", "cc", "mi", "pl", "vs", "vc", "hi", "ls", "ge", "lt", "gt", "le"]
# Valid Thumb-2 modified immediates.
MODIMM = [0, 1, 0x7F, 0xFF, 0x100, 0x3FC, 0x3FC0, 0xFF0, 0xFF000, 0x1FE00, 0x3FC00000, 0x80000000, 0xFF000000,
          0x00AB00AB, 0xAB00AB00, 0xABABABAB, 0x00FF00FF, 0xFF00FF00, 0x7F800000, 0x1FE]


def rr(pool=ANY, n=1):
    return random.sample(pool, n)


def sample_regs(k):
    return [random.choice(ANY) for _ in range(k)]


def gen():
    emit(".syntax unified")
    emit(".thumb")
    emit(".cpu cortex-m4")
    emit(".fpu fpv4-sp-d16")
    emit(".text")
    emit("start:")

    # ---- data processing, modified immediate --------------------------------
    for mnem in ["and", "orr", "eor", "bic", "orn", "adc", "sbc", "sub", "rsb", "add"]:
        for imm in MODIMM:
            for s in ("", "s"):
                d, n = sample_regs(2)
                emit(f"{mnem}{s}.w {d}, {n}, #{imm}")
    for mnem in ["mov", "mvn"]:
        for imm in MODIMM:
            for s in ("", "s"):
                emit(f"{mnem}{s}.w {random.choice(ANY)}, #{imm}")
    for mnem in ["tst", "teq", "cmp", "cmn"]:
        for imm in MODIMM:
            emit(f"{mnem}.w {random.choice(ANY)}, #{imm}")
    for imm in [0, 1, 0xFFF, 0x800, 0x7FF, 0x123, 0xABC]:
        for mnem in ["addw", "subw"]:
            emit(f"{mnem} {random.choice(ANY)}, {random.choice(ANY)}, #{imm}")
    for imm in [0, 0x1234, 0xFFFF, 0x8000, 0x00FF, 0xFF00]:
        emit(f"movw {random.choice(ANY)}, #{imm}")
        emit(f"movt {random.choice(ANY)}, #{imm}")

    # ---- 16-bit immediate forms ------------------------------------------------
    for r in LOW:
        for imm in [0, 1, 7, 8, 100, 255]:
            emit(f"movs {r}, #{imm}")
            emit(f"cmp {r}, #{imm}")
            emit(f"adds {r}, #{imm}")
            emit(f"subs {r}, #{imm}")
    for imm in [0, 1, 7]:
        emit(f"adds {random.choice(LOW)}, {random.choice(LOW)}, #{imm}")
        emit(f"subs {random.choice(LOW)}, {random.choice(LOW)}, #{imm}")
    for imm in [0, 4, 508, 16, 128]:
        emit(f"add sp, sp, #{imm}")
        emit(f"sub sp, sp, #{imm}")
        emit(f"add {random.choice(LOW)}, sp, #{imm}")
        emit(f"add.w {random.choice(ANY)}, sp, #{imm}")
    for imm in [0, 32, 1020, 4, 12]:
        emit(f"add {random.choice(LOW)}, sp, #{imm}")
    emit("rsbs r1, r2, #0")
    emit("rsb.w r1, r2, #0")
    emit("rsb.w r8, r9, #0x100")

    # ---- data processing, register (shifted) -------------------------------------
    shifts = ["", ", lsl #1", ", lsl #31", ", lsr #1", ", lsr #32", ", lsr #31", ", asr #1", ", asr #32", ", asr #7", ", ror #1", ", ror #31", ", rrx"]
    for mnem in ["and", "orr", "eor", "bic", "orn", "adc", "sbc", "sub", "rsb", "add"]:
        for sh in shifts:
            for s in ("", "s"):
                d, n, m = sample_regs(3)
                emit(f"{mnem}{s}.w {d}, {n}, {m}{sh}")
    for sh in shifts:
        for s in ("", "s"):
            d, m = sample_regs(2)
            emit(f"mvn{s}.w {d}, {m}{sh}")
    for s in ("", "s"):
        d, m = sample_regs(2)
        emit(f"mov{s}.w {d}, {m}")
    for mnem in ["tst", "teq", "cmp", "cmn"]:
        for sh in shifts:
            n, m = sample_regs(2)
            emit(f"{mnem}.w {n}, {m}{sh}")
    # 16-bit register forms
    for mnem in ["ands", "eors", "adcs", "sbcs", "orrs", "bics", "mvns", "tst", "cmn", "cmp", "muls"]:
        d, m = rr(LOW, 2)
        emit(f"{mnem} {d}, {m}")
    for mnem in ["adds", "subs"]:
        d, n, m = sample_regs(3)
        emit(f"{mnem} {random.choice(LOW)}, {random.choice(LOW)}, {random.choice(LOW)}")
    for h in ["r8", "r12", "lr"]:
        emit(f"mov {h}, r1")
        emit(f"mov r2, {h}")
        emit(f"add r3, {h}")
        emit(f"add {h}, r3")
        emit(f"cmp r3, {h}")
        emit(f"cmp {h}, r3")
    emit("mov r8, sp")
    emit("mov sp, r8")
    emit("movs r1, r2")
    emit("add sp, r1")
    emit("add r1, sp")

    # ---- shifts ---------------------------------------------------------------------
    for mnem, rng in [("lsl", [1, 2, 15, 31]), ("lsr", [1, 2, 15, 31, 32]), ("asr", [1, 2, 15, 31, 32]), ("ror", [1, 2, 15, 31])]:
        for amt in rng:
            emit(f"{mnem}s {random.choice(LOW)}, {random.choice(LOW)}, #{amt}")
            emit(f"{mnem}.w {random.choice(ANY)}, {random.choice(ANY)}, #{amt}")
            emit(f"{mnem}s.w {random.choice(ANY)}, {random.choice(ANY)}, #{amt}")
        d, m = rr(LOW, 2)
        emit(f"{mnem}s {d}, {m}")
        emit(f"{mnem}.w {random.choice(ANY)}, {random.choice(ANY)}, {random.choice(ANY)}")
        emit(f"{mnem}s.w {random.choice(ANY)}, {random.choice(ANY)}, {random.choice(ANY)}")
    emit("rrx r1, r2")
    emit("rrxs r3, r4")

    # ---- multiply, divide -------------------------------------------------------------
    for _ in range(4):
        d, n, m, a = sample_regs(4)
        emit(f"mul {d}, {n}, {m}")
        emit(f"mla {d}, {n}, {m}, {a}")
        emit(f"mls {d}, {n}, {m}, {a}")
        emit(f"sdiv {d}, {n}, {m}")
        emit(f"udiv {d}, {n}, {m}")
        lo, hi = rr(ANY, 2)
        for mn in ["umull", "smull", "umlal", "smlal", "umaal"]:
            emit(f"{mn} {lo}, {hi}, {n}, {m}")

    # ---- extend, bit manipulation -------------------------------------------------------
    for mn in ["sxtb", "sxth", "uxtb", "uxth"]:
        emit(f"{mn} {random.choice(LOW)}, {random.choice(LOW)}")
        for rot in ["", ", ror #8", ", ror #16", ", ror #24"]:
            emit(f"{mn}.w {random.choice(ANY)}, {random.choice(ANY)}{rot}")
    for mn in ["sxtab", "sxtah", "uxtab", "uxtah", "sxtab16", "uxtab16"]:
        for rot in ["", ", ror #8", ", ror #16", ", ror #24"]:
            emit(f"{mn} {random.choice(ANY)}, {random.choice(ANY)}, {random.choice(ANY)}{rot}")
    for mn in ["sxtb16", "uxtb16"]:
        for rot in ["", ", ror #8", ", ror #16", ", ror #24"]:
            emit(f"{mn} {random.choice(ANY)}, {random.choice(ANY)}{rot}")
    for mn in ["rev", "rev16", "revsh"]:
        emit(f"{mn} {random.choice(LOW)}, {random.choice(LOW)}")
        emit(f"{mn}.w {random.choice(ANY)}, {random.choice(ANY)}")
    for mn in ["rbit", "clz"]:
        emit(f"{mn} {random.choice(ANY)}, {random.choice(ANY)}")
    for lsb, w in [(0, 1), (0, 32 - 0), (4, 8), (31, 1), (16, 16), (1, 31)]:
        if lsb + w <= 32:
            emit(f"bfi {random.choice(ANY)}, {random.choice(ANY)}, #{lsb}, #{w}")
            emit(f"bfc {random.choice(ANY)}, #{lsb}, #{w}")
            emit(f"ubfx {random.choice(ANY)}, {random.choice(ANY)}, #{lsb}, #{w}")
            emit(f"sbfx {random.choice(ANY)}, {random.choice(ANY)}, #{lsb}, #{w}")
    for sat in [1, 8, 16, 31, 32]:
        for sh in ["", ", lsl #1", ", lsl #31", ", asr #1", ", asr #31"]:
            emit(f"ssat {random.choice(ANY)}, #{sat}, {random.choice(ANY)}{sh}")
    for sat in [0, 1, 8, 16, 31]:
        for sh in ["", ", lsl #1", ", lsl #31", ", asr #1", ", asr #31"]:
            emit(f"usat {random.choice(ANY)}, #{sat}, {random.choice(ANY)}{sh}")
    for sat in [1, 8, 16]:
        emit(f"ssat16 {random.choice(ANY)}, #{sat}, {random.choice(ANY)}")
    for sat in [0, 8, 15]:
        emit(f"usat16 {random.choice(ANY)}, #{sat}, {random.choice(ANY)}")
    for mn in ["qadd", "qsub", "qdadd", "qdsub"]:
        emit(f"{mn} {random.choice(ANY)}, {random.choice(ANY)}, {random.choice(ANY)}")
    for sh in [0, 1, 8, 31]:
        emit(f"pkhbt {random.choice(ANY)}, {random.choice(ANY)}, {random.choice(ANY)}, lsl #{sh}" if sh else f"pkhbt {random.choice(ANY)}, {random.choice(ANY)}, {random.choice(ANY)}")
    for sh in [1, 8, 31, 32]:
        emit(f"pkhtb {random.choice(ANY)}, {random.choice(ANY)}, {random.choice(ANY)}, asr #{sh}")
    for prefix in ["s", "q", "sh", "u", "uq", "uh"]:
        for op in ["add16", "asx", "sax", "sub16", "add8", "sub8"]:
            d, n, m = sample_regs(3)
            emit(f"{prefix}{op} {d}, {n}, {m}")
    emit("usad8 r1, r2, r3")
    emit("usada8 r1, r2, r3, r4")
    emit("sel r5, r6, r7")

    # ---- DSP multiplies -------------------------------------------------------------------------
    for xy in ["bb", "bt", "tb", "tt"]:
        d, n, m, a = sample_regs(4)
        emit(f"smul{xy} {d}, {n}, {m}")
        emit(f"smla{xy} {d}, {n}, {m}, {a}")
        lo, hi = rr(ANY, 2)
        emit(f"smlal{xy} {lo}, {hi}, {n}, {m}")
    for y in ["b", "t"]:
        d, n, m, a = sample_regs(4)
        emit(f"smulw{y} {d}, {n}, {m}")
        emit(f"smlaw{y} {d}, {n}, {m}, {a}")
    for x in ["", "x"]:
        d, n, m, a = sample_regs(4)
        lo, hi = rr(ANY, 2)
        emit(f"smuad{x} {d}, {n}, {m}")
        emit(f"smusd{x} {d}, {n}, {m}")
        emit(f"smlad{x} {d}, {n}, {m}, {a}")
        emit(f"smlsd{x} {d}, {n}, {m}, {a}")
        emit(f"smlald{x} {lo}, {hi}, {n}, {m}")
        emit(f"smlsld{x} {lo}, {hi}, {n}, {m}")
    for rnd in ["", "r"]:
        d, n, m, a = sample_regs(4)
        emit(f"smmul{rnd} {d}, {n}, {m}")
        emit(f"smmla{rnd} {d}, {n}, {m}, {a}")
        emit(f"smmls{rnd} {d}, {n}, {m}, {a}")

    # ---- loads and stores ----------------------------------------------------------------------------
    for mn, imm5, imm12 in [("ldr", [0, 4, 124], [128, 4095, 2048]), ("str", [0, 4, 124], [128, 4095, 2048]),
                            ("ldrb", [0, 1, 31], [32, 4095]), ("strb", [0, 1, 31], [32, 4095]),
                            ("ldrh", [0, 2, 62], [64, 4094, 1]), ("strh", [0, 2, 62], [64, 4094, 1]),
                            ("ldrsb", [], [0, 1, 4095]), ("ldrsh", [], [0, 2, 4094])]:
        for imm in imm5:
            emit(f"{mn} {random.choice(LOW)}, [{random.choice(LOW)}, #{imm}]")
        for imm in imm12:
            emit(f"{mn}.w {random.choice(ANY)}, [{random.choice(ANY)}, #{imm}]")
        for imm in [1, 255, 4]:
            emit(f"{mn} {random.choice(ANY)}, [{random.choice(ANY)}, #-{imm}]")
            for fmt in ("{mn} {t}, [{n}, #{imm}]!", "{mn} {t}, [{n}, #-{imm}]!", "{mn} {t}, [{n}], #{imm}", "{mn} {t}, [{n}], #-{imm}"):
                t, nn = rr(ANY, 2)
                emit(fmt.format(mn=mn, t=t, n=nn, imm=imm))
        for sh in ["", ", lsl #1", ", lsl #2", ", lsl #3"]:
            emit(f"{mn}.w {random.choice(ANY)}, [{random.choice(ANY)}, {random.choice(ANY)}{sh}]")
        emit(f"{mn} {random.choice(LOW)}, [{random.choice(LOW)}, {random.choice(LOW)}]")
    for imm in [0, 4, 1020]:
        emit(f"ldr {random.choice(LOW)}, [sp, #{imm}]")
        emit(f"str {random.choice(LOW)}, [sp, #{imm}]")
    for mn in ["ldr", "ldrb", "ldrh", "ldrsb", "ldrsh"]:
        emit(f"{mn}.w {random.choice(ANY)}, [pc, #8]")
        emit(f"{mn}.w {random.choice(ANY)}, [pc, #-8]")
        emit(f"{mn}.w {random.choice(ANY)}, [pc, #4000]")
    emit("ldr r1, [pc, #0]")
    emit("ldr r1, [pc, #1020]")
    for mn in ["ldrt", "ldrbt", "ldrht", "ldrsbt", "ldrsht", "strt", "strbt", "strht"]:
        emit(f"{mn} {random.choice(ANY)}, [{random.choice(ANY)}, #{random.choice([0, 4, 255])}]")
    for mn in ["ldrd", "strd"]:
        for off in [0, 4, 1020, -8, -1020]:
            rt, rt2, rn = rr(ANY, 3)
            emit(f"{mn} {rt}, {rt2}, [{rn}, #{off}]")
            emit(f"{mn} {rt}, {rt2}, [{rn}, #{off}]!")
            emit(f"{mn} {rt}, {rt2}, [{rn}], #{off}")
    emit("ldrd r0, r1, [pc, #8]")
    emit("ldrd r2, r3, [pc, #-16]")
    for lst in ["{r0}", "{r0, r2, r3}", "{r4, r5, r6, r7}", "{r0, r7}"]:
        for wb in ("", "!"):
            emit(f"ldmia r1{wb}, {lst}")
            emit(f"stmia r1{wb}, {lst}")
    for lst in ["{r0, r1}", "{r4, r8, r12}", "{r0, r1, r2, r3, r4, r5, r6, r7, r8, r10, r11, r12}", "{r2, lr}", "{r0, pc}", "{r4, r5, r6, r7, r8, lr}"]:
        for wb in ("", "!"):
            if "pc" in lst:
                emit(f"ldmia.w r9{wb}, {lst}")
                emit(f"ldmdb r9{wb}, {lst}")
            else:
                emit(f"ldmia.w r9{wb}, {lst}")
                emit(f"stmia.w r9{wb}, {lst}")
                emit(f"ldmdb r9{wb}, {lst}")
                emit(f"stmdb r9{wb}, {lst}")
    for lst in ["{r4}", "{r4, lr}", "{r0, r1, r2, r3}", "{r4, r5, r6, r7, lr}", "{r4, r8, lr}", "{r0, r1, r2, r3, r4, r5, r6, r7, r8, r9, r10, r11, r12, lr}"]:
        emit(f"push {lst}")
        emit(f"push.w {lst}")
    for lst in ["{r4}", "{r4, pc}", "{r0, r1, r2, r3}", "{r4, r5, r6, r7, pc}", "{r4, r8, pc}", "{r4, r8, lr}"]:
        emit(f"pop {lst}")
        emit(f"pop.w {lst}")
    emit("ldr.w pc, [r0, #4]")
    emit("ldr.w pc, [sp], #4")
    emit("ldr.w pc, [r1, r2, lsl #2]")
    emit("ldr.w pc, [pc, #16]")
    for imm in [0, 4, 1020]:
        emit(f"ldrex {random.choice(ANY)}, [{random.choice(ANY)}, #{imm}]")
        d, t, n = rr(ANY, 3)
        emit(f"strex {d}, {t}, [{n}, #{imm}]")
    emit("ldrexb r1, [r2]")
    emit("ldrexh r1, [r2]")
    emit("strexb r1, r2, [r3]")
    emit("strexh r1, r2, [r3]")
    emit("clrex")
    emit("tbb [pc, r0]")
    emit("tbh [pc, r1, lsl #1]")
    emit("tbb [r3, r4]")
    emit("tbh [r3, r4, lsl #1]")

    # ---- branches ---------------------------------------------------------------------------------------------
    n = 0
    for pad in [0, 2, 60, 250, 1000, 2046]:
        n += 1
        emit(f"b.n 1{n}f" if pad <= 2046 else f"b.w 1{n}f")
        if pad:
            emit(f".space {pad}")
        out.append(f"1{n}:")
    for pad in [0, 4, 3000, 60000, 1000000]:
        n += 1
        emit(f"b.w 1{n}f")
        emit(f".space {pad}")
        out.append(f"1{n}:")
    for cond in CONDS:
        n += 1
        emit(f"b{cond}.n 1{n}f")
        emit(".space 100")
        out.append(f"1{n}:")
        n += 1
        emit(f"b{cond}.w 1{n}f")
        emit(".space 2000")
        out.append(f"1{n}:")
    for pad in [0, 2, 100, 5000, 1000000]:
        n += 1
        emit(f"bl 1{n}f")
        emit(f".space {pad}")
        out.append(f"1{n}:")
    # backward branches
    for pad in [0, 4, 100, 1000]:
        n += 1
        out.append(f"1{n}:")
        emit(f".space {pad}")
        emit(f"b.n 1{n}b" if pad < 1000 else f"b.w 1{n}b")
        n += 1
        out.append(f"1{n}:")
        emit(f".space {pad}")
        emit(f"bne.w 1{n}b")
        n += 1
        out.append(f"1{n}:")
        emit(f".space {pad}")
        emit(f"bl 1{n}b")
    for r in LOW:
        n += 1
        emit(f"cbz {r}, 1{n}f")
        emit(".space 4")
        out.append(f"1{n}:")
        n += 1
        emit(f"cbnz {r}, 1{n}f")
        emit(".space 120")
        out.append(f"1{n}:")
    for r in ANY + ["lr", "sp"]:
        emit(f"bx {r}")
        if r not in ("sp", "pc"):
            emit(f"blx {r}")

    # ---- system ---------------------------------------------------------------------------------------------------
    for imm in [0, 1, 127, 255]:
        emit(f"svc #{imm}")
        emit(f"bkpt #{imm}")
        emit(f"udf #{imm}")
    for sysreg in ["apsr", "iapsr", "eapsr", "xpsr", "ipsr", "epsr", "iepsr", "msp", "psp", "primask", "basepri", "basepri_max", "faultmask", "control"]:
        emit(f"mrs {random.choice(ANY)}, {sysreg}")
    for sysreg in ["apsr_nzcvq", "apsr_g", "apsr_nzcvqg", "msp", "psp", "primask", "basepri", "basepri_max", "faultmask", "control"]:
        emit(f"msr {sysreg}, {random.choice(ANY)}")
    for f in ["i", "f", "if"]:
        emit(f"cpsid {f}")
        emit(f"cpsie {f}")
    for mn in ["nop", "yield", "wfe", "wfi", "sev", "nop.w", "yield.w", "wfe.w", "wfi.w", "sev.w", "dmb", "dsb", "isb", "dmb sy", "dsb sy", "isb sy", "dbg #3"]:
        emit(mn)

    # ---- IT blocks ---------------------------------------------------------------------------------------------------
    body = ["add{c} r0, r1, r2", "mov{c} r3, #5", "ldr{c} r1, [r2, #4]", "str{c} r1, [r2]", "cmp{c} r0, #1", "and{c} r1, r2", "orr{c}.w r8, r9, #4", "sub{c}s r0, r1, #1"]
    for cond in CONDS:
        for pattern in ["", "t", "e", "tt", "te", "et", "ee", "ttt", "tte", "tet", "tee", "ett", "ete", "eet", "eee"]:
            n_ins = len(pattern) + 1
            emit(f"it{pattern} {cond}")
            idx = CONDS.index(cond)
            inv = CONDS[idx ^ 1]
            conds = [cond] + [(cond if ch == "t" else inv) for ch in pattern]
            for i in range(n_ins):
                text = random.choice(body).format(c=conds[i])
                # `bne` style flags: 16-bit adds inside IT must not use the 's' encoding; let the assembler pick
                text = text.replace("sub" + conds[i] + "s", "sub" + conds[i])
                emit(text)

    # 16-bit flag-setting forms inside IT blocks (S suppressed)
    emit("ite ne")
    emit("addne r1, r2, r3")
    emit("moveq r0, #7")
    emit("itt cs")
    emit("lslcs r0, r1, #3")
    emit("mvncs r2, r3")


if __name__ == "__main__":
    gen()
    print("\n".join(out))
