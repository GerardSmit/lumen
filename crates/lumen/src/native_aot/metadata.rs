//! AST-free JavaScript payload carried inside shared native data.

use crate::bigint::JsBigInt;
use crate::value::Value;

const MAGIC_V2: &[u8; 8] = b"LUMJSN02";
const MAGIC_V3: &[u8; 8] = b"LUMJSN03";

pub(crate) struct Metadata {
    pub units: Vec<Unit>,
    pub functions: Vec<Function>,
    pub entry_unit: Option<u32>,
    pub snapshot: Option<Vec<u8>>,
    pub snapshot_entry: Option<u32>,
}

pub(crate) struct Unit {
    pub path: String,
    pub kind: u32,
    pub top_function: u32,
    pub links: Vec<(String, u32)>,
    pub declarations: Vec<Declaration>,
    pub imports: Vec<Import>,
    pub exports: Vec<Export>,
    pub requires: Vec<String>,
}

pub(crate) struct Declaration {
    pub kind: u8,
    pub name: String,
    pub slot: Option<u32>,
    pub function: Option<u32>,
}

pub(crate) struct Import {
    pub source: String,
    pub specs: Vec<(u8, String, String)>,
}

pub(crate) struct Export {
    pub kind: u8,
    pub source: String,
    pub local: String,
    pub exported: String,
}

pub(crate) struct Class {
    pub name: String,
    pub superclass: Option<u32>,
    pub decorators: Vec<u32>,
    pub members: Vec<Member>,
}

pub(crate) struct Member {
    pub kind: u8,
    pub is_static: bool,
    pub key: MemberKey,
    pub method: Option<u32>,
    pub initializer: Option<u32>,
    pub initializer_named: bool,
    pub decorators: Vec<u32>,
}

pub(crate) enum MemberKey {
    Public(String),
    Private(String),
    Number(f64),
    Computed(u32),
}

pub(crate) struct Function {
    pub name: String,
    pub flags: u32,
    pub length: u32,
    pub slots: u32,
    pub params: u32,
    pub max_stack: u32,
    pub arguments_slot: Option<u32>,
    pub rest_slot: Option<u32>,
    pub virt_base: Option<u32>,
    pub frame_flags: u32,
    pub var_resets: Vec<u32>,
    pub captures: Vec<Capture>,
    pub children: Vec<u32>,
    pub names: Vec<String>,
    pub slot_names: Vec<String>,
    pub constants: Vec<Value>,
    pub classes: Vec<Class>,
}

pub(crate) enum Capture {
    Param(u32, String),
    Var(String),
    Function(u32, String),
    Lexical(String, bool),
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8], &'static str> {
        let end = self
            .at
            .checked_add(len)
            .ok_or("native JS payload too large")?;
        let bytes = self
            .bytes
            .get(self.at..end)
            .ok_or("truncated native JS payload")?;
        self.at = end;
        Ok(bytes)
    }

    fn byte(&mut self) -> Result<u8, &'static str> {
        Ok(self.take(1)?[0])
    }

    fn word(&mut self) -> Result<u32, &'static str> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn count(&mut self, min_bytes: usize) -> Result<usize, &'static str> {
        let count = self.word()? as usize;
        if count > self.bytes.len().saturating_sub(self.at) / min_bytes {
            return Err("native JS table count exceeds payload");
        }
        Ok(count)
    }

    fn string(&mut self) -> Result<String, &'static str> {
        let len = self.word()? as usize;
        let text = std::str::from_utf8(self.take(len)?).map_err(|_| "invalid native JS string")?;
        Ok(text.to_owned())
    }

    fn optional_slot(&mut self, slots: u32) -> Result<Option<u32>, &'static str> {
        match self.word()? {
            u32::MAX => Ok(None),
            slot if slot < slots => Ok(Some(slot)),
            _ => Err("native JS slot out of range"),
        }
    }

    fn function(&mut self, functions: usize) -> Result<Option<u32>, &'static str> {
        match self.word()? {
            u32::MAX => Ok(None),
            index if (index as usize) < functions => Ok(Some(index)),
            _ => Err("native JS function reference out of range"),
        }
    }

    fn functions(&mut self, functions: usize) -> Result<Vec<u32>, &'static str> {
        let count = self.count(4)?;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(
                self.function(functions)?
                    .ok_or("missing native JS function")?,
            );
        }
        Ok(values)
    }
}

pub(crate) fn decode(bytes: &[u8], native_functions: usize) -> Result<Metadata, &'static str> {
    let mut reader = Reader { bytes, at: 0 };
    let magic = reader.take(8)?;
    let version = reader.word()?;
    if !((magic == MAGIC_V2 && version == 2) || (magic == MAGIC_V3 && version == 3)) {
        return Err("unsupported native JS payload");
    }
    let function_count = reader.word()? as usize;
    let unit_count = reader.word()? as usize;
    let entry_unit = match reader.word()? {
        u32::MAX => None,
        index if (index as usize) < unit_count => Some(index),
        _ => return Err("native JS entry unit out of range"),
    };
    if function_count == 0 || function_count != native_functions {
        return Err("native JS function count differs from native image");
    }
    if unit_count > (bytes.len() - reader.at) / 16 {
        return Err("native JS unit count exceeds payload");
    }
    let mut units = Vec::with_capacity(unit_count);
    for _ in 0..unit_count {
        let path = reader.string()?;
        let kind = reader.word()?;
        let top_function = reader.word()?;
        if path.is_empty() || kind > 2 || top_function as usize >= function_count {
            return Err("invalid native JS unit");
        }
        let links_count = reader.count(8)?;
        let mut links = Vec::with_capacity(links_count);
        for _ in 0..links_count {
            let specifier = reader.string()?;
            let target = reader.word()?;
            if specifier.is_empty() || target as usize >= unit_count {
                return Err("invalid native JS module link");
            }
            links.push((specifier, target));
        }
        let declaration_count = reader.count(13)?;
        let mut declarations = Vec::with_capacity(declaration_count);
        for _ in 0..declaration_count {
            let kind = reader.byte()?;
            let name = reader.string()?;
            let slot = reader.word()?;
            let function = reader.function(function_count)?;
            if kind > 6 || name.is_empty() || (kind != 3 && function.is_some()) {
                return Err("invalid native JS declaration");
            }
            declarations.push(Declaration {
                kind,
                name,
                slot: (slot != u32::MAX).then_some(slot),
                function,
            });
        }
        let import_count = reader.count(8)?;
        let mut imports = Vec::with_capacity(import_count);
        for _ in 0..import_count {
            let source = reader.string()?;
            if source.is_empty() {
                return Err("empty native JS import source");
            }
            let spec_count = reader.count(9)?;
            let mut specs = Vec::with_capacity(spec_count);
            for _ in 0..spec_count {
                let tag = reader.byte()?;
                let imported = reader.string()?;
                let local = reader.string()?;
                if tag > 4 || local.is_empty() {
                    return Err("invalid native JS import binding");
                }
                specs.push((tag, imported, local));
            }
            imports.push(Import { source, specs });
        }
        let export_count = reader.count(13)?;
        let mut exports = Vec::with_capacity(export_count);
        for _ in 0..export_count {
            let kind = reader.byte()?;
            let source = reader.string()?;
            let local = reader.string()?;
            let exported = reader.string()?;
            if kind > 3 || (kind == 0 && source != "") || (kind != 0 && source.is_empty()) {
                return Err("invalid native JS export binding");
            }
            exports.push(Export {
                kind,
                source,
                local,
                exported,
            });
        }
        let mut requires = Vec::new();
        if version >= 3 {
            let require_count = reader.count(4)?;
            requires.reserve(require_count);
            for _ in 0..require_count {
                let specifier = reader.string()?;
                if specifier.is_empty() || requires.contains(&specifier) {
                    return Err("invalid native JS CommonJS require");
                }
                requires.push(specifier);
            }
            if kind != 2 && !requires.is_empty() {
                return Err("CommonJS requires on a non-CommonJS unit");
            }
        }
        units.push(Unit {
            path,
            kind,
            top_function,
            links,
            declarations,
            imports,
            exports,
            requires,
        });
    }
    let mut functions = Vec::with_capacity(function_count);
    for _ in 0..function_count {
        let name = reader.string()?;
        let flags = reader.word()?;
        let length = reader.word()?;
        let slots = reader.word()?;
        let params = reader.word()?;
        let max_stack = reader.word()?;
        if flags & !0x7f != 0 || params > slots || length > params {
            return Err("invalid native JS function layout");
        }
        let arguments_slot = reader.optional_slot(slots)?;
        let rest_slot = reader.optional_slot(slots)?;
        let virt_base = reader.optional_slot(slots)?;
        let frame_flags = reader.word()?;
        if frame_flags & !0x0f != 0 {
            return Err("unsupported native JS frame flags");
        }
        for _ in 0..3 {
            reader.word()?;
        }
        let reset_count = reader.count(4)?;
        let mut var_resets = Vec::with_capacity(reset_count);
        for _ in 0..reset_count {
            let slot = reader.word()?;
            if slot >= slots {
                return Err("native JS reset slot out of range");
            }
            var_resets.push(slot);
        }
        let capture_count = reader.count(1)?;
        let mut captures = Vec::with_capacity(capture_count);
        for _ in 0..capture_count {
            captures.push(match reader.byte()? {
                0 => {
                    let slot = reader.word()?;
                    if slot >= slots {
                        return Err("native JS capture slot out of range");
                    }
                    Capture::Param(slot, reader.string()?)
                }
                1 => Capture::Var(reader.string()?),
                2 => Capture::Function(reader.word()?, reader.string()?),
                3 => {
                    let immutable = reader.byte()?;
                    if immutable > 1 {
                        return Err("invalid native JS lexical capture");
                    }
                    Capture::Lexical(reader.string()?, immutable != 0)
                }
                _ => return Err("unknown native JS capture kind"),
            });
        }
        let child_count = reader.count(4)?;
        let mut children = Vec::with_capacity(child_count);
        for _ in 0..child_count {
            let child = reader.word()?;
            if child as usize >= function_count {
                return Err("native JS child function out of range");
            }
            children.push(child);
        }
        if captures.iter().any(|capture| {
            matches!(capture, Capture::Function(index, _) if *index as usize >= children.len())
        }) {
            return Err("native JS captured function out of range");
        }
        let read_names = |reader: &mut Reader<'_>| -> Result<Vec<String>, &'static str> {
            let count = reader.count(4)?;
            (0..count).map(|_| reader.string()).collect()
        };
        let names = read_names(&mut reader)?;
        let slot_names = read_names(&mut reader)?;
        if slot_names.len() != slots as usize {
            return Err("native JS slot names disagree with layout");
        }
        let constant_count = reader.count(1)?;
        let mut constants = Vec::with_capacity(constant_count);
        for _ in 0..constant_count {
            let constant = match reader.byte()? {
                0 => Value::Undefined,
                2 => Value::Null,
                3 => Value::Bool(false),
                4 => Value::Bool(true),
                5 => Value::Num(f64::from_bits(u64::from_le_bytes(
                    reader.take(8)?.try_into().unwrap(),
                ))),
                6 => Value::BigInt(
                    JsBigInt::parse_radix(&reader.string()?, 10)
                        .ok_or("invalid native JS bigint")?,
                ),
                7 => {
                    let text = reader.string()?;
                    Value::Str(text.as_str().into())
                }
                _ => return Err("unsupported native JS constant"),
            };
            constants.push(constant);
        }
        let resume_count = reader.count(8)?;
        let mut resumes = Vec::with_capacity(resume_count);
        for _ in 0..resume_count {
            let pc = reader.word()?;
            let depth = reader.word()?;
            if depth > max_stack
                || resumes
                    .last()
                    .is_some_and(|previous: &(u32, u32)| previous.0 >= pc)
            {
                return Err("invalid native JS resume table");
            }
            resumes.push((pc, depth));
        }
        let position_count = reader.count(8)?;
        let mut positions = Vec::with_capacity(position_count);
        for _ in 0..position_count {
            let pc = reader.word()?;
            let source = reader.word()?;
            if positions
                .last()
                .is_some_and(|previous: &(u32, u32)| previous.0 >= pc)
            {
                return Err("invalid native JS source positions");
            }
            positions.push((pc, source));
        }
        let class_count = reader.count(16)?;
        let mut classes = Vec::with_capacity(class_count);
        for _ in 0..class_count {
            let name = reader.string()?;
            let superclass = reader.function(function_count)?;
            let decorators = reader.functions(function_count)?;
            let member_count = reader.count(20)?;
            let mut members = Vec::with_capacity(member_count);
            for _ in 0..member_count {
                let kind = reader.byte()?;
                let is_static = reader.byte()?;
                let key = match reader.byte()? {
                    0 => MemberKey::Public(reader.string()?),
                    1 => MemberKey::Private(reader.string()?),
                    2 => MemberKey::Number(f64::from_bits(u64::from_le_bytes(
                        reader.take(8)?.try_into().unwrap(),
                    ))),
                    3 => MemberKey::Computed(
                        reader
                            .function(function_count)?
                            .ok_or("missing native JS computed key")?,
                    ),
                    _ => return Err("unknown native JS member key"),
                };
                let method = reader.function(function_count)?;
                let initializer = reader.function(function_count)?;
                let initializer_named = reader.byte()?;
                let decorators = reader.functions(function_count)?;
                if kind > 6 || is_static > 1 || initializer_named > 1 {
                    return Err("invalid native JS class member");
                }
                members.push(Member {
                    kind,
                    is_static: is_static != 0,
                    key,
                    method,
                    initializer,
                    initializer_named: initializer_named != 0,
                    decorators,
                });
            }
            classes.push(Class {
                name,
                superclass,
                decorators,
                members,
            });
        }
        functions.push(Function {
            name,
            flags,
            length,
            slots,
            params,
            max_stack,
            arguments_slot,
            rest_slot,
            virt_base,
            frame_flags,
            var_resets,
            captures,
            children,
            names,
            slot_names,
            constants,
            classes,
        });
    }
    let (snapshot, snapshot_entry) = if version == 3 {
        let len = reader.word()? as usize;
        let snapshot = if len == 0 {
            None
        } else {
            Some(reader.take(len)?.to_vec())
        };
        let entry = reader.function(function_count)?;
        if snapshot.is_some() != entry.is_some() {
            return Err("snapshot entry and graph must occur together");
        }
        (snapshot, entry)
    } else {
        (None, None)
    };
    if reader.at != bytes.len() {
        return Err("trailing native JS metadata");
    }
    for unit in &units {
        let top = &functions[unit.top_function as usize];
        if unit
            .declarations
            .iter()
            .any(|declaration| declaration.slot.is_some_and(|slot| slot >= top.slots))
        {
            return Err("native JS top-level binding slot out of range");
        }
        if unit.imports.iter().any(|import| {
            !unit
                .links
                .iter()
                .any(|(source, _)| source == &import.source)
        }) {
            return Err("native JS import has no linked unit");
        }
        if unit.exports.iter().any(|export| {
            !export.source.is_empty()
                && !unit
                    .links
                    .iter()
                    .any(|(source, _)| source == &export.source)
        }) {
            return Err("native JS export has no linked unit");
        }
    }
    Ok(Metadata {
        units,
        functions,
        entry_unit,
        snapshot,
        snapshot_entry,
    })
}
