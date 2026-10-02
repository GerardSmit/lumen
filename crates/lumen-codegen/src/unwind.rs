//! Unwind instructions recorded at the actual emitted instruction boundaries.

#[derive(Default)]
pub struct Cfi {
    pub bytes: Vec<u8>,
    pc: usize,
    windows: Vec<Vec<u8>>,
}

fn uleb(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 127) as u8;
        value >>= 7;
        out.push(byte | if value != 0 { 128 } else { 0 });
        if value == 0 {
            break;
        }
    }
}

impl Cfi {
    pub fn at(&mut self, pc: usize) {
        if pc != self.pc {
            self.bytes.push(4); // DW_CFA_advance_loc4
            self.bytes
                .extend_from_slice(&((pc - self.pc) as u32).to_le_bytes());
            self.pc = pc;
        }
    }
    pub fn cfa(&mut self, reg: u8, offset: u32) {
        self.bytes.push(12);
        uleb(&mut self.bytes, reg as u64);
        uleb(&mut self.bytes, offset as u64);
    }
    pub fn save(&mut self, reg: u8, below_cfa: u32) {
        self.bytes.push(5);
        uleb(&mut self.bytes, reg as u64);
        uleb(&mut self.bytes, (below_cfa / 8) as u64);
    }
    pub fn restore(&mut self, reg: u8) {
        self.bytes.extend_from_slice(&[6, reg]);
    }
    pub fn remember(&mut self) {
        self.bytes.push(10);
    }
    pub fn reset(&mut self) {
        self.bytes.push(11);
    }
    pub fn win(&mut self, pc: usize, op: u8, extra: &[u8]) {
        let mut code = vec![pc.min(255) as u8, op];
        code.extend_from_slice(extra);
        self.windows.push(code);
    }
    pub fn windows(&self, prologue: usize) -> Result<Vec<u8>, String> {
        let count: usize = self.windows.iter().map(|code| code.len() / 2).sum();
        if prologue > 255 || count > 255 {
            return Err("x64: prologue exceeds Windows unwind encoding".into());
        }
        // No frame-register field: the fixed allocation is undone directly.
        let mut out = vec![1, prologue as u8, count as u8, 0];
        for code in self.windows.iter().rev() {
            out.extend_from_slice(code);
        }
        while out.len() % 4 != 0 {
            out.push(0);
        }
        Ok(out)
    }
}

pub fn x64_register(reg: u8) -> u8 {
    [0, 2, 1, 3, 7, 6, 4, 5, 8, 9, 10, 11, 12, 13, 14, 15][reg as usize]
}

/// Full Windows ARM64 xdata with instruction-accurate prologue operations and
/// separately indexed epilogues. Uses the Windows 10 unwind instruction set.
#[derive(Default)]
pub struct ArmWindows {
    pub prologue: Vec<Vec<u8>>,
    pub epilogues: Vec<(usize, Vec<u8>)>,
}

pub fn arm_alloc(bytes: u32) -> Vec<u8> {
    let size = bytes / 16;
    if size < 32 {
        vec![size as u8]
    } else if size < 2048 {
        vec![0xc0 | (size >> 8) as u8, size as u8]
    } else {
        vec![0xe0, (size >> 16) as u8, (size >> 8) as u8, size as u8]
    }
}

pub fn arm_save(reg: u8, float: bool, offset: u32) -> Vec<u8> {
    let opcode = if float {
        0xdc00 | ((reg - 8) as u16) << 6
    } else {
        0xd000 | ((reg - 19) as u16) << 6
    };
    (opcode | (offset / 8) as u16).to_be_bytes().to_vec()
}

impl ArmWindows {
    pub fn finish(&self, function_len: usize) -> Result<Vec<u8>, String> {
        if function_len % 4 != 0
            || function_len / 4 >= 1 << 18
            || self.epilogues.len() > u16::MAX as usize
        {
            return Err("ARM64 function exceeds Windows unwind range".into());
        }
        let mut codes = Vec::new();
        for code in self.prologue.iter().rev() {
            codes.extend_from_slice(code);
        }
        codes.push(0xe4);
        let mut scopes = Vec::new();
        let mut sequences = Vec::<(&[u8], usize)>::new();
        for (pc, sequence) in &self.epilogues {
            let index = if let Some((_, index)) = sequences
                .iter()
                .find(|(old, _)| *old == sequence.as_slice())
            {
                *index
            } else {
                let index = codes.len();
                codes.extend_from_slice(sequence);
                sequences.push((sequence, index));
                index
            };
            if index >= 1024 {
                return Err("ARM64 unwind code index overflow".into());
            }
            scopes.push((*pc / 4) as u32 | (index as u32) << 22);
        }
        while codes.len() % 4 != 0 {
            codes.push(0xe3);
        }
        let words = codes.len() / 4;
        if words > 255 {
            return Err("ARM64 unwind code words overflow".into());
        }
        let extended = words > 31 || scopes.len() > 31;
        let mut header = (function_len / 4) as u32;
        if !extended {
            header |= (scopes.len() as u32) << 22 | (words as u32) << 27;
        }
        let mut out = header.to_le_bytes().to_vec();
        if extended {
            out.extend_from_slice(&((scopes.len() as u32) | (words as u32) << 16).to_le_bytes());
        }
        for scope in scopes {
            out.extend_from_slice(&scope.to_le_bytes());
        }
        out.extend_from_slice(&codes);
        Ok(out)
    }
}
