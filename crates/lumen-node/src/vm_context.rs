//! `node:vm` contexts and compilation (see `lumen::builtins::vm_context`): the glue in
//! `js/vm.js` validates arguments and keeps the sandbox → context map; these ops create the
//! realms and run code in them.

use lumen_host::{ops, Ctx, OpDecl, Value};

pub const VM_CONTEXT_OPS: &[OpDecl] = ops![
    "createContext" (1) => op_create_context,
    "isContext" (1) => op_is_context,
    "compileScript" (4) => op_compile_script,
    "runScript" (6) => op_run_script,
    "compileFunction" (7) => op_compile_function,
];

fn str_arg(args: &[Value], i: usize) -> String {
    match args.get(i) {
        Some(Value::Str(s)) => s.to_string(),
        _ => String::new(),
    }
}

fn int_arg(args: &[Value], i: usize) -> i32 {
    args.get(i)
        .and_then(Value::as_num_opt)
        .filter(|n| n.is_finite())
        .map_or(0, |n| n as i32)
}

fn context_arg(args: &[Value], i: usize) -> Option<&Value> {
    args.get(i).filter(|v| matches!(v, Value::Obj(_)))
}

/// `(sandbox)` — the new context's global proxy.
fn op_create_context(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let sandbox = args.first().cloned().unwrap_or(Value::Undefined);
    ctx.vm_create_context(&sandbox)
}

/// `(value)` — whether `value` is a context's global proxy.
fn op_is_context(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    Ok(Value::Bool(
        ctx.vm_is_context(args.first().unwrap_or(&Value::Undefined)),
    ))
}

/// `(code, filename, lineOffset, columnOffset)` — throw the script's SyntaxError, if any.
fn op_compile_script(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    ctx.vm_compile_script(&str_arg(args, 0), &str_arg(args, 1), int_arg(args, 2))?;
    Ok(Value::Undefined)
}

/// `(globalProxy | null, code, filename, lineOffset, columnOffset, displayErrors)` — the
/// completion value.
fn op_run_script(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let code = str_arg(args, 1);
    let filename = str_arg(args, 2);
    ctx.vm_run_script(
        context_arg(args, 0),
        &code,
        &filename,
        int_arg(args, 3),
        int_arg(args, 4),
        !matches!(args.get(5), Some(Value::Bool(false))),
    )
}

/// `(globalProxy | null, code, params[], extensions[], filename, lineOffset, columnOffset)`.
fn op_compile_function(ctx: &mut Ctx, _this: Value, args: &[Value]) -> Result<Value, Value> {
    let code = str_arg(args, 1);
    // Both lists are plain arrays the glue validated.
    let list = |ctx: &mut Ctx, v: Option<&Value>| -> Vec<Value> {
        let Some(v @ Value::Obj(_)) = v else {
            return Vec::new();
        };
        let n = ctx
            .get_member(v, "length")
            .ok()
            .and_then(|l| l.as_num_opt())
            .unwrap_or(0.0) as usize;
        (0..n)
            .map(|k| ctx.get_member(v, &k.to_string()).unwrap_or(Value::Undefined))
            .collect()
    };
    let params = list(ctx, args.get(2))
        .into_iter()
        .map(|p| match p {
            Value::Str(s) => s.to_string(),
            _ => String::new(),
        })
        .collect::<Vec<_>>();
    let extensions = list(ctx, args.get(3));
    let filename = str_arg(args, 4);
    ctx.vm_compile_function(
        context_arg(args, 0),
        &code,
        &params,
        &extensions,
        &filename,
        int_arg(args, 5),
        int_arg(args, 6),
    )
}
