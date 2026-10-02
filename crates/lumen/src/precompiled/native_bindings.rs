//! Per-unit bindings without retaining module syntax in the native payload.

use super::{count, string};
use crate::ast::{ArrayPatElem, DeclKind, Expr, ImportSpec, Pattern, Stmt};
use crate::bytecode::Chunk;
use std::rc::Rc;

const DEFAULT: &str = "%lumen-default%";

pub(in crate::precompiled) fn entry_body(body: &[Stmt], source: &str) -> Result<Vec<Stmt>, String> {
    let mut out = Vec::new();
    for statement in body {
        match statement {
            Stmt::Import(import) => {
                let reason = if import.attr_type.is_some() { Some("native import attributes require embedded asset metadata") }
                    else if import.specs.iter().any(|spec| matches!(spec, ImportSpec::Source(_) | ImportSpec::DeferNamespace(_))) {
                        Some("native source/deferred imports are unsupported")
                    } else { None };
                if let Some(reason) = reason {
                    let tokens = crate::lexer::tokenize_goal(source, true).map_err(|error| error.message)?;
                    let offset = (0..tokens.len()).filter_map(|index| tokens.get(index)).find_map(|token| matches!(&token.kind,
                        crate::token::Tok::Str(value) if value.as_ref() == import.source.as_ref()).then_some(token.start)).unwrap_or(0);
                    let (line, column) = crate::interpreter::stack_trace::LineTable::build(source).line_col(offset).unwrap_or((1, 1));
                    return Err(format!("{reason} at {line}:{column}"));
                }
            }
            Stmt::ExportNamed { .. } | Stmt::ExportAll { .. } => {}
            Stmt::ExportDecl(statement) => out.push((**statement).clone()),
            Stmt::ExportDefault(statement) => {
                let expression = match &**statement {
                    Stmt::Expr(expression) => expression.clone(),
                    Stmt::FuncDecl(function) => {
                        if let Some(name) = &function.name {
                            out.push((**statement).clone());
                            Expr::Ident(name.clone())
                        } else { Expr::Func(function.clone()) }
                    }
                    Stmt::ClassDecl(class) => {
                        if let Some(name) = &class.name {
                            out.push((**statement).clone());
                            Expr::Ident(name.clone())
                        } else { Expr::Class(class.clone()) }
                    }
                    _ => return Err("unsupported native default export declaration".into()),
                };
                out.push(Stmt::VarDecl { kind: DeclKind::Const,
                    decls: vec![(Pattern::Ident(DEFAULT.into()), Some(expression))] });
            }
            _ => out.push(statement.clone()),
        }
    }
    Ok(out)
}

fn names(pattern: &Pattern, out: &mut Vec<String>) -> Result<(), String> {
    match pattern {
        Pattern::Ident(name) => out.push(name.clone()),
        Pattern::Array(elements) => for element in elements {
            match element { ArrayPatElem::Hole => {},
                ArrayPatElem::Elem { pattern, .. } | ArrayPatElem::Rest(pattern) => names(pattern, out)? }
        },
        Pattern::Object(object) => {
            for property in &object.props { names(&property.value, out)?; }
            if let Some(name) = &object.rest { out.push(name.clone()); }
        }
        Pattern::Member(_) => return Err("native declaration has a member binding target".into()),
    }
    Ok(())
}

fn declared(statement: &Stmt, out: &mut Vec<(u8, String)>) -> Result<(), String> {
    match statement {
        Stmt::VarDecl { kind, decls } => {
            let tag = match kind { DeclKind::Var => 0, DeclKind::Let => 1, DeclKind::Const => 2,
                DeclKind::Using => 5, DeclKind::AwaitUsing => 6 };
            for (pattern, _) in decls {
                let mut bound = Vec::new(); names(pattern, &mut bound)?;
                out.extend(bound.into_iter().map(|name| (tag, name)));
            }
        }
        Stmt::FuncDecl(function) => if let Some(name) = &function.name { out.push((3, name.clone())); },
        Stmt::ClassDecl(class) => if let Some(name) = &class.name { out.push((4, name.clone())); },
        _ => {}
    }
    Ok(())
}

pub(super) fn validate_cjs(body: &[Stmt], source: &str) -> Result<(), String> {
    let mut declarations = Vec::new();
    for statement in body { declared(statement, &mut declarations)?; }
    if let Some((_, name)) = declarations.iter().find(|(kind, name)| *kind != 0 && *kind != 3
        && matches!(name.as_str(), "module" | "exports" | "require" | "__filename" | "__dirname")) {
        let tokens = crate::lexer::tokenize_goal(source, true).map_err(|error| error.message)?;
        let offset = (0..tokens.len()).find_map(|index| matches!(&tokens[index].kind,
            crate::token::Tok::Ident(identifier) if identifier.as_str() == name).then_some(tokens[index].start)).unwrap_or(0);
        let (line, column) = crate::interpreter::stack_trace::LineTable::build(source).line_col(offset).unwrap_or((1, 1));
        return Err(format!("CommonJS lexical declaration {name:?} conflicts with a wrapper parameter at {line}:{column}"));
    }
    Ok(())
}

fn nested_vars(statement: &Stmt, out: &mut Vec<(u8, String)>) -> Result<(), String> {
    match statement {
        Stmt::VarDecl { kind: DeclKind::Var, .. } => declared(statement, out)?,
        Stmt::Block(body) => for statement in body { nested_vars(statement, out)?; },
        Stmt::If { cons, alt, .. } => {
            nested_vars(cons, out)?;
            if let Some(alt) = alt { nested_vars(alt, out)?; }
        }
        Stmt::For { init, body, .. } => {
            if let Some(init) = init {
                if let crate::ast::ForInit::VarDecl { kind: DeclKind::Var, decls } = &**init {
                    for (pattern, _) in decls {
                        let mut bound = Vec::new(); names(pattern, &mut bound)?;
                        out.extend(bound.into_iter().map(|name| (0, name)));
                    }
                }
            }
            nested_vars(body, out)?;
        }
        Stmt::ForInOf { decl, left, body, .. } => {
            if matches!(decl, Some(DeclKind::Var)) {
                let mut bound = Vec::new(); names(left, &mut bound)?;
                out.extend(bound.into_iter().map(|name| (0, name)));
            }
            nested_vars(body, out)?;
        }
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. }
        | Stmt::Labeled { body, .. } | Stmt::With { body, .. } => nested_vars(body, out)?,
        Stmt::Try { block, handler, finalizer } => {
            for statement in block { nested_vars(statement, out)?; }
            if let Some(handler) = handler { for statement in &handler.1 { nested_vars(statement, out)?; } }
            if let Some(finalizer) = finalizer { for statement in finalizer { nested_vars(statement, out)?; } }
        }
        Stmt::Switch { cases, .. } => for case in cases { for statement in &case.body { nested_vars(statement, out)?; } },
        _ => {}
    }
    Ok(())
}

pub(super) fn encode(body: &[Stmt], entry: &Chunk, functions: &[Rc<crate::ast::Function>], base: u32, source: &str) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let flattened = entry_body(body, source)?;
    let mut declarations = Vec::new();
    for statement in &flattened {
        declared(statement, &mut declarations)?;
        if !matches!(statement, Stmt::VarDecl { .. }) { nested_vars(statement, &mut declarations)?; }
    }
    let mut seen = std::collections::BTreeSet::new();
    let function_names = declarations.iter().filter(|(kind, _)| *kind == 3).map(|(_, name)| name.clone()).collect::<std::collections::BTreeSet<_>>();
    declarations.retain(|(kind, name)| (*kind == 3 || !function_names.contains(name)) && seen.insert(name.clone()));
    count(&mut out, declarations.len())?;
    for (kind, name) in &declarations {
        out.push(*kind); string(&mut out, name)?;
        let slot = entry.native_slot_names().iter().position(|slot| &**slot == name)
            .map(|i| u32::try_from(i).map_err(|_| "too many native slots"))
            .transpose()?.unwrap_or(u32::MAX);
        out.extend_from_slice(&slot.to_le_bytes());
        let declaration = flattened.iter().rev().find_map(|statement| match statement {
            Stmt::FuncDecl(function) if function.name.as_deref() == Some(name.as_str()) => Some(function),
            _ => None,
        });
        let function = declaration.and_then(|declaration| functions.iter().position(|function| Rc::ptr_eq(function, declaration)))
            .filter(|_| *kind == 3).map(|index|
                base.checked_add(u32::try_from(index).map_err(|_| "too many native functions")?)
                    .and_then(|n| n.checked_add(1)).ok_or("too many native functions"))
            .transpose()?.unwrap_or(u32::MAX);
        out.extend_from_slice(&function.to_le_bytes());
    }
    let imports = body.iter().filter_map(|statement| match statement { Stmt::Import(import) => Some(import), _ => None }).collect::<Vec<_>>();
    count(&mut out, imports.len())?;
    for import in imports {
        string(&mut out, &import.source)?; count(&mut out, import.specs.len())?;
        for spec in &import.specs {
            let (tag, imported, local) = match spec {
                ImportSpec::Default(local) => (0, "default", local),
                ImportSpec::Namespace(local) => (1, "*", local),
                ImportSpec::DeferNamespace(local) => (2, "*", local),
                ImportSpec::Source(local) => (3, "*", local),
                ImportSpec::Named { imported, local } => (4, imported.as_str(), local),
            };
            out.push(tag); string(&mut out, imported)?; string(&mut out, local)?;
        }
    }
    let mut exports = Vec::<(u8, String, String, String)>::new();
    for statement in body {
        match statement {
            Stmt::ExportNamed { specs, source } => for spec in specs {
                exports.push((u8::from(source.is_some()), source.as_deref().unwrap_or("").into(), spec.local.clone(), spec.exported.clone()));
            }
            Stmt::ExportDecl(inner) => {
                let mut bound = Vec::new(); declared(inner, &mut bound)?;
                exports.extend(bound.into_iter().map(|(_, name)| (0, String::new(), name.clone(), name)));
            }
            Stmt::ExportDefault(_) => exports.push((0, String::new(), DEFAULT.into(), "default".into())),
            Stmt::ExportAll { source, exported } => exports.push((if exported.is_some() {3} else {2}, source.to_string(), "*".into(), exported.clone().unwrap_or_default())),
            _ => {}
        }
    }
    count(&mut out, exports.len())?;
    for (tag, source, local, exported) in exports {
        out.push(tag); string(&mut out, &source)?; string(&mut out, &local)?; string(&mut out, &exported)?;
    }
    Ok(out)
}
