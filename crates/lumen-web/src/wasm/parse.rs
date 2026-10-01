//! WebAssembly binary-format decoder (the MVP + a few post-MVP proposals: multi-value results,
//! sign-extension, non-trapping conversions, bulk memory). Produces a [`Module`] the interpreter in
//! `exec.rs` runs. Decoding ends with full validation (`validate.rs`), so every module the
//! interpreter and the native tier see is well-typed.

use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValType {
    I32,
    I64,
    F32,
    F64,
    FuncRef,
    ExternRef,
}

#[derive(Debug, Clone)]
pub struct FuncType {
    pub params: Vec<ValType>,
    pub results: Vec<ValType>,
}

#[derive(Debug, Clone)]
pub enum ImportKind {
    Func(u32), // type index
    Table(TableType),
    Memory(Limits),
    Global(GlobalType),
}

#[derive(Debug, Clone)]
pub struct Import {
    pub module: String,
    pub name: String,
    pub kind: ImportKind,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub min: u32,
    pub max: Option<u32>,
}

#[derive(Debug, Clone, Copy)]
pub struct TableType {
    pub elem: ValType,
    pub limits: Limits,
}

#[derive(Debug, Clone, Copy)]
pub struct GlobalType {
    pub val: ValType,
    pub mutable: bool,
}

#[derive(Debug, Clone)]
pub struct Global {
    pub ty: GlobalType,
    pub init: Vec<u8>, // a constant init expression (raw bytes, ending in `end`)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportKind {
    Func,
    Table,
    Memory,
    Global,
}

#[derive(Debug, Clone)]
pub struct Export {
    pub name: String,
    pub kind: ExportKind,
    pub index: u32,
}

#[derive(Debug, Clone)]
pub struct FuncBody {
    pub locals: Vec<ValType>, // flattened (each declared local, expanded from run-length groups)
    pub code: Vec<u8>,        // raw instruction bytes, up to and excluding the final `end`
}

#[derive(Debug, Clone)]
pub struct DataSegment {
    pub active: Option<(u32, Vec<u8>)>, // (memory index, offset init expr) for active segments
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ElemSegment {
    pub table: u32,
    pub offset: Vec<u8>, // offset init expr
    pub func_indices: Vec<u32>,
}

#[derive(Debug, Default)]
pub struct Module {
    pub types: Vec<FuncType>,
    pub imports: Vec<Import>,
    /// Type index for each *defined* function (imports excluded).
    pub func_types: Vec<u32>,
    pub tables: Vec<TableType>,
    pub memories: Vec<Limits>,
    pub globals: Vec<Global>,
    pub exports: Vec<Export>,
    pub start: Option<u32>,
    pub elems: Vec<ElemSegment>,
    pub code: Vec<FuncBody>,
    pub data: Vec<DataSegment>,
    /// Number of imported functions (defined funcs are indexed after these).
    pub imported_func_count: u32,
    pub imported_table_count: u32,
    pub imported_mem_count: u32,
    pub imported_global_count: u32,
}

// ---- byte reader with LEB128 ------------------------------------------------------------------

pub struct Reader<'a> {
    pub data: &'a [u8],
    pub pos: usize,
}

type R<T> = Result<T, String>;

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }
    pub fn eof(&self) -> bool {
        self.pos >= self.data.len()
    }
    pub fn byte(&mut self) -> R<u8> {
        let b = *self
            .data
            .get(self.pos)
            .ok_or("wasm: unexpected end of input")?;
        self.pos += 1;
        Ok(b)
    }
    pub fn bytes(&mut self, n: usize) -> R<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or("wasm: length overflow")?;
        let s = self
            .data
            .get(self.pos..end)
            .ok_or("wasm: unexpected end of input")?;
        self.pos = end;
        Ok(s)
    }
    pub fn u32(&mut self) -> R<u32> {
        Ok(self.uleb(32)? as u32)
    }
    /// Unsigned LEB128.
    pub fn u64_leb(&mut self) -> R<u64> {
        self.uleb(64)
    }
    /// Signed LEB128.
    pub fn i64_leb(&mut self) -> R<i64> {
        self.sleb(64)
    }
    pub fn i32(&mut self) -> R<i32> {
        Ok(self.sleb(32)? as i32)
    }
    /// A block type's signed 33-bit type index.
    pub fn s33(&mut self) -> R<i64> {
        self.sleb(33)
    }
    /// Unsigned LEB128 of at most `bits` bits: at most ceil(bits / 7) bytes, unused bits zero.
    fn uleb(&mut self, bits: u32) -> R<u64> {
        let mut result = 0u64;
        let mut shift = 0;
        loop {
            let b = self.byte()?;
            let left = bits - shift;
            if left < 7 && (b & 0x80 != 0 || (b & 0x7f) >> left != 0) {
                return Err("wasm: integer representation too long".into());
            }
            result |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
        }
    }
    /// Signed LEB128 of at most `bits` bits; the unused bits of the last byte must sign-extend.
    fn sleb(&mut self, bits: u32) -> R<i64> {
        let mut result = 0i64;
        let mut shift = 0;
        loop {
            let b = self.byte()?;
            let left = bits - shift;
            if left < 7 {
                let mask = ((0x7fu32 << (left - 1)) & 0x7f) as u8;
                if b & 0x80 != 0 || (b & mask != 0 && b & mask != mask) {
                    return Err("wasm: integer representation too long".into());
                }
            }
            result |= ((b & 0x7f) as i64) << shift;
            shift += 7;
            if b & 0x80 == 0 {
                if shift < 64 && (b & 0x40) != 0 {
                    result |= -1i64 << shift; // sign-extend
                }
                return Ok(result);
            }
        }
    }
    pub fn f32(&mut self) -> R<f32> {
        Ok(f32::from_le_bytes(self.bytes(4)?.try_into().unwrap()))
    }
    pub fn f64(&mut self) -> R<f64> {
        Ok(f64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }
    pub fn name(&mut self) -> R<String> {
        let len = self.u32()? as usize;
        let bytes = self.bytes(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| "wasm: invalid utf-8 in name".into())
    }
}

fn val_type(b: u8) -> Result<ValType, String> {
    match b {
        0x7f => Ok(ValType::I32),
        0x7e => Ok(ValType::I64),
        0x7d => Ok(ValType::F32),
        0x7c => Ok(ValType::F64),
        0x70 => Ok(ValType::FuncRef),
        0x6f => Ok(ValType::ExternRef),
        other => Err(format!("wasm: unknown value type 0x{other:x}")),
    }
}

/// The most pages a 32-bit memory can have (4 GiB).
pub const MAX_MEMORY_PAGES: u32 = 65536;
/// The largest initial table size accepted (V8's `kV8MaxWasmTableInitEntries`).
pub const MAX_TABLE_SIZE: u32 = 10_000_000;
/// The most locals (parameters included) a function may declare (V8's `kV8MaxWasmFunctionLocals`).
pub const MAX_FUNCTION_LOCALS: u64 = 50_000;

fn limits(r: &mut Reader) -> Result<Limits, String> {
    let flag = r.byte()?;
    if flag > 1 {
        return Err(format!("wasm: unsupported limits flag {flag}"));
    }
    let min = r.u32()?;
    let max = if flag & 1 != 0 { Some(r.u32()?) } else { None };
    if max.is_some_and(|max| max < min) {
        return Err("wasm: limits maximum is below the minimum".into());
    }
    Ok(Limits { min, max })
}

fn memory_limits(r: &mut Reader) -> Result<Limits, String> {
    let l = limits(r)?;
    if l.min > MAX_MEMORY_PAGES || l.max.is_some_and(|max| max > MAX_MEMORY_PAGES) {
        return Err("wasm: memory size must be at most 65536 pages (4GiB)".into());
    }
    Ok(l)
}

fn table_type(r: &mut Reader) -> Result<TableType, String> {
    let elem = val_type(r.byte()?)?;
    if !matches!(elem, ValType::FuncRef | ValType::ExternRef) {
        return Err("wasm: table element type must be a reference type".into());
    }
    let limits = limits(r)?;
    if limits.min > MAX_TABLE_SIZE {
        return Err(format!(
            "wasm: initial table size {} exceeds the limit of {MAX_TABLE_SIZE} elements",
            limits.min
        ));
    }
    Ok(TableType { elem, limits })
}

fn global_type(r: &mut Reader) -> Result<GlobalType, String> {
    let val = val_type(r.byte()?)?;
    let mutable = match r.byte()? {
        0 => false,
        1 => true,
        _ => return Err("wasm: bad global mutability".into()),
    };
    Ok(GlobalType { val, mutable })
}

/// Read a constant/offset init expression (raw bytes) up to and including the terminating `end`.
fn read_const_expr(r: &mut Reader) -> Result<Vec<u8>, String> {
    let start = r.pos;
    let mut depth = 0;
    loop {
        let op = r.byte()?;
        match op {
            0x41 => {
                r.i32()?;
            } // i32.const
            0x42 => {
                r.i64_leb()?;
            } // i64.const
            0x43 => {
                r.bytes(4)?;
            } // f32.const
            0x44 => {
                r.bytes(8)?;
            } // f64.const
            0x23 => {
                r.u32()?;
            } // global.get
            0xd0 => {
                r.byte()?;
            } // ref.null t
            0xd2 => {
                r.u32()?;
            } // ref.func
            0x0b => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            _ => return Err(format!("wasm: unsupported opcode 0x{op:x} in const expr")),
        }
    }
    Ok(r.data[start..r.pos].to_vec())
}

/// Decode and validate a module binary.
pub fn decode(data: &[u8]) -> Result<Rc<Module>, String> {
    let mut r = Reader::new(data);
    if r.bytes(4)? != b"\0asm" {
        return Err("wasm: bad magic".into());
    }
    if r.bytes(4)? != 1u32.to_le_bytes() {
        return Err("wasm: unsupported version".into());
    }

    let mut m = Module::default();
    let mut last_section = 0u8;
    let mut data_count = None;
    while !r.eof() {
        let id = r.byte()?;
        let size = r.u32()? as usize;
        // A section decoder cannot consume bytes belonging to the next section.
        let payload = r.bytes(size).map_err(|_| "wasm: section overruns input")?;
        let mut r = Reader::new(payload);
        // IDs are not ranks: bulk-memory DataCount precedes Code and Data.
        // Custom sections can repeat anywhere; unsupported section kinds still fail.
        if id != 0 {
            let rank = match id {
                1..=9 => id,
                12 => 10,
                10 => 11,
                11 => 12,
                other => return Err(format!("wasm: unknown section id {other}")),
            };
            if rank <= last_section {
                return Err("wasm: sections out of order".into());
            }
            last_section = rank;
        }
        match id {
            0 => {
                r.name()?;
                r.pos = payload.len(); // named opaque custom payload
            }
            1 => decode_types(&mut r, &mut m)?,
            2 => decode_imports(&mut r, &mut m)?,
            3 => decode_functions(&mut r, &mut m)?,
            4 => {
                for _ in 0..r.u32()? {
                    m.tables.push(table_type(&mut r)?);
                }
            }
            5 => {
                for _ in 0..r.u32()? {
                    m.memories.push(memory_limits(&mut r)?);
                }
            }
            6 => decode_globals(&mut r, &mut m)?,
            7 => decode_exports(&mut r, &mut m)?,
            8 => m.start = Some(r.u32()?),
            9 => decode_elems(&mut r, &mut m)?,
            10 => decode_code(&mut r, &mut m)?,
            11 => decode_data(&mut r, &mut m)?,
            12 => data_count = Some(r.u32()?),
            other => return Err(format!("wasm: unknown section id {other}")),
        }
        if r.pos != payload.len() {
            return Err(format!("wasm: section {id} size mismatch"));
        }
    }
    if data_count.is_some_and(|count| count as usize != m.data.len()) {
        return Err("wasm: data count does not match data segments".into());
    }
    if m.code.len() != m.func_types.len() {
        return Err("wasm: code/function count mismatch".into());
    }
    super::validate::module(&m, data_count)?;
    Ok(Rc::new(m))
}

#[cfg(test)]
#[path = "parse_tests.rs"]
mod tests;

fn decode_types(r: &mut Reader, m: &mut Module) -> Result<(), String> {
    for _ in 0..r.u32()? {
        if r.byte()? != 0x60 {
            return Err("wasm: expected func type (0x60)".into());
        }
        let mut params = Vec::new();
        for _ in 0..r.u32()? {
            params.push(val_type(r.byte()?)?);
        }
        let mut results = Vec::new();
        for _ in 0..r.u32()? {
            results.push(val_type(r.byte()?)?);
        }
        m.types.push(FuncType { params, results });
    }
    Ok(())
}

fn decode_imports(r: &mut Reader, m: &mut Module) -> Result<(), String> {
    for _ in 0..r.u32()? {
        let module = r.name()?;
        let name = r.name()?;
        let kind = match r.byte()? {
            0x00 => {
                let t = r.u32()?;
                if t as usize >= m.types.len() {
                    return Err("wasm: imported function type index out of range".into());
                }
                m.imported_func_count += 1;
                ImportKind::Func(t)
            }
            0x01 => {
                m.imported_table_count += 1;
                ImportKind::Table(table_type(r)?)
            }
            0x02 => {
                m.imported_mem_count += 1;
                ImportKind::Memory(memory_limits(r)?)
            }
            0x03 => {
                m.imported_global_count += 1;
                ImportKind::Global(global_type(r)?)
            }
            other => return Err(format!("wasm: unknown import kind {other}")),
        };
        m.imports.push(Import { module, name, kind });
    }
    Ok(())
}

fn decode_functions(r: &mut Reader, m: &mut Module) -> Result<(), String> {
    for _ in 0..r.u32()? {
        let t = r.u32()?;
        if t as usize >= m.types.len() {
            return Err("wasm: function type index out of range".into());
        }
        m.func_types.push(t);
    }
    Ok(())
}

fn decode_globals(r: &mut Reader, m: &mut Module) -> Result<(), String> {
    for _ in 0..r.u32()? {
        let ty = global_type(r)?;
        let init = read_const_expr(r)?;
        m.globals.push(Global { ty, init });
    }
    Ok(())
}

fn decode_exports(r: &mut Reader, m: &mut Module) -> Result<(), String> {
    for _ in 0..r.u32()? {
        let name = r.name()?;
        let kind = match r.byte()? {
            0x00 => ExportKind::Func,
            0x01 => ExportKind::Table,
            0x02 => ExportKind::Memory,
            0x03 => ExportKind::Global,
            other => return Err(format!("wasm: unknown export kind {other}")),
        };
        let index = r.u32()?;
        m.exports.push(Export { name, kind, index });
    }
    Ok(())
}

fn decode_elems(r: &mut Reader, m: &mut Module) -> Result<(), String> {
    for _ in 0..r.u32()? {
        let flags = r.u32()?;
        // Support the common active-segment forms (flags 0 and 2); others are rejected.
        match flags {
            0 => {
                let offset = read_const_expr(r)?;
                let mut func_indices = Vec::new();
                for _ in 0..r.u32()? {
                    func_indices.push(r.u32()?);
                }
                m.elems.push(ElemSegment {
                    table: 0,
                    offset,
                    func_indices,
                });
            }
            2 => {
                let table = r.u32()?;
                let offset = read_const_expr(r)?;
                if r.byte()? != 0x00 {
                    return Err("wasm: unsupported element kind".into());
                }
                let mut func_indices = Vec::new();
                for _ in 0..r.u32()? {
                    func_indices.push(r.u32()?);
                }
                m.elems.push(ElemSegment {
                    table,
                    offset,
                    func_indices,
                });
            }
            other => return Err(format!("wasm: unsupported element segment kind {other}")),
        }
    }
    Ok(())
}

fn decode_code(r: &mut Reader, m: &mut Module) -> Result<(), String> {
    let count = r.u32()?;
    if count as usize != m.func_types.len() {
        return Err("wasm: code/function count mismatch".into());
    }
    for i in 0..count as usize {
        let size = r.u32()? as usize;
        // Locals and instructions are read from the body alone, so they cannot overrun it.
        let mut body = Reader::new(r.bytes(size).map_err(|_| "wasm: bad code body")?);
        let params = m
            .types
            .get(m.func_types[i] as usize)
            .map_or(0, |t| t.params.len() as u64);
        let mut groups = Vec::new();
        let mut total = params;
        for _ in 0..body.u32()? {
            let n = body.u32()?;
            let ty = val_type(body.byte()?)?;
            total += n as u64;
            if total > MAX_FUNCTION_LOCALS {
                return Err("wasm: too many locals".into());
            }
            groups.push((n, ty));
        }
        let mut locals = Vec::with_capacity((total - params) as usize);
        for (n, ty) in groups {
            locals.extend(std::iter::repeat_n(ty, n as usize));
        }
        // Remaining bytes (minus the trailing `end`) are the instruction stream.
        let rest = &body.data[body.pos..];
        let Some((&last, code)) = rest.split_last() else {
            return Err("wasm: function body not terminated by end".into());
        };
        if last != 0x0b {
            return Err("wasm: function body not terminated by end".into());
        }
        m.code.push(FuncBody {
            locals,
            code: code.to_vec(),
        });
    }
    Ok(())
}

fn decode_data(r: &mut Reader, m: &mut Module) -> Result<(), String> {
    for _ in 0..r.u32()? {
        let flags = r.u32()?;
        match flags {
            0 => {
                let offset = read_const_expr(r)?;
                let len = r.u32()? as usize;
                let bytes = r.bytes(len)?.to_vec();
                m.data.push(DataSegment {
                    active: Some((0, offset)),
                    bytes,
                });
            }
            1 => {
                let len = r.u32()? as usize;
                let bytes = r.bytes(len)?.to_vec();
                m.data.push(DataSegment {
                    active: None,
                    bytes,
                });
            }
            2 => {
                let memidx = r.u32()?;
                let offset = read_const_expr(r)?;
                let len = r.u32()? as usize;
                let bytes = r.bytes(len)?.to_vec();
                m.data.push(DataSegment {
                    active: Some((memidx, offset)),
                    bytes,
                });
            }
            other => return Err(format!("wasm: unsupported data segment kind {other}")),
        }
    }
    Ok(())
}

/// Whether `data` is a valid module (`WebAssembly.validate`).
pub fn validate(data: &[u8]) -> bool {
    decode(data).is_ok()
}
