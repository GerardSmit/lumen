//! ARM32 (A32) instruction encoding: fixed 32-bit words, labels and PC-relative fixups.
//!
//! Every instruction is one little-endian word. Branches are emitted in their final form and
//! patched in [`Asm::finish`], which fails (and the function is rejected) when a displacement
//! does not fit (±32 MiB for `b`). Only the subset the backend uses is encoded: data processing,
//! `movw`/`movt`, multiplies, loads and stores, `push`/`pop`, and D0..D15/S0..S31 scalar floating
//! point (common to VFPv2 and VFPv3-D16).
//!
//! [`Asm::v7`] selects the instruction set. Without it (ARMv6, e.g. the ARM1176 of the
//! Raspberry Pi 1 and Zero) constants are built with `mov`/`orr` instead of `movw`/`movt`,
//! barriers are the CP15 `mcr` form and `rbit` is never emitted.

pub type Label = usize;

pub const SP: u8 = 13;
pub const LR: u8 = 14;
pub const PC: u8 = 15;
/// Intra-procedure-call scratch register (IP): never allocated.
pub const R12: u8 = 12;
pub const R11: u8 = 11;

/// Condition codes (hardware encoding).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Cond {
    Eq = 0,
    Ne = 1,
    Hs = 2,
    Lo = 3,
    Mi = 4,
    Pl = 5,
    Vs = 6,
    Vc = 7,
    Hi = 8,
    Ls = 9,
    Ge = 10,
    Lt = 11,
    Gt = 12,
    Le = 13,
    Al = 14,
}

impl Cond {
    pub fn invert(self) -> Cond {
        const ALL: [Cond; 14] = [
            Cond::Eq,
            Cond::Ne,
            Cond::Hs,
            Cond::Lo,
            Cond::Mi,
            Cond::Pl,
            Cond::Vs,
            Cond::Vc,
            Cond::Hi,
            Cond::Ls,
            Cond::Ge,
            Cond::Lt,
            Cond::Gt,
            Cond::Le,
        ];
        assert!(self != Cond::Al);
        ALL[self as usize ^ 1]
    }
    fn bits(self) -> u32 {
        (self as u32) << 28
    }
}

/// Data-processing opcodes (bits 24:21).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Dp {
    And = 0,
    Eor = 1,
    Sub = 2,
    Rsb = 3,
    Add = 4,
    Adc = 5,
    Sbc = 6,
    Rsc = 7,
    Tst = 8,
    Teq = 9,
    Cmp = 10,
    Cmn = 11,
    Orr = 12,
    Mov = 13,
    Bic = 14,
    Mvn = 15,
}

impl Dp {
    /// Compare/test forms: no destination, always set the flags.
    pub fn is_compare(self) -> bool {
        matches!(self, Dp::Tst | Dp::Teq | Dp::Cmp | Dp::Cmn)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Shift {
    Lsl = 0,
    Lsr = 1,
    Asr = 2,
    Ror = 3,
}

/// The flexible second operand of a data-processing instruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op2 {
    /// A modified immediate (see [`encode_imm`]).
    Imm(u32),
    Reg(u8),
    /// `rm, <shift> #amount` (amount 1..=31 for lsl/ror, 1..=32 for lsr/asr).
    RegShiftImm(u8, Shift, u8),
    /// `rm, <shift> rs`
    RegShiftReg(u8, Shift, u8),
}

/// The modified-immediate encoding (`imm8` rotated right by an even amount) of `v`.
pub fn encode_imm(v: u32) -> Option<u32> {
    for rot in 0..16u32 {
        let r = v.rotate_left(rot * 2);
        if r < 256 {
            return Some(rot << 8 | r);
        }
    }
    None
}

/// Load/store forms.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LdSt {
    Ldr,
    Ldrb,
    Ldrsb,
    Ldrh,
    Ldrsh,
    Str,
    Strb,
    Strh,
}

impl LdSt {
    pub fn is_load(self) -> bool {
        matches!(
            self,
            LdSt::Ldr | LdSt::Ldrb | LdSt::Ldrsb | LdSt::Ldrh | LdSt::Ldrsh
        )
    }
    /// Whether `off` is encodable as an immediate offset.
    pub fn offset_ok(self, off: i64) -> bool {
        match self {
            LdSt::Ldr | LdSt::Ldrb | LdSt::Str | LdSt::Strb => (-4095..=4095).contains(&off),
            _ => (-255..=255).contains(&off),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ext {
    Sxtb,
    Sxth,
    Uxtb,
    Uxth,
}

/// VFP data-processing operations on two sources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FOp2 {
    Add,
    Sub,
    Mul,
    Div,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FOp1 {
    Mov,
    Neg,
    Abs,
    Sqrt,
}

#[derive(Clone, Copy)]
enum Kind {
    /// `b`/`bl` imm24.
    B24,
}

struct Fixup {
    pos: usize,
    label: Label,
    kind: Kind,
}

pub struct Asm {
    pub buf: Vec<u8>,
    labels: Vec<Option<usize>>,
    fixups: Vec<Fixup>,
    /// ARMv7-A instructions (`movw`, `movt`, `dmb`, `rbit`) are available.
    pub v7: bool,
}

impl Default for Asm {
    fn default() -> Self {
        Self::new()
    }
}

fn r(n: u8) -> u32 {
    debug_assert!(n < 16);
    n as u32
}

/// Split a single-precision register number into (`V` field, extra bit).
fn sreg(n: u8) -> (u32, u32) {
    debug_assert!(n < 32);
    ((n >> 1) as u32, (n & 1) as u32)
}

impl Asm {
    pub fn new() -> Asm {
        Asm {
            buf: Vec::with_capacity(1024),
            labels: Vec::new(),
            fixups: Vec::new(),
            v7: true,
        }
    }

    /// An assembler for ARMv7-A (`v7`) or ARMv6.
    pub fn with_isa(v7: bool) -> Asm {
        Asm { v7, ..Asm::new() }
    }

    /// `dmb ish`, or its ARMv6 form `mov scratch, #0; mcr p15, 0, scratch, c7, c10, 5`.
    pub fn barrier(&mut self, scratch: u8) {
        if self.v7 {
            self.word(0xf57f_f05b);
        } else {
            self.dp(Cond::Al, Dp::Mov, false, scratch, 0, Op2::Imm(0));
            self.word(0xee07_0fba | r(scratch) << 12);
        }
    }

    /// A no-op word: the `nop` hint, or `mov r0, r0` on ARMv6 (the hint is ARMv6K/T2).
    pub fn nop(&mut self) {
        self.word(if self.v7 { 0xe320_f000 } else { 0xe1a0_0000 });
    }

    pub fn new_label(&mut self) -> Label {
        self.labels.push(None);
        self.labels.len() - 1
    }

    pub fn bind(&mut self, l: Label) {
        debug_assert!(self.labels[l].is_none(), "label bound twice");
        self.labels[l] = Some(self.buf.len());
    }

    pub fn pos(&self) -> usize {
        self.buf.len()
    }

    pub fn word(&mut self, w: u32) {
        self.buf.extend_from_slice(&w.to_le_bytes());
    }

    /// Pad with `udf` words to a multiple of `n` bytes (`n` a multiple of 4).
    pub fn align(&mut self, n: usize) {
        while self.buf.len() % n != 0 {
            self.word(0xe7f0_00f0);
        }
    }

    pub fn finish(&mut self) -> Result<(), String> {
        for f in std::mem::take(&mut self.fixups) {
            let target = self.labels[f.label].ok_or("arm32: unbound label")? as i64;
            let at = f.pos;
            let mut w = u32::from_le_bytes(self.buf[at..at + 4].try_into().unwrap());
            // The pc reads as the instruction address + 8.
            let disp = target - (at as i64 + 8);
            match f.kind {
                Kind::B24 => {
                    let d = disp >> 2;
                    if !(-(1 << 23)..(1 << 23)).contains(&d) {
                        return Err("arm32: branch out of range".into());
                    }
                    w |= (d as u32) & 0x00ff_ffff;
                }
            }
            self.buf[at..at + 4].copy_from_slice(&w.to_le_bytes());
        }
        Ok(())
    }

    // ----- branches -----

    pub fn b_cond(&mut self, c: Cond, l: Label) {
        self.fixups.push(Fixup {
            pos: self.buf.len(),
            label: l,
            kind: Kind::B24,
        });
        self.word(c.bits() | 0x0a00_0000);
    }

    pub fn b(&mut self, l: Label) {
        self.b_cond(Cond::Al, l);
    }

    pub fn bx(&mut self, rm: u8) {
        self.word(Cond::Al.bits() | 0x012f_ff10 | r(rm));
    }

    pub fn blx(&mut self, rm: u8) {
        self.word(Cond::Al.bits() | 0x012f_ff30 | r(rm));
    }

    // ----- data processing -----

    fn op2(op2: Op2) -> u32 {
        match op2 {
            Op2::Imm(v) => (1 << 25) | encode_imm(v).expect("arm32: unencodable immediate"),
            Op2::Reg(m) => r(m),
            Op2::RegShiftImm(m, s, n) => {
                debug_assert!(n >= 1 && n <= 32);
                let n = (n & 31) as u32;
                n << 7 | (s as u32) << 5 | r(m)
            }
            Op2::RegShiftReg(m, s, rs) => r(rs) << 8 | (s as u32) << 5 | 1 << 4 | r(m),
        }
    }

    /// `op{s}{cond} rd, rn, op2` (`rd` ignored for compares, `rn` ignored for `mov`/`mvn`).
    pub fn dp(&mut self, c: Cond, op: Dp, s: bool, rd: u8, rn: u8, op2: Op2) {
        let s = s || op.is_compare();
        let (rd, rn) = match op {
            Dp::Mov | Dp::Mvn => (rd, 0),
            _ if op.is_compare() => (0, rn),
            _ => (rd, rn),
        };
        self.word(
            c.bits()
                | (op as u32) << 21
                | (s as u32) << 20
                | r(rn) << 16
                | r(rd) << 12
                | Self::op2(op2),
        );
    }

    pub fn mov(&mut self, c: Cond, rd: u8, rm: u8) {
        self.dp(c, Dp::Mov, false, rd, 0, Op2::Reg(rm));
    }

    /// `movw rd, #imm16`
    pub fn movw(&mut self, c: Cond, rd: u8, imm: u32) {
        debug_assert!(imm < 1 << 16);
        self.word(c.bits() | 0x0300_0000 | (imm >> 12) << 16 | r(rd) << 12 | (imm & 0xfff));
    }

    /// `movt rd, #imm16`
    pub fn movt(&mut self, c: Cond, rd: u8, imm: u32) {
        debug_assert!(imm < 1 << 16);
        self.word(c.bits() | 0x0340_0000 | (imm >> 12) << 16 | r(rd) << 12 | (imm & 0xfff));
    }

    /// `rd = imm` by the shortest sequence: `mov` or `mvn`, then `movw`+`movt` (ARMv7) or up
    /// to four `mov`/`orr` (or `mvn`/`bic`) instructions (ARMv6).
    pub fn mov_imm(&mut self, rd: u8, imm: u32) {
        if encode_imm(imm).is_some() {
            self.dp(Cond::Al, Dp::Mov, false, rd, 0, Op2::Imm(imm));
        } else if encode_imm(!imm).is_some() {
            self.dp(Cond::Al, Dp::Mvn, false, rd, 0, Op2::Imm(!imm));
        } else if self.v7 {
            self.movw(Cond::Al, rd, imm & 0xffff);
            if imm >> 16 != 0 {
                self.movt(Cond::Al, rd, imm >> 16);
            }
        } else {
            let (pos, neg) = (imm_chunks(imm), imm_chunks(!imm));
            let (chunks, first, rest) = if neg.len() < pos.len() {
                (neg, Dp::Mvn, Dp::Bic)
            } else {
                (pos, Dp::Mov, Dp::Orr)
            };
            for (k, &c) in chunks.iter().enumerate() {
                let op = if k == 0 { first } else { rest };
                self.dp(Cond::Al, op, false, rd, rd, Op2::Imm(c));
            }
        }
    }

    /// A fixed-length load of `imm` that [`patch_mov32`] can rewrite: `movw`+`movt` (ARMv7),
    /// or `mov` and three `orr` of one byte each (ARMv6).
    pub fn mov_imm_patchable(&mut self, rd: u8, imm: u32) {
        if self.v7 {
            self.movw(Cond::Al, rd, imm & 0xffff);
            self.movt(Cond::Al, rd, imm >> 16);
        } else {
            // Every byte keeps its rotation, even when zero, so the patch only rewrites imm8.
            let at = self.buf.len();
            self.dp(Cond::Al, Dp::Mov, false, rd, 0, Op2::Imm(0));
            for _ in 1..4 {
                self.dp(Cond::Al, Dp::Orr, false, rd, rd, Op2::Imm(0));
            }
            patch_mov32(&mut self.buf, at, imm);
        }
    }

    pub fn mul(&mut self, rd: u8, rm: u8, rs: u8) {
        self.word(Cond::Al.bits() | r(rd) << 16 | r(rs) << 8 | 0x90 | r(rm));
    }

    /// `rd = rm * rs + ra`
    pub fn mla(&mut self, rd: u8, rm: u8, rs: u8, ra: u8) {
        self.word(
            Cond::Al.bits() | 1 << 21 | r(rd) << 16 | r(ra) << 12 | r(rs) << 8 | 0x90 | r(rm),
        );
    }

    /// `umull`/`smull`: `{hi, lo} = rm * rs`
    pub fn long_mul(&mut self, signed: bool, lo: u8, hi: u8, rm: u8, rs: u8) {
        let op = if signed { 0x00c0_0000 } else { 0x0080_0000 };
        self.word(Cond::Al.bits() | op | r(hi) << 16 | r(lo) << 12 | r(rs) << 8 | 0x90 | r(rm));
    }

    /// `sdiv`/`udiv rd, rn, rm` (ARMv7 with the integer-divide extension).
    pub fn div(&mut self, signed: bool, rd: u8, rn: u8, rm: u8) {
        let op = if signed { 0x0710_f010 } else { 0x0730_f010 };
        self.word(Cond::Al.bits() | op | r(rd) << 16 | r(rm) << 8 | r(rn));
    }

    pub fn clz(&mut self, rd: u8, rm: u8) {
        self.word(Cond::Al.bits() | 0x016f_0f10 | r(rd) << 12 | r(rm));
    }

    pub fn rbit(&mut self, rd: u8, rm: u8) {
        self.word(Cond::Al.bits() | 0x06ff_0f30 | r(rd) << 12 | r(rm));
    }

    pub fn ext(&mut self, e: Ext, rd: u8, rm: u8) {
        let op = match e {
            Ext::Sxtb => 0x06af_0070,
            Ext::Sxth => 0x06bf_0070,
            Ext::Uxtb => 0x06ef_0070,
            Ext::Uxth => 0x06ff_0070,
        };
        self.word(Cond::Al.bits() | op | r(rd) << 12 | r(rm));
    }

    // ----- loads and stores -----

    /// `ldr`/`str`-family with an immediate offset from `rn`.
    pub fn ldst(&mut self, op: LdSt, rt: u8, rn: u8, off: i64) {
        debug_assert!(
            op.offset_ok(off),
            "arm32: offset {off} out of range for {op:?}"
        );
        let u = off >= 0;
        let a = off.unsigned_abs() as u32;
        let base = Cond::Al.bits() | 1 << 24 | (u as u32) << 23 | r(rn) << 16 | r(rt) << 12;
        match op {
            LdSt::Ldr | LdSt::Ldrb | LdSt::Str | LdSt::Strb => {
                let b = matches!(op, LdSt::Ldrb | LdSt::Strb) as u32;
                self.word(base | 0x0400_0000 | b << 22 | (op.is_load() as u32) << 20 | a);
            }
            _ => {
                let (l, sh) = match op {
                    LdSt::Strh => (0, 0b01),
                    LdSt::Ldrh => (1, 0b01),
                    LdSt::Ldrsb => (1, 0b10),
                    _ => (1, 0b11),
                };
                self.word(
                    base | 1 << 22 | l << 20 | (a >> 4) << 8 | 1 << 7 | sh << 5 | 1 << 4 | (a & 15),
                );
            }
        }
    }

    /// `ldr`/`str`-family with a register offset `[rn, rm]` (added).
    pub fn ldst_reg(&mut self, op: LdSt, rt: u8, rn: u8, rm: u8) {
        let base = Cond::Al.bits() | 1 << 24 | 1 << 23 | r(rn) << 16 | r(rt) << 12;
        match op {
            LdSt::Ldr | LdSt::Ldrb | LdSt::Str | LdSt::Strb => {
                let b = matches!(op, LdSt::Ldrb | LdSt::Strb) as u32;
                self.word(base | 0x0600_0000 | b << 22 | (op.is_load() as u32) << 20 | r(rm));
            }
            _ => {
                let (l, sh) = match op {
                    LdSt::Strh => (0, 0b01),
                    LdSt::Ldrh => (1, 0b01),
                    LdSt::Ldrsb => (1, 0b10),
                    _ => (1, 0b11),
                };
                self.word(base | l << 20 | 1 << 7 | sh << 5 | 1 << 4 | r(rm));
            }
        }
    }

    /// `push {regs}` (bit mask of registers).
    pub fn push(&mut self, regs: u16) {
        self.word(Cond::Al.bits() | 0x092d_0000 | regs as u32);
    }

    /// `pop {regs}`
    pub fn pop(&mut self, regs: u16) {
        self.word(Cond::Al.bits() | 0x08bd_0000 | regs as u32);
    }

    // ----- VFP -----

    /// `vmov sd, rt` (`to_core` false) or `vmov rt, sn` (`to_core` true).
    pub fn vmov_sr(&mut self, c: Cond, to_core: bool, s: u8, rt: u8) {
        let (v, n) = sreg(s);
        self.word(c.bits() | 0x0e00_0a10 | (to_core as u32) << 20 | v << 16 | r(rt) << 12 | n << 7);
    }

    /// `vmov dm, rt, rt2` (`to_core` false) or `vmov rt, rt2, dm` (`to_core` true).
    pub fn vmov_dr(&mut self, c: Cond, to_core: bool, d: u8, rt: u8, rt2: u8) {
        debug_assert!(d < 16);
        self.word(
            c.bits() | 0x0c40_0b10 | (to_core as u32) << 20 | r(rt2) << 16 | r(rt) << 12 | d as u32,
        );
    }

    fn vd(double: bool, d: u8, n: u8, m: u8) -> u32 {
        if double {
            debug_assert!(d < 16 && n < 16 && m < 16);
            0x0b00 | (n as u32) << 16 | (d as u32) << 12 | m as u32
        } else {
            let (vd, dd) = sreg(d);
            let (vn, nn) = sreg(n);
            let (vm, mm) = sreg(m);
            0x0a00 | vn << 16 | vd << 12 | dd << 22 | nn << 7 | mm << 5 | vm
        }
    }

    /// Register arguments are D registers, or S register numbers when `double` is false.
    pub fn fop2(&mut self, c: Cond, op: FOp2, double: bool, d: u8, n: u8, m: u8) {
        let o = match op {
            FOp2::Add => 0x0e30_0000,
            FOp2::Sub => 0x0e30_0040,
            FOp2::Mul => 0x0e20_0000,
            FOp2::Div => 0x0e80_0000,
        };
        self.word(c.bits() | o | Self::vd(double, d, n, m));
    }

    pub fn fop1(&mut self, c: Cond, op: FOp1, double: bool, d: u8, m: u8) {
        let o = match op {
            FOp1::Mov => 0x0eb0_0040,
            FOp1::Neg => 0x0eb1_0040,
            FOp1::Abs => 0x0eb0_00c0,
            FOp1::Sqrt => 0x0eb1_00c0,
        };
        self.word(c.bits() | o | Self::vd(double, d, 0, m));
    }

    pub fn vcmp(&mut self, double: bool, d: u8, m: u8) {
        self.word(Cond::Al.bits() | 0x0eb4_0040 | Self::vd(double, d, 0, m));
    }

    /// `vmrs APSR_nzcv, fpscr`
    pub fn vmrs_flags(&mut self) {
        self.word(Cond::Al.bits() | 0x0ef1_fa10);
    }

    /// `vcvt.f64/f32.s32/u32 d, s`: `d` is a D register (S number when single), `s` an S number.
    pub fn vcvt_from_int(&mut self, signed: bool, double: bool, d: u8, s: u8) {
        let (vm, m) = sreg(s);
        let (dreg, dbit) = if double {
            (d as u32, 0)
        } else {
            (d as u32 >> 1, d as u32 & 1)
        };
        let o = 0x0eb8_0a40 | (signed as u32) << 7 | (double as u32) << 8;
        self.word(Cond::Al.bits() | o | dbit << 22 | dreg << 12 | m << 5 | vm);
    }

    /// `vcvt.s32/u32.f64/f32 s, m` (truncating, saturating): `s` an S number, `m` a D register
    /// (S number when single).
    pub fn vcvt_to_int(&mut self, signed: bool, double: bool, s: u8, m: u8) {
        let (vd, d) = sreg(s);
        let (mreg, mbit) = if double {
            (m as u32, 0)
        } else {
            (m as u32 >> 1, m as u32 & 1)
        };
        let o = 0x0ebc_0ac0 | (signed as u32) << 16 | (double as u32) << 8;
        self.word(Cond::Al.bits() | o | d << 22 | vd << 12 | mbit << 5 | mreg);
    }

    /// `vcvt.f64.f32 dd, sm` (`to_double`; `d` is a D register, `m` an S register number) or
    /// `vcvt.f32.f64 sd, dm` (`d` is an S register number, `m` a D register).
    pub fn vcvt_f(&mut self, to_double: bool, d: u8, m: u8) {
        if to_double {
            let (vm, mm) = sreg(m);
            self.word(Cond::Al.bits() | 0x0eb7_0ac0 | (d as u32) << 12 | mm << 5 | vm);
        } else {
            let (vd, dd) = sreg(d);
            self.word(Cond::Al.bits() | 0x0eb7_0bc0 | dd << 22 | vd << 12 | m as u32);
        }
    }

    /// `vldr`/`vstr` with a word-aligned offset in ±1020. `reg` is a D register, or an S
    /// register number when `double` is false.
    pub fn vldst(&mut self, load: bool, double: bool, reg: u8, rn: u8, off: i64) {
        debug_assert!(off % 4 == 0 && (-1020..=1020).contains(&off));
        let u = off >= 0;
        let imm8 = (off.unsigned_abs() / 4) as u32;
        let (v, d) = if double { (reg as u32, 0) } else { sreg(reg) };
        let o = 0x0d00_0a00 | (double as u32) << 8 | (load as u32) << 20 | (u as u32) << 23;
        self.word(Cond::Al.bits() | o | d << 22 | r(rn) << 16 | v << 12 | imm8);
    }

    /// `vpush {d_first .. d_first+count-1}`
    pub fn vpush(&mut self, first: u8, count: u8) {
        self.word(Cond::Al.bits() | 0x0d2d_0b00 | (first as u32) << 12 | 2 * count as u32);
    }

    /// `vpop {d_first .. d_first+count-1}`
    pub fn vpop(&mut self, first: u8, count: u8) {
        self.word(Cond::Al.bits() | 0x0cbd_0b00 | (first as u32) << 12 | 2 * count as u32);
    }
}

/// `v` as the fewest modified immediates whose bitwise or is `v` (each an `imm8` at an even
/// bit position; at most four).
fn imm_chunks(mut v: u32) -> Vec<u32> {
    let mut out = Vec::with_capacity(4);
    while v != 0 {
        let low = v.trailing_zeros() & !1;
        let c = v & (0xff << low);
        out.push(c);
        v &= !c;
    }
    out
}

/// Patch the load at `at` emitted by [`Asm::mov_imm_patchable`] (either form) to load `imm`.
pub fn patch_mov32(buf: &mut [u8], at: usize, imm: u32) {
    let word =
        |buf: &[u8], k: usize| u32::from_le_bytes(buf[at + k..at + k + 4].try_into().unwrap());
    if word(buf, 0) & 0x0ff0_0000 == 0x0300_0000 {
        // movw, movt: imm4 in bits 19:16, imm12 in bits 11:0.
        for (k, half) in [(0, imm & 0xffff), (4, imm >> 16)] {
            let mut w = word(buf, k);
            w &= !(0xf << 16 | 0xfff);
            w |= (half >> 12) << 16 | (half & 0xfff);
            buf[at + k..at + k + 4].copy_from_slice(&w.to_le_bytes());
        }
    } else {
        // mov #b0; orr #b1, ror 24; orr #b2, ror 16; orr #b3, ror 8.
        for k in 0..4 {
            let byte = imm >> (8 * k) & 0xff;
            let rot = (16 - 4 * k as u32) % 16;
            let mut w = word(buf, 4 * k);
            w = (w & !0xfff) | rot << 8 | byte;
            buf[at + 4 * k..at + 4 * k + 4].copy_from_slice(&w.to_le_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(f: impl FnOnce(&mut Asm)) -> u32 {
        let mut a = Asm::new();
        f(&mut a);
        u32::from_le_bytes(a.buf[..4].try_into().unwrap())
    }

    #[test]
    fn modified_immediates() {
        assert_eq!(encode_imm(0xff), Some(0xff));
        assert_eq!(encode_imm(0xff00), Some(0xcff));
        assert_eq!(encode_imm(0x8000_0000), Some(0x102));
        assert_eq!(encode_imm(0x101), None);
        assert_eq!(encode_imm(0xf000_000f), Some(0x2ff));
    }

    #[test]
    fn known_encodings() {
        // Checked against GNU as / objdump.
        assert_eq!(
            one(|a| a.dp(Cond::Al, Dp::Add, false, 1, 2, Op2::Reg(3))),
            0xe0821003
        );
        assert_eq!(
            one(|a| a.dp(Cond::Al, Dp::Add, true, 1, 2, Op2::Imm(4))),
            0xe2921004
        );
        assert_eq!(
            one(|a| a.dp(Cond::Al, Dp::Sub, false, 0, 13, Op2::Imm(16))),
            0xe24d0010
        );
        assert_eq!(
            one(|a| a.dp(Cond::Al, Dp::Cmp, true, 0, 1, Op2::Reg(2))),
            0xe1510002
        );
        assert_eq!(
            one(|a| a.dp(
                Cond::Al,
                Dp::Mov,
                false,
                1,
                0,
                Op2::RegShiftImm(2, Shift::Lsl, 3)
            )),
            0xe1a01182
        );
        assert_eq!(
            one(|a| a.dp(
                Cond::Al,
                Dp::Mov,
                false,
                1,
                0,
                Op2::RegShiftReg(2, Shift::Asr, 3)
            )),
            0xe1a01352
        );
        assert_eq!(one(|a| a.movw(Cond::Al, 4, 0x1234)), 0xe3014234);
        assert_eq!(one(|a| a.movt(Cond::Al, 4, 0xabcd)), 0xe34a4bcd);
        assert_eq!(one(|a| a.mul(1, 2, 3)), 0xe0010392);
        assert_eq!(one(|a| a.mla(1, 2, 3, 4)), 0xe0214392);
        assert_eq!(one(|a| a.long_mul(false, 1, 2, 3, 4)), 0xe0821493);
        assert_eq!(one(|a| a.long_mul(true, 1, 2, 3, 4)), 0xe0c21493);
        assert_eq!(one(|a| a.clz(1, 2)), 0xe16f1f12);
        assert_eq!(one(|a| a.div(true, 1, 2, 3)), 0xe711f312);
        assert_eq!(one(|a| a.div(false, 1, 2, 3)), 0xe731f312);
        assert_eq!(one(|a| a.rbit(1, 2)), 0xe6ff1f32);
        assert_eq!(one(|a| a.ext(Ext::Sxtb, 1, 2)), 0xe6af1072);
        assert_eq!(one(|a| a.ext(Ext::Uxth, 1, 2)), 0xe6ff1072);
        assert_eq!(one(|a| a.ldst(LdSt::Ldr, 1, 2, 8)), 0xe5921008);
        assert_eq!(one(|a| a.ldst(LdSt::Str, 1, 2, -8)), 0xe5021008);
        assert_eq!(one(|a| a.ldst(LdSt::Ldrb, 1, 2, 3)), 0xe5d21003);
        assert_eq!(one(|a| a.ldst(LdSt::Ldrh, 1, 2, 6)), 0xe1d210b6);
        assert_eq!(one(|a| a.ldst(LdSt::Ldrsb, 1, 2, 6)), 0xe1d210d6);
        assert_eq!(one(|a| a.ldst(LdSt::Ldrsh, 1, 2, 6)), 0xe1d210f6);
        assert_eq!(one(|a| a.ldst(LdSt::Strh, 1, 2, 6)), 0xe1c210b6);
        assert_eq!(one(|a| a.ldst_reg(LdSt::Ldr, 1, 2, 3)), 0xe7921003);
        assert_eq!(one(|a| a.ldst_reg(LdSt::Ldrsh, 1, 2, 3)), 0xe19210f3);
        assert_eq!(one(|a| a.push(0x4ff0)), 0xe92d4ff0);
        assert_eq!(one(|a| a.pop(0x8ff0)), 0xe8bd8ff0);
        assert_eq!(one(|a| a.blx(12)), 0xe12fff3c);
        assert_eq!(one(|a| a.bx(14)), 0xe12fff1e);
    }

    #[test]
    fn vfp_encodings() {
        assert_eq!(
            one(|a| a.fop2(Cond::Al, FOp2::Add, true, 1, 2, 3)),
            0xee321b03
        );
        assert_eq!(
            one(|a| a.fop2(Cond::Al, FOp2::Sub, true, 1, 2, 3)),
            0xee321b43
        );
        assert_eq!(
            one(|a| a.fop2(Cond::Al, FOp2::Mul, true, 1, 2, 3)),
            0xee221b03
        );
        assert_eq!(
            one(|a| a.fop2(Cond::Al, FOp2::Div, true, 1, 2, 3)),
            0xee821b03
        );
        assert_eq!(
            one(|a| a.fop2(Cond::Al, FOp2::Add, false, 2, 4, 6)),
            0xee321a03
        );
        assert_eq!(one(|a| a.fop1(Cond::Al, FOp1::Neg, true, 1, 2)), 0xeeb11b42);
        assert_eq!(one(|a| a.fop1(Cond::Al, FOp1::Abs, true, 1, 2)), 0xeeb01bc2);
        assert_eq!(
            one(|a| a.fop1(Cond::Al, FOp1::Sqrt, true, 1, 2)),
            0xeeb11bc2
        );
        assert_eq!(one(|a| a.fop1(Cond::Al, FOp1::Mov, true, 1, 2)), 0xeeb01b42);
        assert_eq!(one(|a| a.vcmp(true, 1, 2)), 0xeeb41b42);
        assert_eq!(one(|a| a.vmrs_flags()), 0xeef1fa10);
        assert_eq!(one(|a| a.vmov_sr(Cond::Al, false, 2, 3)), 0xee013a10);
        assert_eq!(one(|a| a.vmov_sr(Cond::Al, true, 3, 4)), 0xee114a90);
        assert_eq!(one(|a| a.vmov_dr(Cond::Al, false, 2, 3, 4)), 0xec443b12);
        assert_eq!(one(|a| a.vmov_dr(Cond::Al, true, 2, 3, 4)), 0xec543b12);
        assert_eq!(one(|a| a.vldst(true, true, 2, 3, 8)), 0xed932b02);
        assert_eq!(one(|a| a.vldst(false, true, 2, 3, -8)), 0xed032b02);
        assert_eq!(one(|a| a.vldst(true, false, 2, 3, 4)), 0xed931a01);
        assert_eq!(one(|a| a.vcvt_from_int(true, true, 1, 4)), 0xeeb81bc2);
        assert_eq!(one(|a| a.vcvt_to_int(true, true, 2, 1)), 0xeebd1bc1);
        assert_eq!(one(|a| a.vcvt_f(true, 1, 4)), 0xeeb71ac2);
        assert_eq!(one(|a| a.vcvt_f(false, 2, 1)), 0xeeb71bc1);
        assert_eq!(one(|a| a.vcvt_from_int(false, false, 2, 4)), 0xeeb81a42);
        assert_eq!(one(|a| a.vcvt_to_int(false, false, 2, 4)), 0xeebc1ac2);
        assert_eq!(
            one(|a| a.dp(
                Cond::Al,
                Dp::Add,
                false,
                PC,
                PC,
                Op2::RegShiftImm(1, Shift::Lsl, 2)
            )),
            0xe08ff101
        );
        assert_eq!(one(|a| a.vpush(8, 8)), 0xed2d8b10);
        assert_eq!(one(|a| a.vpop(0, 8)), 0xecbd0b10);
    }
}
