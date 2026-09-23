//! AArch64 (A64) instruction encoding: fixed 32-bit words, labels and PC-relative fixups.
//!
//! Every instruction is one little-endian word, so there is no branch relaxation: branches are
//! emitted with their final form and patched in [`Asm::finish`], which fails (and the function
//! is rejected) when a displacement does not fit — ±128 MiB for `b`, ±1 MiB for `b.cond`,
//! `cbz` and literal loads, which no realistic function reaches.
//!
//! Register number 31 is `sp` or the zero register depending on the instruction; the
//! constants [`SP`] and [`ZR`] are the same number and name the intent at each call site.

pub type Label = usize;

pub const SP: u8 = 31;
pub const ZR: u8 = 31;
pub const FP: u8 = 29;
pub const LR: u8 = 30;
/// Intra-procedure-call scratch registers (IP0/IP1): never allocated.
pub const X16: u8 = 16;
pub const X17: u8 = 17;

/// Condition codes (hardware encoding).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
        ALL[self as usize ^ 1]
    }
}

/// Load/store forms. The value is the unsigned-offset encoding with every operand field zero;
/// [`LdSt::log2`] is the access size used to scale the offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum LdSt {
    StrB = 0x3900_0000,
    LdrB = 0x3940_0000,
    LdrSb64 = 0x3980_0000,
    LdrSb32 = 0x39C0_0000,
    StrH = 0x7900_0000,
    LdrH = 0x7940_0000,
    LdrSh64 = 0x7980_0000,
    LdrSh32 = 0x79C0_0000,
    StrW = 0xB900_0000,
    LdrW = 0xB940_0000,
    LdrSw = 0xB980_0000,
    StrX = 0xF900_0000,
    LdrX = 0xF940_0000,
    StrS = 0xBD00_0000,
    LdrS = 0xBD40_0000,
    StrD = 0xFD00_0000,
    LdrD = 0xFD40_0000,
}

impl LdSt {
    pub fn log2(self) -> u32 {
        (self as u32) >> 30
    }
    /// Whether `off` is encodable directly (scaled unsigned 12-bit, or unscaled signed 9-bit).
    pub fn offset_ok(self, off: i64) -> bool {
        let sz = 1i64 << self.log2();
        (off >= 0 && off % sz == 0 && off / sz < 4096) || (-256..256).contains(&off)
    }
}

/// Integer register-register operations (data-processing, 2 sources, plus arithmetic/logical).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Rrr {
    Add = 0x0B00_0000,
    Sub = 0x4B00_0000,
    Subs = 0x6B00_0000,
    And = 0x0A00_0000,
    Orr = 0x2A00_0000,
    Eor = 0x4A00_0000,
    Mul = 0x1B00_7C00,
    Udiv = 0x1AC0_0800,
    Sdiv = 0x1AC0_0C00,
    Lslv = 0x1AC0_2000,
    Lsrv = 0x1AC0_2400,
    Asrv = 0x1AC0_2800,
    Rorv = 0x1AC0_2C00,
}

/// Logical-immediate operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum LogImm {
    And = 0x1200_0000,
    Orr = 0x3200_0000,
    Eor = 0x5200_0000,
}

/// Two-operand scalar float operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum FOp2 {
    Mul = 0x1E20_0800,
    Div = 0x1E20_1800,
    Add = 0x1E20_2800,
    Sub = 0x1E20_3800,
    Max = 0x1E20_4800,
    Min = 0x1E20_5800,
}

/// One-operand scalar float operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum FOp1 {
    Mov = 0x1E20_4000,
    Abs = 0x1E20_C000,
    Neg = 0x1E21_4000,
    Sqrt = 0x1E21_C000,
    /// Round to nearest, ties to even.
    RintN = 0x1E24_4000,
    /// Round toward +inf.
    RintP = 0x1E24_C000,
    /// Round toward -inf.
    RintM = 0x1E25_4000,
    /// Round toward zero.
    RintZ = 0x1E25_C000,
}

#[derive(Clone, Copy)]
enum Kind {
    /// `b`/`bl`: imm26 at bit 0.
    B26,
    /// `b.cond`/`cbz`/`cbnz`/`ldr` literal: imm19 at bit 5.
    B19,
    /// `adr`: 21-bit byte offset split immlo (29..31) / immhi (5..24).
    Adr,
    /// A jump-table entry: `label - base` as i32.
    Table(Label),
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
}

fn sf(w: bool) -> u32 {
    (w as u32) << 31
}
fn r(n: u8) -> u32 {
    (n & 31) as u32
}
/// `ftype` field: 0 = single, 1 = double.
fn ft(double: bool) -> u32 {
    (double as u32) << 22
}

impl Default for Asm {
    fn default() -> Self {
        Self::new()
    }
}

impl Asm {
    pub fn new() -> Asm {
        Asm {
            buf: Vec::with_capacity(1024),
            labels: Vec::new(),
            fixups: Vec::new(),
        }
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

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Pad with `udf #0` words to a multiple of `n` bytes (`n` a multiple of 4).
    pub fn align(&mut self, n: usize) {
        while self.buf.len() % n != 0 {
            self.word(0);
        }
    }

    fn fixup(&mut self, label: Label, kind: Kind) {
        self.fixups.push(Fixup {
            pos: self.buf.len(),
            label,
            kind,
        });
    }

    /// Resolve every fixup; fails when a label is unbound or a displacement is out of range.
    pub fn finish(&mut self) -> Result<(), String> {
        for f in std::mem::take(&mut self.fixups) {
            let target = self.labels[f.label].ok_or("aarch64: unbound label")? as i64;
            let at = f.pos;
            let mut w = u32::from_le_bytes(self.buf[at..at + 4].try_into().unwrap());
            let disp = target - at as i64;
            match f.kind {
                Kind::B26 => {
                    let d = disp >> 2;
                    if !(-(1 << 25)..(1 << 25)).contains(&d) {
                        return Err("aarch64: branch out of range".into());
                    }
                    w |= (d as u32) & 0x03ff_ffff;
                }
                Kind::B19 => {
                    let d = disp >> 2;
                    if !(-(1 << 18)..(1 << 18)).contains(&d) {
                        return Err("aarch64: conditional branch out of range".into());
                    }
                    w |= ((d as u32) & 0x7_ffff) << 5;
                }
                Kind::Adr => {
                    if !(-(1 << 20)..(1 << 20)).contains(&disp) {
                        return Err("aarch64: adr out of range".into());
                    }
                    let d = disp as u32;
                    w |= (d & 3) << 29 | ((d >> 2) & 0x7_ffff) << 5;
                }
                Kind::Table(base) => {
                    let b = self.labels[base].ok_or("aarch64: unbound label")? as i64;
                    w = (target - b) as i32 as u32;
                }
            }
            self.buf[at..at + 4].copy_from_slice(&w.to_le_bytes());
        }
        Ok(())
    }

    // ----- branches -----

    pub fn b(&mut self, l: Label) {
        self.fixup(l, Kind::B26);
        self.word(0x1400_0000);
    }
    pub fn b_cond(&mut self, c: Cond, l: Label) {
        self.fixup(l, Kind::B19);
        self.word(0x5400_0000 | c as u32);
    }
    /// `cbz`/`cbnz` (`nz`) on a 32- or 64-bit register.
    pub fn cbz(&mut self, nz: bool, w64: bool, rt: u8, l: Label) {
        self.fixup(l, Kind::B19);
        self.word(sf(w64) | 0x3400_0000 | (nz as u32) << 24 | r(rt));
    }
    pub fn br(&mut self, rn: u8) {
        self.word(0xD61F_0000 | r(rn) << 5);
    }
    pub fn blr(&mut self, rn: u8) {
        self.word(0xD63F_0000 | r(rn) << 5);
    }
    pub fn ret(&mut self) {
        self.word(0xD65F_03C0);
    }
    pub fn adr(&mut self, rd: u8, l: Label) {
        self.fixup(l, Kind::Adr);
        self.word(0x1000_0000 | r(rd));
    }
    /// `ldr` (literal) of a 64-bit GPR (`fp: false`) or a d/s register.
    pub fn ldr_lit(&mut self, rt: u8, fp: bool, double: bool, l: Label) {
        self.fixup(l, Kind::B19);
        let op = match (fp, double) {
            (false, _) => 0x5800_0000,
            (true, true) => 0x5C00_0000,
            (true, false) => 0x1C00_0000,
        };
        self.word(op | r(rt));
    }
    /// A jump-table entry: `target - base`.
    pub fn table_entry(&mut self, target: Label, base: Label) {
        self.fixup(target, Kind::Table(base));
        self.word(0);
    }

    // ----- integer -----

    /// `add`/`sub` (immediate): `rd = rn ± (imm12 << (12 if shift))`; rd/rn may be `sp`.
    pub fn add_imm(&mut self, w64: bool, sub: bool, rd: u8, rn: u8, imm12: u32, shift: bool) {
        debug_assert!(imm12 < 4096);
        let op = if sub { 0x5100_0000 } else { 0x1100_0000 };
        self.word(sf(w64) | op | (shift as u32) << 22 | imm12 << 10 | r(rn) << 5 | r(rd));
    }
    /// `cmp rn, #imm` (`subs zr`) or `cmn rn, #imm` (`adds zr`, `neg`).
    pub fn cmp_imm(&mut self, w64: bool, neg: bool, rn: u8, imm12: u32, shift: bool) {
        let op = if neg { 0x3100_0000 } else { 0x7100_0000 };
        self.word(sf(w64) | op | (shift as u32) << 22 | imm12 << 10 | r(rn) << 5 | 31);
    }
    /// `rd = rn op rm` (register; for `add`/`sub`/logical, register 31 is the zero register).
    pub fn rrr(&mut self, op: Rrr, w64: bool, rd: u8, rn: u8, rm: u8) {
        self.word(sf(w64) | op as u32 | r(rm) << 16 | r(rn) << 5 | r(rd));
    }
    /// `add`/`sub` (extended register, UXTX): lets `rd`/`rn` be `sp`.
    pub fn add_ext(&mut self, sub: bool, rd: u8, rn: u8, rm: u8) {
        let op = if sub { 0xCB20_6000 } else { 0x8B20_6000 };
        self.word(op | r(rm) << 16 | r(rn) << 5 | r(rd));
    }
    /// `cmp rn, rm` where `rn` may be `sp` (`subs zr, rn, rm, uxtx`).
    pub fn cmp_ext(&mut self, rn: u8, rm: u8) {
        self.word(0xEB20_6000 | r(rm) << 16 | r(rn) << 5 | 31);
    }
    pub fn mov(&mut self, w64: bool, rd: u8, rm: u8) {
        self.rrr(Rrr::Orr, w64, rd, ZR, rm);
    }
    /// `mov` to or from `sp` (`add rd, rn, #0`).
    pub fn mov_sp(&mut self, rd: u8, rn: u8) {
        self.add_imm(true, false, rd, rn, 0, false);
    }
    /// `rd = ra - rn * rm`
    pub fn msub(&mut self, w64: bool, rd: u8, rn: u8, rm: u8, ra: u8) {
        self.word(sf(w64) | 0x1B00_8000 | r(rm) << 16 | r(ra) << 10 | r(rn) << 5 | r(rd));
    }
    /// Logical immediate with a pre-encoded `N:immr:imms` (see [`logical_imm`]).
    pub fn log_imm(&mut self, op: LogImm, w64: bool, rd: u8, rn: u8, enc: u32) {
        self.word(sf(w64) | op as u32 | enc << 10 | r(rn) << 5 | r(rd));
    }
    /// `sbfm` (`signed`) / `ubfm`.
    pub fn bfm(&mut self, signed: bool, w64: bool, rd: u8, rn: u8, immr: u32, imms: u32) {
        let op = if signed { 0x1300_0000 } else { 0x5300_0000 };
        self.word(sf(w64) | op | (w64 as u32) << 22 | immr << 16 | imms << 10 | r(rn) << 5 | r(rd));
    }
    pub fn lsl_imm(&mut self, w64: bool, rd: u8, rn: u8, s: u32) {
        let bits = if w64 { 64 } else { 32 };
        self.bfm(false, w64, rd, rn, (bits - s) % bits, bits - 1 - s);
    }
    pub fn lsr_imm(&mut self, w64: bool, rd: u8, rn: u8, s: u32) {
        let bits = if w64 { 64 } else { 32 };
        self.bfm(false, w64, rd, rn, s, bits - 1);
    }
    pub fn asr_imm(&mut self, w64: bool, rd: u8, rn: u8, s: u32) {
        let bits = if w64 { 64 } else { 32 };
        self.bfm(true, w64, rd, rn, s, bits - 1);
    }
    /// `ror rd, rn, #s` (`extr rd, rn, rn, #s`).
    pub fn ror_imm(&mut self, w64: bool, rd: u8, rn: u8, s: u32) {
        self.word(
            sf(w64) | 0x1380_0000 | (w64 as u32) << 22 | r(rn) << 16 | s << 10 | r(rn) << 5 | r(rd),
        );
    }
    /// `sxtb`/`sxth`/`sxtw` (sign-extend the low `from` bits).
    pub fn sxt(&mut self, w64: bool, rd: u8, rn: u8, from: u32) {
        self.bfm(true, w64, rd, rn, 0, from - 1);
    }
    pub fn clz(&mut self, w64: bool, rd: u8, rn: u8) {
        self.word(sf(w64) | 0x5AC0_1000 | r(rn) << 5 | r(rd));
    }
    pub fn rbit(&mut self, w64: bool, rd: u8, rn: u8) {
        self.word(sf(w64) | 0x5AC0_0000 | r(rn) << 5 | r(rd));
    }
    /// `csel rd, rn, rm, c` (`rd = c ? rn : rm`).
    pub fn csel(&mut self, w64: bool, rd: u8, rn: u8, rm: u8, c: Cond) {
        self.word(sf(w64) | 0x1A80_0000 | r(rm) << 16 | (c as u32) << 12 | r(rn) << 5 | r(rd));
    }
    /// `cset wd, c` (`csinc wd, wzr, wzr, !c`).
    pub fn cset(&mut self, rd: u8, c: Cond) {
        self.word(0x1A80_0400 | 31 << 16 | (c.invert() as u32) << 12 | 31 << 5 | r(rd));
    }
    /// `movz`/`movn`/`movk` of 16 bits at `hw * 16`.
    pub fn movw(&mut self, kind: MovW, w64: bool, rd: u8, imm16: u32, hw: u32) {
        let op = match kind {
            MovW::N => 0x1280_0000,
            MovW::Z => 0x5280_0000,
            MovW::K => 0x7280_0000,
        };
        self.word(sf(w64) | op | hw << 21 | (imm16 & 0xffff) << 5 | r(rd));
    }

    /// `rd = imm` in the fewest instructions: one `movz`/`movn`/`orr`, else `movz`/`movn` plus
    /// `movk`s. A value below 2^32 uses the 32-bit forms (which zero the upper half).
    pub fn mov_imm(&mut self, rd: u8, imm: u64) {
        let w64 = imm >> 32 != 0;
        let (v, n) = if w64 {
            (imm, 4)
        } else {
            (imm & 0xffff_ffff, 2)
        };
        let chunk = |i: u32| ((v >> (16 * i)) & 0xffff) as u32;
        let zeros = (0..n).filter(|&i| chunk(i) == 0).count();
        let ones = (0..n).filter(|&i| chunk(i) == 0xffff).count();
        if zeros >= n as usize - 1 {
            let i = (0..n).find(|&i| chunk(i) != 0).unwrap_or(0);
            self.movw(MovW::Z, w64, rd, chunk(i), i);
            return;
        }
        if ones >= n as usize - 1 {
            let i = (0..n).find(|&i| chunk(i) != 0xffff).unwrap_or(0);
            self.movw(MovW::N, w64, rd, !chunk(i) & 0xffff, i);
            return;
        }
        if let Some(enc) = logical_imm(v, w64) {
            self.log_imm(LogImm::Orr, w64, rd, ZR, enc);
            return;
        }
        let inverted = ones > zeros;
        let skip = if inverted { 0xffff } else { 0 };
        let mut first = true;
        for i in 0..n {
            let c = chunk(i);
            if c == skip {
                continue;
            }
            if first {
                if inverted {
                    self.movw(MovW::N, w64, rd, !c & 0xffff, i);
                } else {
                    self.movw(MovW::Z, w64, rd, c, i);
                }
                first = false;
            } else {
                self.movw(MovW::K, w64, rd, c, i);
            }
        }
    }

    // ----- memory -----

    /// `op rt, [rn, #off]`, where `off` must satisfy [`LdSt::offset_ok`].
    pub fn ldst(&mut self, op: LdSt, rt: u8, rn: u8, off: i64) {
        let sz = 1i64 << op.log2();
        if off >= 0 && off % sz == 0 && off / sz < 4096 {
            self.word(op as u32 | ((off / sz) as u32) << 10 | r(rn) << 5 | r(rt));
        } else {
            debug_assert!(
                (-256..256).contains(&off),
                "aarch64: offset {off} not encodable"
            );
            // Unscaled (ldur/stur): clear bit 24, imm9 at bit 12.
            self.word(
                (op as u32 & !0x0100_0000) | ((off as u32) & 0x1ff) << 12 | r(rn) << 5 | r(rt),
            );
        }
    }
    /// `op rt, [rn, rm]` (register offset, no shift).
    pub fn ldst_reg(&mut self, op: LdSt, rt: u8, rn: u8, rm: u8, shift: bool) {
        let w = (op as u32 & !0x0100_0000)
            | 1 << 21
            | r(rm) << 16
            | 0b011 << 13
            | (shift as u32) << 12
            | 0b10 << 10;
        self.word(w | r(rn) << 5 | r(rt));
    }
    /// `op rt, [rn], #off` (post-index, signed 9-bit).
    pub fn ldst_post(&mut self, op: LdSt, rt: u8, rn: u8, off: i32) {
        let w = (op as u32 & !0x0100_0000) | ((off as u32) & 0x1ff) << 12 | 0b01 << 10;
        self.word(w | r(rn) << 5 | r(rt));
    }
    /// `op rt, [rn, #off]` for any offset, materializing it in `tmp` when not encodable.
    pub fn ldst_any(&mut self, op: LdSt, rt: u8, rn: u8, off: i64, tmp: u8) {
        if op.offset_ok(off) {
            self.ldst(op, rt, rn, off);
        } else {
            self.mov_imm(tmp, off as u64);
            self.ldst_reg(op, rt, rn, tmp, false);
        }
    }
    /// `stp`/`ldp` of 64-bit GPRs (`fp: false`) or d registers, offset a multiple of 8 in
    /// `-512..512`.
    #[allow(clippy::too_many_arguments)]
    pub fn pair(
        &mut self,
        load: bool,
        fp: bool,
        mode: PairMode,
        rt: u8,
        rt2: u8,
        rn: u8,
        off: i32,
    ) {
        debug_assert!(off % 8 == 0 && (-512..512).contains(&off));
        let base = if fp { 0x6C00_0000 } else { 0xA800_0000 };
        let mode = match mode {
            PairMode::Offset => 0x0100_0000,
            PairMode::Pre => 0x0180_0000,
            PairMode::Post => 0x0080_0000,
        };
        let w = base | mode | (load as u32) << 22;
        self.word(w | (((off / 8) as u32) & 0x7f) << 15 | r(rt2) << 10 | r(rn) << 5 | r(rt));
    }

    // ----- float -----

    pub fn fop2(&mut self, op: FOp2, double: bool, rd: u8, rn: u8, rm: u8) {
        self.word(op as u32 | ft(double) | r(rm) << 16 | r(rn) << 5 | r(rd));
    }
    pub fn fop1(&mut self, op: FOp1, double: bool, rd: u8, rn: u8) {
        self.word(op as u32 | ft(double) | r(rn) << 5 | r(rd));
    }
    pub fn fmov(&mut self, rd: u8, rn: u8) {
        self.fop1(FOp1::Mov, true, rd, rn);
    }
    pub fn fcmp(&mut self, double: bool, rn: u8, rm: u8) {
        self.word(0x1E20_2000 | ft(double) | r(rm) << 16 | r(rn) << 5);
    }
    /// `fcsel rd, rn, rm, c`
    pub fn fcsel(&mut self, double: bool, rd: u8, rn: u8, rm: u8, c: Cond) {
        self.word(0x1E20_0C00 | ft(double) | r(rm) << 16 | (c as u32) << 12 | r(rn) << 5 | r(rd));
    }
    /// `fcvt`: single → double (`to_double`) or double → single.
    pub fn fcvt(&mut self, to_double: bool, rd: u8, rn: u8) {
        let op = if to_double { 0x1E22_C000 } else { 0x1E62_4000 };
        self.word(op | r(rn) << 5 | r(rd));
    }
    /// `fmov rd, #imm8` (see [`fmov_imm8`]).
    pub fn fmov_imm(&mut self, double: bool, rd: u8, imm8: u32) {
        self.word(0x1E20_1000 | ft(double) | imm8 << 13 | r(rd));
    }
    /// `movi dN, #0` (zero the whole register).
    pub fn fzero(&mut self, rd: u8) {
        self.word(0x2F00_E400 | r(rd));
    }
    /// Integer ↔ float conversions and moves (`sf` is the GPR width).
    pub fn fcvt_int(&mut self, op: FInt, w64: bool, double: bool, rd: u8, rn: u8) {
        self.word(sf(w64) | op as u32 | ft(double) | r(rn) << 5 | r(rd));
    }
    /// `cnt vd.8b, vn.8b`
    pub fn cnt8b(&mut self, rd: u8, rn: u8) {
        self.word(0x0E20_5800 | r(rn) << 5 | r(rd));
    }
    /// `addv bd, vn.8b`
    pub fn addv8b(&mut self, rd: u8, rn: u8) {
        self.word(0x0E31_B800 | r(rn) << 5 | r(rd));
    }
}

/// Addressing of [`Asm::pair`]: `[rn, #off]`, `[rn, #off]!` or `[rn], #off`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairMode {
    Offset,
    Pre,
    Post,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MovW {
    N,
    Z,
    K,
}

/// Integer ↔ float conversion opcodes (rmode/opcode fields).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum FInt {
    Scvtf = 0x1E22_0000,
    Ucvtf = 0x1E23_0000,
    Fcvtzs = 0x1E38_0000,
    Fcvtzu = 0x1E39_0000,
    /// `fmov` float → GPR.
    ToGpr = 0x1E26_0000,
    /// `fmov` GPR → float.
    FromGpr = 0x1E27_0000,
}

/// The `N:immr:imms` encoding of `imm` as a logical immediate of the given width, if it is one
/// (a rotated run of ones replicated across 2-, 4-, …, 64-bit elements; not 0 or all-ones).
pub fn logical_imm(imm: u64, w64: bool) -> Option<u32> {
    let v = if w64 {
        imm
    } else {
        let lo = imm & 0xffff_ffff;
        lo | lo << 32
    };
    if v == 0 || v == u64::MAX {
        return None;
    }
    // Smallest element size whose replication gives `v`.
    let mut size = 64u32;
    while size > 2 {
        let half = size / 2;
        let mask = (1u64 << half) - 1;
        if v & mask != (v >> half) & mask {
            break;
        }
        size = half;
    }
    let mask = if size == 64 {
        u64::MAX
    } else {
        (1u64 << size) - 1
    };
    let elt = v & mask;
    let ones = elt.count_ones();
    let run = if ones == 64 {
        u64::MAX
    } else {
        (1u64 << ones) - 1
    };
    let rol = |x: u64, n: u32| -> u64 {
        if n == 0 {
            x
        } else {
            ((x << n) | (x >> (size - n))) & mask
        }
    };
    // `elt` is `run` rotated right by `immr`, i.e. rotating it left by `immr` gives `run`.
    let immr = (0..size).find(|&k| rol(elt, k) == run)?;
    let n = (size == 64) as u32;
    let imms = ((!(size - 1) << 1) | (ones - 1)) & 0x3f;
    Some(n << 12 | immr << 6 | imms)
}

/// The 8-bit `fmov` immediate for `bits` (an f32 when `!double`), if representable:
/// ±(16..31)/16 × 2^(-3..4).
pub fn fmov_imm8(bits: u64, double: bool) -> Option<u32> {
    if double {
        // a:NOT(b):bbbbbbbb:cdefgh:0^48 → sign a, exponent b, low 6 bits cdefgh.
        if bits & 0xffff_ffff_ffff != 0 {
            return None;
        }
        let e = (bits >> 54) & 0x1ff; // bits 62..54
        if e != 0x100 && e != 0x0ff {
            return None;
        }
        let a = (bits >> 63) as u32;
        let b = ((bits >> 54) & 1) as u32;
        let cdefgh = ((bits >> 48) & 0x3f) as u32;
        Some(a << 7 | b << 6 | cdefgh)
    } else {
        let bits = bits as u32;
        if bits & 0x7_ffff != 0 {
            return None;
        }
        let e = (bits >> 25) & 0x3f; // bits 30..25
        if e != 0x20 && e != 0x1f {
            return None;
        }
        let a = bits >> 31;
        let b = (bits >> 25) & 1;
        let cdefgh = (bits >> 19) & 0x3f;
        Some(a << 7 | b << 6 | cdefgh)
    }
}
