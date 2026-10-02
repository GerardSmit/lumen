//! Host-only JS native metadata. The payload never contains AST, bytecode or pointers.

use super::CompiledUnit;
use crate::bytecode::{NativeCaptureInit, NativeFunction};
use crate::value::Value;
use std::rc::Rc;

#[path = "native_bindings.rs"]
mod bindings;
#[path = "native_classes.rs"]
mod classes;
pub(super) use bindings::entry_body;

pub struct NativeUnit {
    /// Entry first, then functions in the stable snapshot order.
    pub functions: Vec<NativeFunction>,
    /// Function records for LUMJSN02; references use global function indices.
    pub metadata: Vec<u8>,
    pub bindings: Vec<u8>,
    /// Function-entry source locations; missing for synthetic expressions.
    pub locations: Vec<Option<(u32, u32)>>,
    pub snapshot_functions: NativeSnapshotFunctions,
}

#[derive(Default)]
pub struct NativeSnapshotFunctions(Vec<(Rc<crate::ast::Function>, u32, bool)>);

pub struct NativeSnapshotBaseline(Vec<(String, Value)>);

impl NativeSnapshotBaseline {
    pub fn new(engine: &crate::Engine) -> Self { Self(engine.snapshot_intrinsics()) }
}

impl NativeSnapshotFunctions {
    pub fn extend(&mut self, other: Self) { self.0.extend(other.0); }

    pub fn capture(&self, engine: &crate::Engine, baseline: &NativeSnapshotBaseline) -> Result<crate::heap_snapshot::Snapshot, String> {
        use crate::ast::FnSource;
        engine.capture_initialized_heap(|function, force_derived| {
            let mut matches = self.0.iter().filter(|(known, _, derived)| {
                *derived == force_derived && (Rc::ptr_eq(known, function) || (known.name == function.name
                    && known.is_arrow == function.is_arrow && known.is_method == function.is_method
                    && known.is_fn_expr == function.is_fn_expr && known.expr_body == function.expr_body
                    && known.is_strict == function.is_strict && known.is_async == function.is_async
                    && known.is_generator == function.is_generator && known.params.len() == function.params.len()
                    && match (&known.source, &function.source) {
                        (FnSource::Range { src: a, start: sa, end: ea }, FnSource::Range { src: b, start: sb, end: eb }) => a == b && sa == sb && ea == eb,
                        (FnSource::None, FnSource::None) => {
                            let actual = if force_derived { crate::bytecode::compile_derived(function) } else { crate::bytecode::compile(function) };
                            known.code.get().and_then(Option::as_ref).zip(actual.as_ref())
                                .is_some_and(|(known, actual)| known.native_equivalent(actual))
                        }
                        _ => false,
                    }))
            });
            let index = matches.next().map(|(_, index, _)| *index)?;
            if matches.next().is_some() { None } else { Some(index) }
        }, |value| baseline.0.iter().find_map(|(name, known)| {
            let identical = match (known, value) {
                (Value::Obj(a), Value::Obj(b)) => crate::value::Gc::ptr_eq(a, b),
                (Value::Sym(a), Value::Sym(b)) => Rc::ptr_eq(a, b),
                _ => false,
            };
            identical.then(|| name.clone())
        }))
    }
}

pub(crate) fn count(out: &mut Vec<u8>, value: usize) -> Result<(), String> {
    let value = u32::try_from(value).map_err(|_| "native metadata exceeds 32-bit limits")?;
    out.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn string(out: &mut Vec<u8>, value: &str) -> Result<(), String> {
    count(out, value.len())?;
    out.extend_from_slice(value.as_bytes());
    Ok(())
}

pub(super) fn synthetic(body: Vec<crate::ast::Stmt>, name: &str, arrow: bool, strict: bool, method: bool) -> Rc<crate::ast::Function> {
    use std::cell::{Cell, OnceCell, RefCell};
    Rc::new(crate::ast::Function {
        name: Some(name.into()), params: Vec::new(), body: RefCell::new(Some(Rc::new(body))),
        lazy: RefCell::new(None), body_used: Cell::new(false), lazy_error: OnceCell::new(),
        is_arrow: arrow, is_strict: strict, expr_body: false, is_generator: false,
        is_async: false, is_method: method, is_fn_expr: false,
        source: crate::ast::FnSource::None, scan: Cell::new(0), hoist: RefCell::new(None),
        calls: Cell::new(0), code: OnceCell::new(), fn_maps: OnceCell::new(),
    })
}

impl CompiledUnit {
    pub fn native_snapshot_warnings(&self) -> Result<Vec<String>, String> {
        use crate::token::Tok;
        let tokens = crate::lexer::tokenize_goal(&self.native_source, self.kind != crate::SourceKind::Module).map_err(|error| error.message)?;
        let mut warnings = std::collections::BTreeSet::new();
        for index in 0..tokens.len() {
            if let Tok::Ident(name) = &tokens[index].kind {
                if name.as_str() == "Date" { warnings.insert("snapshot source references Date; initialized values may depend on host time".to_owned()); }
                if name.as_str() == "Math" && tokens.get(index + 2).is_some_and(|token|
                    matches!(&token.kind, Tok::Ident(name) if name.as_str() == "random")) {
                    warnings.insert("snapshot source references Math.random; initialized values may be nondeterministic".to_owned());
                }
                if name.as_str() == "process" && tokens.get(index + 2).is_some_and(|token| match &token.kind {
                    Tok::Ident(name) => name.as_str() == "env", Tok::Str(name) => &**name == "env", _ => false }) {
                    warnings.insert("snapshot source reads process.env; initialized values may depend on the host environment".to_owned());
                }
            }
        }
        Ok(warnings.into_iter().collect())
    }

    /// Remove unreachable module-local function declarations. Exported functions
    /// and nonfunction statements remain roots; lexical identifier shadowing may retain extras.
    pub fn trim_native_members(&self, keep: impl Fn(&str) -> bool) -> Result<(Self, Vec<String>, Vec<String>), String> {
        use crate::ast::{FnSource, Stmt};
        use crate::token::{Tok, TokVec, TplPart};
        if self.kind != crate::SourceKind::Module {
            return Ok((self.clone(), Vec::new(), vec!["global/CommonJS bindings are retained because their dynamic namespace is observable".into()]));
        }
        let body = bindings::entry_body(&self.native_body, &self.native_source)?;
        let candidates = body.iter().filter_map(|statement| match statement {
            Stmt::FuncDecl(function) => function.name.as_ref().map(|name| (name.clone(), function.clone())),
            _ => None,
        }).collect::<Vec<_>>();
        let Some((_, first)) = candidates.first() else { return Ok((self.clone(), Vec::new(), Vec::new())); };
        let FnSource::Range { src, .. } = &first.source else {
            return Ok((self.clone(), Vec::new(), vec!["module function source positions unavailable; declarations retained".into()]));
        };
        fn refs(tokens: &TokVec, spans: &[(u32, u32)], out: &mut std::collections::BTreeSet<String>) {
            for index in 0..tokens.len() {
                let token = &tokens[index];
                if spans.iter().any(|(start, end)| token.start >= *start && token.start < *end) { continue; }
                match &token.kind {
                    Tok::Ident(name) => { out.insert(name.as_str().to_owned()); }
                    Tok::Template(parts) => for part in parts.iter() { if let TplPart::Sub(sub) = part { refs(&sub.0, spans, out); } },
                    _ => {}
                }
            }
        }
        let spans = candidates.iter().filter_map(|(_, function)| match &function.source {
            FnSource::Range { start, end, .. } => Some((*start, *end)), _ => None,
        }).collect::<Vec<_>>();
        let mut reachable = std::collections::BTreeSet::new();
        let tokens = crate::lexer::tokenize_goal(src, false).map_err(|error| error.message)?;
        refs(&tokens, &spans, &mut reachable);
        for (name, _) in &candidates { if keep(name) { reachable.insert(name.clone()); } }
        for statement in &self.native_body {
            match statement {
                Stmt::ExportDecl(inner) | Stmt::ExportDefault(inner) => if let Stmt::FuncDecl(function) = &**inner {
                    if let Some(name) = &function.name { reachable.insert(name.clone()); }
                },
                Stmt::ExportNamed { specs, source: None } => for spec in specs { reachable.insert(spec.local.clone()); },
                _ => {}
            }
        }
        loop {
            let before = reachable.len();
            for (name, function) in &candidates {
                if reachable.contains(name) {
                    if let Some(source) = function.source() {
                        let tokens = crate::lexer::tokenize_goal(&source, false).map_err(|error| error.message)?;
                        refs(&tokens, &[], &mut reachable);
                    }
                }
            }
            if reachable.len() == before { break; }
        }
        let removed = candidates.iter().filter(|(name, _)| !reachable.contains(name)).collect::<Vec<_>>();
        let mut trimmed = self.clone();
        trimmed.native_body.retain(|statement| !matches!(statement, Stmt::FuncDecl(function)
            if removed.iter().any(|(_, known)| Rc::ptr_eq(function, known))));
        trimmed.native_funcs.retain(|function| !removed.iter().any(|(_, removed)| match (&function.source, &removed.source) {
            (FnSource::Range { src: a, start: sa, end: ea }, FnSource::Range { src: b, start: sb, end: eb }) => a == b && sa >= sb && ea <= eb,
            _ => Rc::ptr_eq(function, removed),
        }));
        let dynamic = (0..tokens.len()).any(|index| match &tokens[index].kind {
            Tok::Ident(name) => matches!(name.as_str(), "Reflect" | "eval" | "Function"),
            Tok::Punct("[") if index > 0 && index + 1 < tokens.len() => {
                matches!(&tokens[index - 1].kind, Tok::Ident(_) | Tok::Punct(")" | "]"))
                    && !matches!(&tokens[index + 1].kind, Tok::Str(_) | Tok::Num(_))
            }
            _ => false,
        });
        Ok((trimmed, removed.iter().map(|(name, _)| name.clone()).collect(),
            if dynamic { vec!["dynamic property access/reflection retains observable members; keep the unit explicitly for aggressive trimming".into()] } else { Vec::new() }))
    }

    pub fn native_trim_source_bytes(&self, removed: &[String]) -> usize {
        self.native_body.iter().filter_map(|statement| match statement {
            crate::ast::Stmt::FuncDecl(function) if function.name.as_ref().is_some_and(|name| removed.contains(name)) => match &function.source {
                crate::ast::FnSource::Range { start, end, .. } => Some((*end - *start) as usize), _ => None,
            }, _ => None,
        }).sum()
    }

    pub fn native_snapshot_entry(&self, name: &str, base: u32) -> Result<u32, String> {
        use crate::ast::{FnSource, Stmt};
        let body = bindings::entry_body(&self.native_body, &self.native_source)?;
        let function = body.iter().rev().find_map(|statement| match statement {
            Stmt::FuncDecl(function) if function.name.as_deref() == Some(name) => Some(function),
            _ => None,
        }).ok_or_else(|| format!("snapshot entry {name:?} must be a top-level function declaration"))?;
        let FnSource::Range { src, .. } = &function.source else {
            return Err("snapshot entry requires original host source positions".into());
        };
        fn called(tokens: &crate::token::TokVec, name: &str, functions: &[Rc<crate::ast::Function>]) -> bool {
            use crate::token::{Tok, TplPart};
            for index in 0..tokens.len() {
                let token = &tokens[index];
                if functions.iter().any(|function| matches!(&function.source,
                    FnSource::Range { start, end, .. } if token.start >= *start && token.start < *end)) { continue; }
                if let Tok::Template(parts) = &token.kind {
                    if parts.iter().any(|part| matches!(part, TplPart::Sub(sub) if called(&sub.0, name, functions))) { return true; }
                }
                if !matches!(&token.kind, Tok::Ident(identifier) if identifier.as_str() == name)
                    || index > 0 && matches!(&tokens[index - 1].kind, Tok::Punct("." | "?.")) { continue; }
                let mut next = index + 1;
                while tokens.get(next).is_some_and(|token| matches!(&token.kind, Tok::Punct(")"))) { next += 1; }
                if tokens.get(next).is_some_and(|token| matches!(&token.kind, Tok::Punct("(")))
                    || tokens.get(next).is_some_and(|token| matches!(&token.kind, Tok::Punct("?.")))
                    && tokens.get(next + 1).is_some_and(|token| matches!(&token.kind, Tok::Punct("("))) { return true; }
            }
            false
        }
        let tokens = crate::lexer::tokenize_goal(src, self.kind != crate::SourceKind::Module).map_err(|error| error.message)?;
        if called(&tokens, name, &self.native_funcs) {
            return Err(format!("snapshot initialization must not directly call entry {name:?}"));
        }
        let index = self.native_funcs.iter().position(|known| Rc::ptr_eq(known, function))
            .ok_or("snapshot entry was not compiled")?;
        base.checked_add(u32::try_from(index).map_err(|_| "too many native functions")?)
            .and_then(|index| index.checked_add(1)).ok_or_else(|| "too many native functions".into())
    }

    /// Build native IR and AST-free runtime records. Nonliteral object constants
    /// and expressions refused by the compiler are build errors.
    pub fn compile_native(&self, pointer_width: u8, function_base: u32) -> Result<NativeUnit, String> {
        self.compile_native_with_profile(pointer_width, function_base, None)
    }

    pub fn compile_native_with_profile(&self, pointer_width: u8, function_base: u32, profile: Option<&crate::feedback::Profile>) -> Result<NativeUnit, String> {
        if self.kind == crate::SourceKind::CommonJs { bindings::validate_cjs(&self.native_body, &self.native_source)?; }
        let mut chunks = std::iter::once(self.native_entry_chunk()?)
            .chain(self.native_chunks()?).collect::<Vec<_>>();
        let mut metadata = Vec::new();
        let mut functions = Vec::with_capacity(chunks.len());
        let mut templates = self.native_funcs.clone();
        let derived_constructors = chunks.iter().flat_map(|chunk| chunk.native_classes()).filter(|class| class.superclass.is_some())
            .flat_map(|class| class.members.iter()).filter(|member| matches!(member.kind, crate::ast::MemberKind::Constructor))
            .filter_map(|member| member.func.clone()).collect::<Vec<_>>();
        for (index, function) in templates.iter().enumerate() {
            if derived_constructors.iter().any(|constructor| Rc::ptr_eq(constructor, function)) {
                chunks[index + 1] = crate::bytecode::compile_derived(function)
                    .ok_or("native compilation refused a derived constructor")?;
            }
        }
        let lines = crate::interpreter::stack_trace::LineTable::decode(&self.lines)
            .ok_or("invalid host source line table")?;
        let mut locations = Vec::new();
        let mut index = 0;
        while index < chunks.len() {
            let chunk = chunks[index].clone();
            let native = match profile {
                Some(profile) => chunk.build_native_with_profile(pointer_width, profile),
                None => chunk.build_native(pointer_width),
            }.map_err(|error| {
                let offset = if index == 0 { 0 } else { match &templates[index - 1].source {
                    crate::ast::FnSource::Range { start, .. } => *start,
                    _ => chunk.native_positions().first().map_or(0, |(_, position)| *position),
                }};
                let (line, column) = lines.line_col(offset).unwrap_or((1, 1));
                format!("{error} at {line}:{column}")
            })?;
            let (name, flags, length) = if index == 0 {
                ("<entry>", 64 | u32::from(self.native_entry_async()) << 3
                    | if self.kind == crate::SourceKind::Module { 2 } else { 0 }, 0)
            } else {
                let f = &templates[index - 1];
                (f.name.as_deref().unwrap_or(""),
                    u32::from(f.is_arrow) | u32::from(f.is_strict) << 1
                    | u32::from(f.is_generator) << 2 | u32::from(f.is_async) << 3
                    | u32::from(f.is_method) << 4 | u32::from(f.is_fn_expr) << 5,
                    f.params.iter().take_while(|p| p.default.is_none() && !p.rest).count())
            };
            string(&mut metadata, name)?;
            metadata.extend_from_slice(&flags.to_le_bytes());
            count(&mut metadata, length)?;
            count(&mut metadata, chunk.native_slot_count())?;
            count(&mut metadata, chunk.native_parameter_count())?;
            count(&mut metadata, native.max_stack)?;
            let layout = chunk.native_frame_layout();
            for slot in [layout.arguments_slot, layout.rest_slot, layout.virt_base] {
                metadata.extend_from_slice(&slot.map_or(u32::MAX, u32::from).to_le_bytes());
            }
            let frame_flags = u32::from(layout.uses_this) | u32::from(layout.env_this) << 1
                | u32::from(layout.derived) << 2 | u32::from(layout.reflect_args) << 3;
            metadata.extend_from_slice(&frame_flags.to_le_bytes());
            for value in [layout.property_cache_count, layout.name_cache_count, layout.object_template_count] {
                count(&mut metadata, value)?;
            }
            count(&mut metadata, layout.var_force_resets.len())?;
            for &slot in layout.var_force_resets { count(&mut metadata, usize::from(slot))?; }
            count(&mut metadata, layout.cap_inits.len())?;
            for capture in layout.cap_inits {
                match capture {
                    NativeCaptureInit::Param(slot, name) => {
                        metadata.push(0);
                        count(&mut metadata, usize::from(slot))?;
                        string(&mut metadata, &name)?;
                    }
                    NativeCaptureInit::Function(slot, name) => {
                        metadata.push(2);
                        count(&mut metadata, usize::from(slot))?;
                        string(&mut metadata, &name)?;
                    }
                    NativeCaptureInit::Var(name) => { metadata.push(1); string(&mut metadata, &name)?; }
                    NativeCaptureInit::Lexical(name, immutable) => {
                        metadata.push(3); metadata.push(u8::from(immutable)); string(&mut metadata, &name)?;
                    }
                }
            }
            count(&mut metadata, chunk.native_child_functions().len())?;
            for child in chunk.native_child_functions() {
                let local = templates.iter().position(|f| Rc::ptr_eq(f, child))
                    .ok_or("native child function is outside the unit")?;
                let global = function_base.checked_add(u32::try_from(local).map_err(|_| "too many native functions")?)
                    .and_then(|n| n.checked_add(1)).ok_or("too many native functions")?;
                metadata.extend_from_slice(&global.to_le_bytes());
            }
            for names in [chunk.native_names(), chunk.native_slot_names()] {
                count(&mut metadata, names.len())?;
                for name in names { string(&mut metadata, name)?; }
            }
            count(&mut metadata, chunk.native_constants().len())?;
            for value in chunk.native_constants() {
                match value {
                    Value::Undefined => metadata.push(0),
                    Value::Null => metadata.push(2),
                    Value::Bool(value) => metadata.push(if *value { 4 } else { 3 }),
                    Value::Num(value) => { metadata.push(5); metadata.extend_from_slice(&value.to_bits().to_le_bytes()); }
                    Value::BigInt(value) => { metadata.push(6); string(&mut metadata, &value.to_string_radix(10))?; }
                    Value::Str(value) => { metadata.push(7); string(&mut metadata, value.as_str())?; }
                    Value::Empty | Value::Sym(_) | Value::Obj(_) => return Err("native constant is not a primitive literal".into()),
                }
            }
            count(&mut metadata, native.resumes.len())?;
            for &(pc, depth) in &native.resumes { count(&mut metadata, pc)?; count(&mut metadata, depth)?; }
            let positions = chunk.native_positions();
            count(&mut metadata, positions.len())?;
            for (pc, source) in positions { metadata.extend_from_slice(&pc.to_le_bytes()); metadata.extend_from_slice(&source.to_le_bytes()); }
            count(&mut metadata, chunk.native_classes().len())?;
            for class in chunk.native_classes() {
                classes::encode(&mut metadata, class, function_base, &mut templates, &mut chunks)?;
            }
            functions.push(native);
            locations.push(if index == 0 { lines.line_col(0) } else {
                match &templates[index - 1].source {
                    crate::ast::FnSource::Range { start, .. } => lines.line_col(*start),
                    _ => None,
                }
            });
            index += 1;
        }
        let snapshot_functions = NativeSnapshotFunctions(templates.into_iter().enumerate().map(|(index, function)|
            Ok((function, function_base.checked_add(u32::try_from(index).map_err(|_| "too many native functions")?)
                .and_then(|index| index.checked_add(1)).ok_or("too many native functions")?, chunks[index + 1].native_frame_layout().derived)))
            .collect::<Result<_, String>>()?);
        Ok(NativeUnit { functions, metadata, locations, snapshot_functions,
            bindings: bindings::encode(&self.native_body, &chunks[0], &self.native_funcs, function_base, &self.native_source)? })
    }
}
