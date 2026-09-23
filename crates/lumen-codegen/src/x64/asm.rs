//! x86-64 instruction encoding: REX/VEX prefixes, ModRM/SIB addressing, labels and fixups.
//!
//! Jumps are sized in two passes (docs/jit-notes/lowering-emit.md): the first pass emits every
//! jump long and records which displacements fit in 8 bits; the second emits those short. Since
//! shortening only ever moves code closer together, every jump that fit in pass 1 still fits.

pub type Label = usize;

/// A ModRM `r/m` operand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RM {
    Reg(u8),
    /// `[base + index << scale + disp]`, scale as log2.
    Mem {
        base: u8,
        index: Option<(u8, u8)>,
        disp: i32,
    },
    /// `[rip + label]`
    Rip(Label),
}

impl RM {
    pub fn mem(base: u8, disp: i32) -> RM {
        RM::Mem {
            base,
            index: None,
            disp,
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Rel32,
    Rel8,
    /// A jump-table entry: `label - base` as i32.
    Table(Label),
}

struct Fixup {
    pos: usize,
    /// The address displacements are relative to (end of the instruction).
    end: usize,
    label: Label,
    kind: Kind,
    /// Jump sequence number, for shortening.
    jump: Option<usize>,
}

pub struct Asm {
    pub buf: Vec<u8>,
    labels: Vec<Option<usize>>,
    fixups: Vec<Fixup>,
    short: Vec<bool>,
    jumps: usize,
}

pub const RAX: u8 = 0;
pub const RCX: u8 = 1;
pub const RDX: u8 = 2;
pub const RSP: u8 = 4;
pub const RBP: u8 = 5;
pub const R10: u8 = 10;
pub const R11: u8 = 11;

impl Asm {
    /// `short[i]`: emit the `i`th jump in its 8-bit form.
    pub fn new(short: Vec<bool>) -> Asm {
        Asm {
            buf: Vec::with_capacity(1024),
            labels: Vec::new(),
            fixups: Vec::new(),
            short,
            jumps: 0,
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

    pub fn byte(&mut self, b: u8) {
        self.buf.push(b);
    }

    pub fn bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn align(&mut self, n: usize, fill: u8) {
        while self.buf.len() % n != 0 {
            self.buf.push(fill);
        }
    }

    /// A jump-table entry holding `target - base`.
    pub fn table_entry(&mut self, target: Label, base: Label) {
        self.fixups.push(Fixup {
            pos: self.buf.len(),
            end: 0,
            label: target,
            kind: Kind::Table(base),
            jump: None,
        });
        self.u32(0);
    }

    /// Emit `[pfx] [REX] opc modrm [sib] [disp]`; `imm_len` immediate bytes must follow (they
    /// shift the end of the instruction a RIP-relative operand is measured from). `byte` marks
    /// 8-bit register operands, which need a REX prefix to name SPL/BPL/SIL/DIL.
    pub fn op(&mut self, pfx: u8, w: bool, opc: &[u8], reg: u8, rm: RM, byte: bool, imm_len: usize) {
        if pfx != 0 {
            self.buf.push(pfx);
        }
        let (b, x) = match rm {
            RM::Reg(r) => (r >> 3, 0),
            RM::Mem { base, index, .. } => (base >> 3, index.map_or(0, |i| i.0 >> 3)),
            RM::Rip(_) => (0, 0),
        };
        let r = (reg >> 3) & 1;
        let byte_rex = byte && ((4..8).contains(&reg) || matches!(rm, RM::Reg(4..=7)));
        if w || r != 0 || x != 0 || b != 0 || byte_rex {
            self.buf
                .push(0x40 | (w as u8) << 3 | r << 2 | (x & 1) << 1 | (b & 1));
        }
        self.buf.extend_from_slice(opc);
        self.modrm(reg, rm, imm_len);
    }

    fn modrm(&mut self, reg: u8, rm: RM, imm_len: usize) {
        let reg = (reg & 7) << 3;
        match rm {
            RM::Reg(r) => self.buf.push(0xc0 | reg | (r & 7)),
            RM::Mem { base, index, disp } => {
                let b7 = base & 7;
                let md = if disp == 0 && b7 != 5 {
                    0
                } else if i8::try_from(disp).is_ok() {
                    1
                } else {
                    2
                };
                if index.is_some() || b7 == 4 {
                    self.buf.push(md << 6 | reg | 4);
                    let (idx, sc) = index.unwrap_or((4, 0));
                    self.buf.push(sc << 6 | (idx & 7) << 3 | b7);
                } else {
                    self.buf.push(md << 6 | reg | b7);
                }
                match md {
                    1 => self.buf.push(disp as i8 as u8),
                    2 => self.u32(disp as u32),
                    _ => {}
                }
            }
            RM::Rip(l) => {
                self.buf.push(reg | 5);
                let pos = self.buf.len();
                self.fixups.push(Fixup {
                    pos,
                    end: pos + 4 + imm_len,
                    label: l,
                    kind: Kind::Rel32,
                    jump: None,
                });
                self.u32(0);
            }
        }
    }

    /// A VEX-encoded `0F38` instruction (BMI2): `reg`, `vvvv`, `r/m`.
    pub fn vex38(&mut self, pp: u8, w: bool, opc: u8, reg: u8, vvvv: u8, rm: RM) {
        let (b, x) = match rm {
            RM::Reg(r) => (r >> 3, 0),
            RM::Mem { base, index, .. } => (base >> 3, index.map_or(0, |i| i.0 >> 3)),
            RM::Rip(_) => (0, 0),
        };
        let r = (reg >> 3) & 1;
        self.buf.push(0xc4);
        self.buf
            .push(((r ^ 1) << 7) | (((x & 1) ^ 1) << 6) | (((b & 1) ^ 1) << 5) | 0b00010);
        self.buf.push((w as u8) << 7 | ((!vvvv) & 0xf) << 3 | pp);
        self.buf.push(opc);
        self.modrm(reg, rm, 0);
    }

    fn next_short(&mut self) -> (bool, usize) {
        let i = self.jumps;
        self.jumps += 1;
        (self.short.get(i).copied().unwrap_or(false), i)
    }

    fn rel(&mut self, l: Label, short: bool, jump: usize) {
        let pos = self.buf.len();
        let (kind, n) = if short { (Kind::Rel8, 1) } else { (Kind::Rel32, 4) };
        self.fixups.push(Fixup {
            pos,
            end: pos + n,
            label: l,
            kind,
            jump: Some(jump),
        });
        self.buf.extend(std::iter::repeat_n(0, n));
    }

    pub fn jmp(&mut self, l: Label) {
        let (short, i) = self.next_short();
        self.buf.push(if short { 0xeb } else { 0xe9 });
        self.rel(l, short, i);
    }

    /// `jcc` with a hardware condition code.
    pub fn jcc(&mut self, cc: u8, l: Label) {
        let (short, i) = self.next_short();
        if short {
            self.buf.push(0x70 + cc);
        } else {
            self.buf.extend_from_slice(&[0x0f, 0x80 + cc]);
        }
        self.rel(l, short, i);
    }

    /// Fixed-size `call rel32` or `lea`-style displacement to a label.
    pub fn rel32(&mut self, l: Label) {
        let pos = self.buf.len();
        self.fixups.push(Fixup {
            pos,
            end: pos + 4,
            label: l,
            kind: Kind::Rel32,
            jump: None,
        });
        self.u32(0);
    }

    pub fn push(&mut self, r: u8) {
        if r >= 8 {
            self.buf.push(0x41);
        }
        self.buf.push(0x50 + (r & 7));
    }

    pub fn pop(&mut self, r: u8) {
        if r >= 8 {
            self.buf.push(0x41);
        }
        self.buf.push(0x58 + (r & 7));
    }

    /// `mov r64, r64`
    pub fn mov_rr(&mut self, dst: u8, src: u8) {
        self.op(0, true, &[0x89], src, RM::Reg(dst), false, 0);
    }

    /// `mov r, r/m` (64-bit when `w`).
    pub fn mov_load(&mut self, w: bool, dst: u8, rm: RM) {
        self.op(0, w, &[0x8b], dst, rm, false, 0);
    }

    /// `mov r/m, r`
    pub fn mov_store(&mut self, w: bool, src: u8, rm: RM) {
        self.op(0, w, &[0x89], src, rm, false, 0);
    }

    /// Load a 64-bit immediate into a register with the shortest encoding. `xor` clears flags,
    /// so `flags_live` forces a `mov` for zero.
    pub fn mov_imm(&mut self, dst: u8, imm: u64, flags_live: bool) {
        if imm == 0 && !flags_live {
            self.op(0, false, &[0x31], dst, RM::Reg(dst), false, 0);
        } else if imm <= u32::MAX as u64 {
            if dst >= 8 {
                self.buf.push(0x41);
            }
            self.buf.push(0xb8 + (dst & 7));
            self.u32(imm as u32);
        } else if let Ok(i) = i32::try_from(imm as i64) {
            self.op(0, true, &[0xc7], 0, RM::Reg(dst), false, 4);
            self.u32(i as u32);
        } else {
            self.buf.push(0x48 | (dst >> 3));
            self.buf.push(0xb8 + (dst & 7));
            self.u64(imm);
        }
    }

    /// Group-1 ALU with an immediate (`/digit`), choosing the 8-bit form when it fits.
    pub fn alu_imm(&mut self, w: bool, digit: u8, rm: RM, imm: i32) {
        if let Ok(b) = i8::try_from(imm) {
            self.op(0, w, &[0x83], digit, rm, false, 1);
            self.buf.push(b as u8);
        } else {
            self.op(0, w, &[0x81], digit, rm, false, 4);
            self.u32(imm as u32);
        }
    }

    pub fn ret(&mut self) {
        self.buf.push(0xc3);
    }

    /// Resolve every fixup. Returns, per jump, whether its displacement fits in 8 bits.
    pub fn finish(&mut self) -> Result<Vec<bool>, String> {
        let mut fits = vec![false; self.jumps];
        for f in &self.fixups {
            let target = self.labels[f.label].ok_or("x64: unbound label")? as i64;
            match f.kind {
                Kind::Rel32 => {
                    let d = target - f.end as i64;
                    let d = i32::try_from(d).map_err(|_| "x64: displacement out of range")?;
                    self.buf[f.pos..f.pos + 4].copy_from_slice(&d.to_le_bytes());
                    if let Some(j) = f.jump {
                        fits[j] = i8::try_from(d).is_ok();
                    }
                }
                Kind::Rel8 => {
                    let d = i8::try_from(target - f.end as i64)
                        .map_err(|_| "x64: short jump out of range")?;
                    self.buf[f.pos] = d as u8;
                    if let Some(j) = f.jump {
                        fits[j] = true;
                    }
                }
                Kind::Table(base) => {
                    let b = self.labels[base].ok_or("x64: unbound label")? as i64;
                    let d = (target - b) as i32;
                    self.buf[f.pos..f.pos + 4].copy_from_slice(&d.to_le_bytes());
                }
            }
        }
        Ok(fits)
    }
}
