//! `node:vm` contexts and compilation (see `lumen::builtins::vm_context`): the glue in
//! `js/vm.js` validates arguments and keeps the sandbox → context map; these ops create the
//! realms and run code in them.

use lumen::embed::OpError;
use lumen_host::{Ctx, Value};

pub(crate) use bindings::Module;

#[lumen_bind::module(name = "__vmc")]
pub(crate) mod bindings {
    use super::*;

    fn context_arg(v: &Value) -> Option<&Value> {
        matches!(v, Value::Obj(_)).then_some(v)
    }

    /// `(sandbox)`: the new context's global proxy.
    #[op(name = "createContext")]
    pub fn create_context(ctx: &mut Ctx, sandbox: Value) -> Result<Value, OpError> {
        ctx.vm_create_context(&sandbox).map_err(OpError::thrown)
    }

    /// `(value)`: whether `value` is a context's global proxy.
    #[op(name = "isContext")]
    pub fn is_context(ctx: &mut Ctx, value: Value) -> bool {
        ctx.vm_is_context(&value)
    }

    /// `(code, filename, lineOffset, columnOffset)`: throw the script's SyntaxError, if any.
    #[op(coerce, name = "compileScript")]
    pub fn compile_script(
        ctx: &mut Ctx,
        code: String,
        filename: String,
        line_offset: i32,
        _column_offset: i32,
    ) -> Result<(), OpError> {
        ctx.vm_compile_script(&code, &filename, line_offset).map_err(OpError::thrown)?;
        Ok(())
    }

    /// `(globalProxy | null, code, filename, lineOffset, columnOffset, displayErrors)`: the
    /// completion value.
    #[op(coerce, name = "runScript")]
    pub fn run_script(
        ctx: &mut Ctx,
        global_proxy: Value,
        code: String,
        filename: String,
        line_offset: i32,
        column_offset: i32,
        display_errors: Value,
    ) -> Result<Value, OpError> {
        ctx.vm_run_script(
            context_arg(&global_proxy),
            &code,
            &filename,
            line_offset,
            column_offset,
            !matches!(display_errors, Value::Bool(false)),
        )
        .map_err(OpError::thrown)
    }

    /// `(globalProxy | null, code, params[], extensions[], filename, lineOffset, columnOffset)`.
    #[op(coerce, name = "compileFunction")]
    pub fn compile_function(
        ctx: &mut Ctx,
        global_proxy: Value,
        code: String,
        params: Vec<String>,
        extensions: Vec<Value>,
        filename: String,
        line_offset: i32,
        column_offset: i32,
    ) -> Result<Value, OpError> {
        ctx.vm_compile_function(
            context_arg(&global_proxy),
            &code,
            &params,
            &extensions,
            &filename,
            line_offset,
            column_offset,
        )
        .map_err(OpError::thrown)
    }
}
