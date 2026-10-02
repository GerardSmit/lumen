//! Class data refers only to native functions; expressions are host compiler inputs.

use super::{count, string, synthetic};
use crate::ast::{Class, Expr, Function, MemberKind, PropKey, Stmt};
use crate::bytecode::Chunk;
use std::rc::Rc;

fn expression(expr: &Expr, field: bool, base: u32, templates: &mut Vec<Rc<Function>>, chunks: &mut Vec<Rc<Chunk>>) -> Result<u32, String> {
    let function = synthetic(vec![Stmt::Return(Some(expr.clone()))],
        if field { "<field-initializer>" } else { "<class-expression>" }, !field, true, field);
    let chunk = crate::bytecode::compile(&function).ok_or("native class expression could not compile")?;
    let _ = function.code.set(Some(chunk.clone()));
    let index = base.checked_add(u32::try_from(chunks.len()).map_err(|_| "too many native functions")?)
        .ok_or("too many native functions")?;
    templates.push(function);
    chunks.push(chunk);
    Ok(index)
}

fn expressions(out: &mut Vec<u8>, values: &[Expr], base: u32, templates: &mut Vec<Rc<Function>>, chunks: &mut Vec<Rc<Chunk>>) -> Result<(), String> {
    count(out, values.len())?;
    for value in values {
        out.extend_from_slice(&expression(value, false, base, templates, chunks)?.to_le_bytes());
    }
    Ok(())
}

pub(super) fn encode(out: &mut Vec<u8>, class: &Class, base: u32, templates: &mut Vec<Rc<Function>>, chunks: &mut Vec<Rc<Chunk>>) -> Result<(), String> {
    string(out, class.name.as_deref().unwrap_or(""))?;
    let superclass = class.superclass.as_deref().map(|value| expression(value, false, base, templates, chunks))
        .transpose()?.unwrap_or(u32::MAX);
    out.extend_from_slice(&superclass.to_le_bytes());
    expressions(out, &class.decorators, base, templates, chunks)?;
    count(out, class.members.len())?;
    for member in &class.members {
        out.push(match member.kind { MemberKind::Constructor => 0, MemberKind::Method => 1,
            MemberKind::Get => 2, MemberKind::Set => 3, MemberKind::Field => 4,
            MemberKind::Accessor => 5, MemberKind::StaticBlock => 6 });
        out.push(u8::from(member.is_static));
        match &member.key {
            PropKey::Ident(name) => { out.push(u8::from(name.starts_with('#'))); string(out, name)?; }
            PropKey::Str(name) => { out.push(0); string(out, name)?; }
            PropKey::Num(number) => { out.push(2); out.extend_from_slice(&number.to_bits().to_le_bytes()); }
            PropKey::Computed(value) => {
                out.push(3); out.extend_from_slice(&expression(value, false, base, templates, chunks)?.to_le_bytes());
            }
        }
        let method = member.func.as_ref().map(|function| {
            let index = templates.iter().position(|template| Rc::ptr_eq(template, function))
                .ok_or("native class method is outside the function table")?;
            base.checked_add(u32::try_from(index).map_err(|_| "too many native functions")?)
                .and_then(|index| index.checked_add(1)).ok_or("too many native functions")
        }).transpose()?.unwrap_or(u32::MAX);
        out.extend_from_slice(&method.to_le_bytes());
        let initializer = member.value.as_ref().map(|value| expression(value, true, base, templates, chunks))
            .transpose()?.unwrap_or(u32::MAX);
        out.extend_from_slice(&initializer.to_le_bytes());
        out.push(u8::from(member.value.as_ref().is_some_and(crate::eval::is_anonymous_fn)));
        expressions(out, &member.decorators, base, templates, chunks)?;
    }
    Ok(())
}
