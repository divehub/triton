//! Debug disassembler for predecoded instructions (GNU/UAL style, full operand
//! forms). Used by traces and by the decoder validation against
//! `arm-none-eabi-objdump`. It tracks IT blocks to print condition suffixes.

use crate::alu::{SHIFT_ASR, SHIFT_LSL, SHIFT_LSR, SHIFT_ROR, SHIFT_RRX};
use crate::decode;
use crate::op::*;

const REGS: [&str; 16] = ["r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11", "r12", "sp", "lr", "pc"];
const CONDS: [&str; 16] = ["eq", "ne", "cs", "cc", "mi", "pl", "vs", "vc", "hi", "ls", "ge", "lt", "gt", "le", "", "nv"];

fn r(n: u8) -> &'static str {
    REGS[(n & 15) as usize]
}

fn imm(v: i64) -> String {
    if v.abs() < 10 {
        format!("#{v}")
    } else if v < 0 {
        format!("#-0x{:x}", -v)
    } else {
        format!("#0x{v:x}")
    }
}

fn reglist(list: u32) -> String {
    let regs: Vec<&str> = (0..16).filter(|i| list & (1 << i) != 0).map(|i| REGS[i]).collect();
    format!("{{{}}}", regs.join(", "))
}

fn shift_suffix(ty: u8, amt: u8) -> String {
    match ty {
        SHIFT_LSL if amt == 0 => String::new(),
        SHIFT_LSL => format!(", lsl #{amt}"),
        SHIFT_LSR => format!(", lsr #{amt}"),
        SHIFT_ASR => format!(", asr #{amt}"),
        SHIFT_ROR => format!(", ror #{amt}"),
        SHIFT_RRX => ", rrx".to_string(),
        _ => String::new(),
    }
}

/// Disassembler with IT-block tracking.
#[derive(Default)]
pub struct Disassembler {
    itstate: u8,
}

impl Disassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Disassembles one instruction; returns `(text, length in bytes)`.
    pub fn next(&mut self, pc: u32, hw1: u16, hw2: u16) -> (String, usize) {
        let op = decode::decode(pc, hw1, hw2);
        let len = op.len as usize;
        let in_it = self.itstate != 0;
        let cond = if in_it { Some((self.itstate >> 4) as usize) } else { None };
        let mut op2 = op;
        if in_it && op.flags & FL_IT != 0 {
            op2.flags &= !FL_S;
        }
        let mut text = format_op(pc, &op2, hw1, hw2, cond);
        if op.kind == Kind::It {
            self.itstate = op.imm as u8;
            text = it_text(op.imm as u8);
        } else if in_it {
            if self.itstate & 7 == 0 {
                self.itstate = 0;
            } else {
                self.itstate = (self.itstate & 0xE0) | ((self.itstate << 1) & 0x1F);
            }
        }
        (text, len)
    }
}

/// Disassembles a single instruction without IT context.
pub fn disassemble(pc: u32, hw1: u16, hw2: u16) -> (String, usize) {
    Disassembler::new().next(pc, hw1, hw2)
}

fn it_text(it: u8) -> String {
    let first = (it >> 4) as usize;
    let mask = it & 0xF;
    if mask == 0 {
        return "it ?".to_string();
    }
    let n = 4 - mask.trailing_zeros();
    let mut s = String::from("it");
    for i in 1..n {
        let bit = (mask >> (4 - i)) & 1;
        s.push(if bit as usize == first & 1 { 't' } else { 'e' });
    }
    format!("{} {}", s, CONDS[first])
}

fn format_op(pc: u32, op: &Op, hw1: u16, hw2: u16, cond: Option<usize>) -> String {
    use Kind::*;
    let cs = cond.map(|c| CONDS[c & 15]).unwrap_or("");
    let s = if op.flags & FL_S != 0 { "s" } else { "" };
    let rd = r(op.rd);
    let rn = r(op.rn);
    let rm = r(op.rm);
    let ra = r(op.ra);
    let sh = shift_suffix(op.ra, op.x);
    let imm_u = op.imm as i32 as i64;
    let m = |base: &str| format!("{base}{cs}");
    let ms = |base: &str| format!("{base}{s}{cs}");
    // ADR and PC-reading encodings are rewritten by the decoder; print them as written.
    if op.len == 2 {
        let h = hw1 as u32;
        if h & 0xF800 == 0xA000 {
            return format!("{} {}, 0x{:08x}", m("adr"), r(((h >> 8) & 7) as u8), op.imm);
        }
        if h & 0xFF78 == 0x4678 {
            return format!("{} {}, pc", m("mov"), r((h & 7) as u8 | ((h >> 4) & 8) as u8));
        }
        if h & 0xFF78 == 0x4478 {
            return format!("{} {}, pc", m("add"), r((h & 7) as u8 | ((h >> 4) & 8) as u8));
        }
        if h & 0xFF78 == 0x4578 {
            return format!("{} {}, pc", m("cmp"), r((h & 7) as u8 | ((h >> 4) & 8) as u8));
        }
    } else {
        let (h1, h2) = (hw1 as u32, hw2 as u32);
        if (h1 & 0xFBFF == 0xF20F || h1 & 0xFBFF == 0xF2AF) && h2 & 0x8000 == 0 {
            return format!("{} {}, 0x{:08x}", m("adr"), r(((h2 >> 8) & 0xF) as u8), op.imm);
        }
    }
    let mem = |base: &str, rt: &str, size_label: &str| -> String {
        let _ = size_label;
        let off = imm_u;
        let idx = op.flags & FL_IDX != 0;
        let wb = op.flags & FL_WB != 0;
        if idx && !wb {
            if off == 0 {
                format!("{} {}, [{}]", m(base), rt, rn)
            } else {
                format!("{} {}, [{}, {}]", m(base), rt, rn, imm(off))
            }
        } else if idx && wb {
            format!("{} {}, [{}, {}]!", m(base), rt, rn, imm(off))
        } else {
            format!("{} {}, [{}], {}", m(base), rt, rn, imm(off))
        }
    };
    let reg_mem = |base: &str| -> String {
        if op.x == 0 {
            format!("{} {}, [{}, {}]", m(base), rd, rn, rm)
        } else {
            format!("{} {}, [{}, {}, lsl #{}]", m(base), rd, rn, rm, op.x)
        }
    };
    let lit = |base: &str| -> String {
        let off = op.imm as i64 - ((pc.wrapping_add(4)) & !3) as i64;
        format!("{} {}, [pc, {}]", m(base), rd, imm(off))
    };
    let par_names = ["add16", "asx", "sax", "sub16", "add8", "sub8"];
    let par_prefix = ["s", "q", "sh", "u", "uq", "uh"];
    match op.kind {
        Undecoded | CutHead | Undefined | PageEnd => format!("undefined (0x{:08x})", op.raw),
        Coproc | Nocp | Vfp | VfpEnd => format!("{}{}", crate::vfp::disassemble(hw1, hw2), if cs.is_empty() { "".to_string() } else { format!(" ; {cs}") }),
        Barrier => {
            let name = match (hw2 >> 4) & 0xF {
                4 => "dsb",
                5 => "dmb",
                _ => "isb",
            };
            let option = match hw2 & 0xF {
                0xF => "sy".to_string(),
                0xE => "st".to_string(),
                0xB => "ish".to_string(),
                0xA => "ishst".to_string(),
                0x7 => "nsh".to_string(),
                0x6 => "nshst".to_string(),
                0x3 => "osh".to_string(),
                0x2 => "oshst".to_string(),
                n => format!("#{n}"),
            };
            format!("{} {}", m(name), option)
        }
        BSelf => format!("{} 0x{:08x}", m("b"), op.imm),
        Nop => m("nop"),
        Wfi => m("wfi"),
        Wfe => m("wfe"),
        Sev => m("sev"),
        Svc => format!("{} {}", m("svc"), imm(op.imm as i64)),
        Bkpt => format!("{} {}", m("bkpt"), imm(op.imm as i64)),
        Cps => {
            let mut flags = String::new();
            if op.x & 2 != 0 {
                flags.push('i');
            }
            if op.x & 1 != 0 {
                flags.push('f');
            }
            format!("cps{} {}", if op.x & 0x10 != 0 { "id" } else { "ie" }, flags)
        }
        Mrs => format!("{} {}, {}", m("mrs"), rd, sysreg(op.imm)),
        Msr => format!("{} {}, {}", m("msr"), sysreg_w(op.imm, op.x), rn),
        Clrex => m("clrex"),
        It => it_text(op.imm as u8),
        B => format!("{} 0x{:08x}", m("b"), op.imm),
        Bcc => format!("b{} 0x{:08x}", CONDS[(op.x & 15) as usize], op.imm),
        Bl => format!("{} 0x{:08x}", m("bl"), op.imm),
        Bx => format!("{} {}", m("bx"), rm),
        Blx => format!("{} {}", m("blx"), rm),
        Cbz => format!("cbz {}, 0x{:08x}", rn, op.imm),
        Cbnz => format!("cbnz {}, 0x{:08x}", rn, op.imm),
        Tbb => format!("{} [{}, {}]", m("tbb"), if op.rn == 15 { "pc" } else { rn }, rm),
        Tbh => format!("{} [{}, {}, lsl #1]", m("tbh"), if op.rn == 15 { "pc" } else { rn }, rm),
        MovToPc => format!("{} pc, {}", m("mov"), rm),
        AddToPc => format!("{} pc, pc, {}", m("add"), rm),
        MovImm => format!("{} {}, {}", ms("mov"), rd, imm(op.imm as i64)),
        MvnImm => format!("{} {}, {}", ms("mvn"), rd, imm(op.imm as i64)),
        Movw => format!("{} {}, {}", m("movw"), rd, imm(op.imm as i64)),
        Movt => format!("{} {}, {}", m("movt"), rd, imm((op.imm >> 16) as i64)),
        AndImm => format!("{} {}, {}, {}", ms("and"), rd, rn, imm(op.imm as i64)),
        BicImm => format!("{} {}, {}, {}", ms("bic"), rd, rn, imm(op.imm as i64)),
        OrrImm => format!("{} {}, {}, {}", ms("orr"), rd, rn, imm(op.imm as i64)),
        OrnImm => format!("{} {}, {}, {}", ms("orn"), rd, rn, imm(op.imm as i64)),
        EorImm => format!("{} {}, {}, {}", ms("eor"), rd, rn, imm(op.imm as i64)),
        TstImm => format!("{} {}, {}", m("tst"), rn, imm(op.imm as i64)),
        TeqImm => format!("{} {}, {}", m("teq"), rn, imm(op.imm as i64)),
        AddImm => {
            if op.len == 4 && op.rn == 15 {
                format!("{} {}, pc, {}", m("addw"), rd, imm(op.imm as i64))
            } else {
                format!("{} {}, {}, {}", ms("add"), rd, rn, imm(op.imm as i64))
            }
        }
        AdcImm => format!("{} {}, {}, {}", ms("adc"), rd, rn, imm(op.imm as i64)),
        SubImm => format!("{} {}, {}, {}", ms("sub"), rd, rn, imm(op.imm as i64)),
        SbcImm => format!("{} {}, {}, {}", ms("sbc"), rd, rn, imm(op.imm as i64)),
        RsbImm => format!("{} {}, {}, {}", ms("rsb"), rd, rn, imm(op.imm as i64)),
        CmpImm => format!("{} {}, {}", m("cmp"), rn, imm(op.imm as i64)),
        CmnImm => format!("{} {}, {}", m("cmn"), rn, imm(op.imm as i64)),
        MovReg => format!("{} {}, {}", ms("mov"), rd, rm),
        MvnReg => format!("{} {}, {}{}", ms("mvn"), rd, rm, sh),
        AndReg => format!("{} {}, {}, {}{}", ms("and"), rd, rn, rm, sh),
        BicReg => format!("{} {}, {}, {}{}", ms("bic"), rd, rn, rm, sh),
        OrrReg => format!("{} {}, {}, {}{}", ms("orr"), rd, rn, rm, sh),
        OrnReg => format!("{} {}, {}, {}{}", ms("orn"), rd, rn, rm, sh),
        EorReg => format!("{} {}, {}, {}{}", ms("eor"), rd, rn, rm, sh),
        TstReg => format!("{} {}, {}{}", m("tst"), rn, rm, sh),
        TeqReg => format!("{} {}, {}{}", m("teq"), rn, rm, sh),
        AddReg => format!("{} {}, {}, {}{}", ms("add"), rd, rn, rm, sh),
        AdcReg => format!("{} {}, {}, {}{}", ms("adc"), rd, rn, rm, sh),
        SubReg => format!("{} {}, {}, {}{}", ms("sub"), rd, rn, rm, sh),
        SbcReg => format!("{} {}, {}, {}{}", ms("sbc"), rd, rn, rm, sh),
        RsbReg => format!("{} {}, {}, {}{}", ms("rsb"), rd, rn, rm, sh),
        CmpReg => format!("{} {}, {}{}", m("cmp"), rn, rm, sh),
        CmnReg => format!("{} {}, {}{}", m("cmn"), rn, rm, sh),
        LslImm => format!("{} {}, {}, #{}", ms("lsl"), rd, rm, op.x),
        LsrImm => format!("{} {}, {}, #{}", ms("lsr"), rd, rm, op.x),
        AsrImm => format!("{} {}, {}, #{}", ms("asr"), rd, rm, op.x),
        RorImm => format!("{} {}, {}, #{}", ms("ror"), rd, rm, op.x),
        Rrx => format!("{} {}, {}", ms("rrx"), rd, rm),
        LslReg => format!("{} {}, {}, {}", ms("lsl"), rd, rn, rm),
        LsrReg => format!("{} {}, {}, {}", ms("lsr"), rd, rn, rm),
        AsrReg => format!("{} {}, {}, {}", ms("asr"), rd, rn, rm),
        RorReg => format!("{} {}, {}, {}", ms("ror"), rd, rn, rm),
        Mul => format!("{} {}, {}, {}", ms("mul"), rd, rn, rm),
        Mla => format!("{} {}, {}, {}, {}", m("mla"), rd, rn, rm, ra),
        Mls => format!("{} {}, {}, {}, {}", m("mls"), rd, rn, rm, ra),
        Umull => format!("{} {}, {}, {}, {}", m("umull"), rd, ra, rn, rm),
        Smull => format!("{} {}, {}, {}, {}", m("smull"), rd, ra, rn, rm),
        Umlal => format!("{} {}, {}, {}, {}", m("umlal"), rd, ra, rn, rm),
        Smlal => format!("{} {}, {}, {}, {}", m("smlal"), rd, ra, rn, rm),
        Umaal => format!("{} {}, {}, {}, {}", m("umaal"), rd, ra, rn, rm),
        Sdiv => format!("{} {}, {}, {}", m("sdiv"), rd, rn, rm),
        Udiv => format!("{} {}, {}, {}", m("udiv"), rd, rn, rm),
        Sxtb | Sxth | Uxtb | Uxth | Sxtb16 | Uxtb16 => {
            let n = match op.kind {
                Sxtb => "sxtb",
                Sxth => "sxth",
                Uxtb => "uxtb",
                Uxth => "uxth",
                Sxtb16 => "sxtb16",
                _ => "uxtb16",
            };
            let rot = if op.x != 0 { format!(", ror #{}", op.x) } else { String::new() };
            format!("{} {}, {}{}", m(n), rd, rm, rot)
        }
        Sxtab | Sxtah | Uxtab | Uxtah | Sxtab16 | Uxtab16 => {
            let n = match op.kind {
                Sxtab => "sxtab",
                Sxtah => "sxtah",
                Uxtab => "uxtab",
                Uxtah => "uxtah",
                Sxtab16 => "sxtab16",
                _ => "uxtab16",
            };
            let rot = if op.x != 0 { format!(", ror #{}", op.x) } else { String::new() };
            format!("{} {}, {}, {}{}", m(n), rd, rn, rm, rot)
        }
        Rev => format!("{} {}, {}", m("rev"), rd, rm),
        Rev16 => format!("{} {}, {}", m("rev16"), rd, rm),
        Revsh => format!("{} {}, {}", m("revsh"), rd, rm),
        Rbit => format!("{} {}, {}", m("rbit"), rd, rm),
        Clz => format!("{} {}, {}", m("clz"), rd, rm),
        Bfi => format!("{} {}, {}, #{}, #{}", m("bfi"), rd, rn, op.x, op.ra as u32 - op.x as u32 + 1),
        Bfc => format!("{} {}, #{}, #{}", m("bfc"), rd, op.x, op.ra as u32 - op.x as u32 + 1),
        Ubfx => format!("{} {}, {}, #{}, #{}", m("ubfx"), rd, rn, op.x, op.ra as u32 + 1),
        Sbfx => format!("{} {}, {}, #{}, #{}", m("sbfx"), rd, rn, op.x, op.ra as u32 + 1),
        Ssat => format!("{} {}, #{}, {}{}", m("ssat"), rd, op.imm, rn, shift_suffix(op.ra, op.x)),
        Usat => format!("{} {}, #{}, {}{}", m("usat"), rd, op.imm, rn, shift_suffix(op.ra, op.x)),
        Ssat16 => format!("{} {}, #{}, {}", m("ssat16"), rd, op.imm, rn),
        Usat16 => format!("{} {}, #{}, {}", m("usat16"), rd, op.imm, rn),
        Qadd => format!("{} {}, {}, {}", m("qadd"), rd, rm, rn),
        Qsub => format!("{} {}, {}, {}", m("qsub"), rd, rm, rn),
        Qdadd => format!("{} {}, {}, {}", m("qdadd"), rd, rm, rn),
        Qdsub => format!("{} {}, {}, {}", m("qdsub"), rd, rm, rn),
        Pkhbt => format!("{} {}, {}, {}{}", m("pkhbt"), rd, rn, rm, shift_suffix(op.ra, op.x)),
        Pkhtb => format!("{} {}, {}, {}, asr #{}", m("pkhtb"), rd, rn, rm, op.x),
        Parallel => {
            let p = (op.x / 6) as usize;
            let o = (op.x % 6) as usize;
            format!("{}{}{} {}, {}, {}", par_prefix[p], par_names[o], cs, rd, rn, rm)
        }
        Usad8 => format!("{} {}, {}, {}", m("usad8"), rd, rn, rm),
        Usada8 => format!("{} {}, {}, {}, {}", m("usada8"), rd, rn, rm, ra),
        Sel => format!("{} {}, {}, {}", m("sel"), rd, rn, rm),
        Smulxy | Smlaxy => {
            let nm = ["bb", "bt", "tb", "tt"][(op.x & 3) as usize];
            if op.kind == Smulxy {
                format!("smul{}{} {}, {}, {}", nm, cs, rd, rn, rm)
            } else {
                format!("smla{}{} {}, {}, {}, {}", nm, cs, rd, rn, rm, ra)
            }
        }
        Smulwy | Smlawy => {
            let nm = if op.x & 1 != 0 { "t" } else { "b" };
            if op.kind == Smulwy {
                format!("smulw{}{} {}, {}, {}", nm, cs, rd, rn, rm)
            } else {
                format!("smlaw{}{} {}, {}, {}, {}", nm, cs, rd, rn, rm, ra)
            }
        }
        Smlalxy => {
            let nm = ["bb", "bt", "tb", "tt"][(op.x & 3) as usize];
            format!("smlal{}{} {}, {}, {}, {}", nm, cs, rd, ra, rn, rm)
        }
        Smuad | Smusd => {
            let x = if op.x & 1 != 0 { "x" } else { "" };
            format!("{}{}{} {}, {}, {}", if op.kind == Smuad { "smuad" } else { "smusd" }, x, cs, rd, rn, rm)
        }
        Smlad | Smlsd => {
            let x = if op.x & 1 != 0 { "x" } else { "" };
            format!("{}{}{} {}, {}, {}, {}", if op.kind == Smlad { "smlad" } else { "smlsd" }, x, cs, rd, rn, rm, ra)
        }
        Smlald | Smlsld => {
            let x = if op.x & 1 != 0 { "x" } else { "" };
            format!("{}{}{} {}, {}, {}, {}", if op.kind == Smlald { "smlald" } else { "smlsld" }, x, cs, rd, ra, rn, rm)
        }
        Smmul => format!("smmul{}{} {}, {}, {}", if op.x & 1 != 0 { "r" } else { "" }, cs, rd, rn, rm),
        Smmla => format!("smmla{}{} {}, {}, {}, {}", if op.x & 1 != 0 { "r" } else { "" }, cs, rd, rn, rm, ra),
        Smmls => format!("smmls{}{} {}, {}, {}, {}", if op.x & 1 != 0 { "r" } else { "" }, cs, rd, rn, rm, ra),
        LdrImm => mem("ldr", rd, ""),
        LdrbImm => mem("ldrb", rd, ""),
        LdrhImm => mem("ldrh", rd, ""),
        LdrsbImm => mem("ldrsb", rd, ""),
        LdrshImm => mem("ldrsh", rd, ""),
        StrImm => mem("str", rd, ""),
        StrbImm => mem("strb", rd, ""),
        StrhImm => mem("strh", rd, ""),
        LdrReg => reg_mem("ldr"),
        LdrbReg => reg_mem("ldrb"),
        LdrhReg => reg_mem("ldrh"),
        LdrsbReg => reg_mem("ldrsb"),
        LdrshReg => reg_mem("ldrsh"),
        StrReg => reg_mem("str"),
        StrbReg => reg_mem("strb"),
        StrhReg => reg_mem("strh"),
        LdrLit => lit("ldr"),
        LdrbLit => lit("ldrb"),
        LdrhLit => lit("ldrh"),
        LdrsbLit => lit("ldrsb"),
        LdrshLit => lit("ldrsh"),
        LdrToPc => match op.x & 15 {
            0 => {
                let o = Op { rd: 15, ..*op };
                format_op(pc, &Op { kind: LdrImm, ..o }, hw1, hw2, cond)
            }
            1 => {
                let o = Op { rd: 15, x: op.x >> 4, ..*op };
                format_op(pc, &Op { kind: LdrReg, ..o }, hw1, hw2, cond)
            }
            _ => {
                let o = Op { rd: 15, ..*op };
                format_op(pc, &Op { kind: LdrLit, ..o }, hw1, hw2, cond)
            }
        },
        LdrdImm | StrdImm => {
            let name = if op.kind == LdrdImm { "ldrd" } else { "strd" };
            if op.rn == 15 {
                let off = op.imm as i64 - ((pc.wrapping_add(4)) & !3) as i64;
                return format!("{} {}, {}, [pc, {}]", m(name), rd, ra, imm(off));
            }
            let idx = op.flags & FL_IDX != 0;
            let wb = op.flags & FL_WB != 0;
            if idx && !wb {
                if op.imm == 0 {
                    format!("{} {}, {}, [{}]", m(name), rd, ra, rn)
                } else {
                    format!("{} {}, {}, [{}, {}]", m(name), rd, ra, rn, imm(imm_u))
                }
            } else if idx && wb {
                format!("{} {}, {}, [{}, {}]!", m(name), rd, ra, rn, imm(imm_u))
            } else {
                format!("{} {}, {}, [{}], {}", m(name), rd, ra, rn, imm(imm_u))
            }
        }
        Ldm | LdmPc | Stm => {
            let base = if op.kind == Stm { "stm" } else { "ldm" };
            let mode = if op.flags & FL_DB != 0 { "db" } else { "ia" };
            let bang = if op.flags & FL_WB != 0 { "!" } else { "" };
            format!("{}{}{} {}{}, {}", base, mode, cs, rn, bang, reglist(op.imm))
        }
        Push => format!("{} {}", m("push"), reglist(op.imm)),
        Pop | PopPc => format!("{} {}", m("pop"), reglist(op.imm)),
        Ldrex => format!("{} {}, [{}{}]", m("ldrex"), rd, rn, if op.imm != 0 { format!(", {}", imm(op.imm as i64)) } else { String::new() }),
        Ldrexb => format!("{} {}, [{}]", m("ldrexb"), rd, rn),
        Ldrexh => format!("{} {}, [{}]", m("ldrexh"), rd, rn),
        Strex => format!("{} {}, {}, [{}{}]", m("strex"), rd, ra, rn, if op.imm != 0 { format!(", {}", imm(op.imm as i64)) } else { String::new() }),
        Strexb => format!("{} {}, {}, [{}]", m("strexb"), rd, ra, rn),
        Strexh => format!("{} {}, {}, [{}]", m("strexh"), rd, ra, rn),
    }
}

fn sysreg(sysm: u32) -> &'static str {
    match sysm {
        0 => "apsr",
        1 => "iapsr",
        2 => "eapsr",
        3 => "xpsr",
        5 => "ipsr",
        6 => "epsr",
        7 => "iepsr",
        8 => "msp",
        9 => "psp",
        16 => "primask",
        17 => "basepri",
        18 => "basepri_max",
        19 => "faultmask",
        20 => "control",
        _ => "?",
    }
}

fn sysreg_w(sysm: u32, mask: u8) -> String {
    match sysm {
        0..=3 => {
            let base = sysreg(sysm);
            let suffix = match mask & 3 {
                2 => "_nzcvq",
                1 => "_g",
                3 => "_nzcvqg",
                _ => "",
            };
            if base == "apsr" {
                format!("{base}{suffix}")
            } else {
                base.to_string()
            }
        }
        _ => sysreg(sysm).to_string(),
    }
}
