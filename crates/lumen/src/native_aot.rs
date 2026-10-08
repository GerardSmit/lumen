//! JS native image ownership and call entry. The ABI is published only after
//! its metadata, helper table and runtime semantics are complete.

use crate::interpreter::{new_var_scope, Abrupt, Binding, Env, Interp};
use crate::value::{Property, Value};
use lumen_common::aot::fingerprint::{self, Layout, NativeAbi};
use lumen_common::aot::{self, Language};
use lumen_common::target::TargetSpec;
use lumen_os::native::LoadedNative;
use std::rc::Rc;
use std::sync::Arc;

pub(crate) mod classes;
pub(crate) mod coroutines;
pub(crate) mod metadata;
mod modules;
mod ops;
mod snapshot;

/// The producer may publish native blobs only when the JS runtime ABI is complete.
pub fn compiler_ready() -> bool {
    true
}

/// Stable semantic helper names used by the native compiler and ABI fingerprint.
pub fn helper_names() -> &'static [&'static str] {
    crate::native_ops::OP_NAMES
}

/// Compute the native ABI identity for a target with this engine's value and
/// frame layout. The profile may publish it only after every helper is live.
pub fn fingerprint(target: &TargetSpec) -> u64 {
    let mut helpers = vec![
        ("enter", "(NativeFrame*)->u32"),
        ("safepoint", "(NativeFrame*)->u32"),
        ("land", "(NativeFrame*,u32)->u32"),
        ("resume", "(NativeFrame*)->u32"),
        ("type_guard", "(NativeFrame*,u32,u32)->u32"),
        ("num_binary", "(NativeFrame*,u32,u32,u32)->u32"),
        ("shape_guard", "(NativeFrame*,u32,u32)->u32"),
        ("shape_get_prop", "(NativeFrame*,u32,u32,u32,u32)->u32"),
    ];
    helpers.extend(
        helper_names()
            .iter()
            .map(|name| (*name, "(NativeFrame*,u32,u32,u32,u32,u32,u32)->u32")),
    );
    fingerprint::calculate(&NativeAbi {
        arch: target.arch,
        abi: target.abi,
        features: target.features,
        pointer_width: target.pointer_width,
        page_size: target.page_size,
        builtin_modules_hash: target.builtin_modules_hash,
        value_layout: Layout {
            size: core::mem::size_of::<Value>() as u32,
            align: core::mem::align_of::<Value>() as u32,
            fields: &[("tag", 0), ("payload", 8)],
        },
        frame_layout: Layout {
            size: core::mem::size_of::<NativeFrame>() as u32,
            align: core::mem::align_of::<NativeFrame>() as u32,
            fields: &[
                ("resume", core::mem::offset_of!(NativeFrame, resume) as u32),
                ("depth", core::mem::offset_of!(NativeFrame, depth) as u32),
                ("slots", core::mem::offset_of!(NativeFrame, slots) as u32),
                ("stack", core::mem::offset_of!(NativeFrame, stack) as u32),
                ("env", core::mem::offset_of!(NativeFrame, env) as u32),
            ],
        },
        helpers: &helpers,
        statics: &[],
        data_version: aot::native_data::VERSION,
        code_version: aot::NATIVE_CODE_VERSION,
    })
}

/// Validate AST-free metadata before allocating executable memory.
pub fn validate_payload(bytes: &[u8], function_count: usize) -> Result<(), String> {
    let metadata = metadata::decode(bytes, function_count).map_err(str::to_owned)?;
    if let Some(bytes) = metadata.snapshot.as_deref() {
        snapshot::decode(bytes, function_count)?;
    }
    Ok(())
}

/// Check an authenticated upload against this realm before replacing a live
/// application. This performs no executable mapping or JavaScript execution.
#[cfg(all(feature = "embed", feature = "aot-native"))]
pub(crate) fn validate_install(
    interp: &Interp,
    bytes: &[u8],
    additional_modules: &[&str],
) -> Result<(), String> {
    if !compiler_ready() {
        return Err("native AOT helper ABI is not complete".into());
    }
    let container = aot::NativeContainer::parse(bytes).map_err(str::to_owned)?;
    if container.language != Language::JavaScript {
        return Err("native image is not JavaScript".into());
    }
    let target = crate::target::host();
    container.mapping_len(&target).map_err(str::to_owned)?;
    let data = container
        .sections
        .iter()
        .find(|section| section.kind == aot::SEC_NATIVE_DATA)
        .ok_or("native JS data section missing")?
        .data;
    let code_len = container
        .sections
        .iter()
        .find(|section| section.kind == aot::SEC_NATIVE_CODE)
        .ok_or("native JS code section missing")?
        .data
        .len();
    let shared = aot::native_data::NativeData::parse(data, code_len)?;
    let metadata =
        metadata::decode(shared.payload, container.functions.len()).map_err(str::to_owned)?;
    if let Some(snapshot) = metadata.snapshot.as_deref() {
        snapshot::decode(snapshot, container.functions.len())?;
    }
    for reloc in &container.got_relocs {
        match reloc.kind {
            aot::got::Kind::Helper if helper_address(reloc.index).is_none() => {
                return Err(format!("unknown native helper {}", reloc.index));
            }
            aot::got::Kind::Helper | aot::got::Kind::Function | aot::got::Kind::NativeImport => {}
            _ => {
                return Err(format!(
                    "unsupported native GOT relocation {:?}",
                    reloc.kind
                ))
            }
        }
    }
    for import in &container.required_imports {
        let valid = if import.name.is_empty() {
            interp.native_module_names.contains(import.module)
                || additional_modules.contains(&import.module)
        } else {
            interp
                .native_bindings
                .get(&(import.module.to_owned(), import.name.to_owned()))
                .is_some_and(|&(signature, address)| {
                    signature == import.signature_hash && address != 0
                })
        };
        if !valid {
            return Err(format!(
                "missing native import {}::{}",
                import.module, import.name
            ));
        }
    }
    Ok(())
}

pub(crate) struct NativeProgram {
    #[cfg(feature = "parallel")]
    pub(crate) trusted_glue: bool,
    pub(crate) image: Arc<LoadedNative>,
    pub(crate) metadata: metadata::Metadata,
    /// The authenticated, immutable source for another realm's independent GOT.
    #[cfg(feature = "parallel")]
    pub(crate) bytes: Arc<[u8]>,
}

/// Module state is owned by each realm; a program never retains the scopes of
/// functions that keep that same program alive.
pub(crate) struct UnitState {
    pub(crate) env: Env,
    pub(crate) namespace: Value,
    pub(crate) evaluation: Option<Value>,
    pub(crate) cycle_root: u32,
    pub(crate) visiting: bool,
    pub(crate) waiting: usize,
    pub(crate) evaluating: bool,
    pub(crate) evaluated: bool,
}

/// The first two fields are the only fields read directly by generated code.
#[repr(C)]
pub(crate) struct NativeFrame {
    pub resume: u32,
    pub depth: u32,
    interp: *mut Interp,
    program: Rc<NativeProgram>,
    function: u32,
    slots: Vec<Value>,
    stack: Vec<Value>,
    args: Vec<Value>,
    env: Env,
    pending_env: Option<Env>,
    this_val: Value,
    exception: Value,
    result: Value,
    pending_resume: Option<Value>,
    parked: coroutines::Parked,
    tail_allowed: bool,
}

const _: () = {
    assert!(core::mem::offset_of!(NativeFrame, resume) == 0);
    assert!(core::mem::offset_of!(NativeFrame, depth) == 4);
};

const STATUS_OK: u32 = 0;
const STATUS_THROW: u32 = 1;
const STATUS_SUSPEND: u32 = 2;

fn helper_address(id: u32) -> Option<usize> {
    match id {
        crate::native_ops::ENTER => Some(enter as *const () as usize),
        crate::native_ops::SAFEPOINT => Some(safepoint as *const () as usize),
        crate::native_ops::LAND => Some(land as *const () as usize),
        crate::native_ops::RESUME => Some(resume as *const () as usize),
        crate::native_ops::TYPE_GUARD => Some(type_guard as *const () as usize),
        crate::native_ops::NUM_BINARY => Some(num_binary as *const () as usize),
        _ => ops::address(id),
    }
}

/// A profile hint is checked against live boxed values before a specialized path.
pub(crate) unsafe extern "C" fn type_guard(
    frame: *mut NativeFrame,
    left_kind: u32,
    right_kind: u32,
) -> u32 {
    let frame = unsafe { &*frame };
    let Some(pair) = frame.stack.get(frame.stack.len().saturating_sub(2)..) else {
        return 0;
    };
    u32::from(
        pair.len() == 2
            && crate::native_ops::value_kind(&pair[0]) == left_kind
            && crate::native_ops::value_kind(&pair[1]) == right_kind,
    )
}

/// Numeric operation chosen by a checked profile guard. A stale profile still
/// takes the ordinary boxed semantics through `Interp::binary`.
pub(crate) unsafe extern "C" fn num_binary(
    frame: *mut NativeFrame,
    base_id: u32,
    depth: u32,
    next_resume: u32,
) -> u32 {
    let frame = unsafe { &mut *frame };
    frame.depth = depth;
    frame.resume = next_resume;
    let operator = match base_id {
        4153 => "+",
        4154 => "-",
        4155 => "*",
        4156 => "/",
        4157 => "%",
        4164 => "<",
        4165 => ">",
        4166 => "<=",
        4167 => ">=",
        4168 => "==",
        4169 => "!=",
        4170 => "===",
        4171 => "!==",
        _ => {
            let error = unsafe { &mut *frame.interp }.throw("Error", "invalid numeric helper id");
            return failed(frame, error);
        }
    };
    let Some(right) = frame.stack.pop() else {
        let error = unsafe { &mut *frame.interp }.throw("Error", "native operand stack underflow");
        return failed(frame, error);
    };
    let Some(left) = frame.stack.pop() else {
        let error = unsafe { &mut *frame.interp }.throw("Error", "native operand stack underflow");
        return failed(frame, error);
    };
    match unsafe { &mut *frame.interp }.binary(operator, left, right) {
        Ok(value) => {
            frame.stack.push(value);
            frame.depth = frame.stack.len() as u32;
            STATUS_OK
        }
        Err(error) => failed(frame, error),
    }
}

fn failed(frame: &mut NativeFrame, abrupt: Abrupt) -> u32 {
    frame.exception = match abrupt {
        Abrupt::Throw(value) => value,
        _ => Value::Undefined,
    };
    STATUS_THROW
}

/// Native prologue: enforce stack headroom and the first GC/interrupt poll.
pub(crate) unsafe extern "C" fn enter(frame: *mut NativeFrame) -> u32 {
    let frame = unsafe { &mut *frame };
    let interp = unsafe { &mut *frame.interp };
    if interp.stack_guard() {
        return failed(
            frame,
            interp.throw("RangeError", "Maximum call stack size exceeded"),
        );
    }
    match interp.gc_check() {
        Ok(()) => STATUS_OK,
        Err(error) => failed(frame, error),
    }
}

/// Poll at a loop backedge or another generated safepoint.
pub(crate) unsafe extern "C" fn safepoint(frame: *mut NativeFrame) -> u32 {
    let frame = unsafe { &mut *frame };
    match unsafe { &mut *frame.interp }.gc_check() {
        Ok(()) => STATUS_OK,
        Err(error) => failed(frame, error),
    }
}

/// Resume a suspended function, injecting a rejected await into its handler.
pub(crate) unsafe extern "C" fn resume(frame: *mut NativeFrame) -> u32 {
    let frame = unsafe { &mut *frame };
    if let Some(error) = frame.pending_resume.take() {
        frame.exception = error;
        STATUS_THROW
    } else {
        STATUS_OK
    }
}

/// Enter a catch/finally landing pad with its saved operand depth.
pub(crate) unsafe extern "C" fn land(frame: *mut NativeFrame, saved_depth: u32) -> u32 {
    let frame = unsafe { &mut *frame };
    let depth = (saved_depth as usize).min(frame.stack.len());
    frame.stack.truncate(depth);
    frame
        .stack
        .push(std::mem::replace(&mut frame.exception, Value::Undefined));
    frame.depth = u32::try_from(depth + 1).unwrap_or(u32::MAX);
    STATUS_OK
}

pub(crate) fn call(
    interp: &mut Interp,
    program: &Rc<NativeProgram>,
    function_index: u32,
    closure: &Env,
    this: Value,
    args: &[Value],
) -> Result<Value, Abrupt> {
    let Some(function) = program.metadata.functions.get(function_index as usize) else {
        return Err(interp.throw("TypeError", "native function metadata is missing"));
    };
    if function.flags & (4 | 8) != 0 {
        return coroutines::call(interp, program, function_index, closure, this, args);
    }
    let mut frame = prepare_frame(interp, program, function_index, closure, this, args)?;
    let status = execute_frame(&mut frame);
    match status {
        STATUS_OK => Ok(std::mem::replace(&mut frame.result, Value::Undefined)),
        STATUS_THROW => Err(Abrupt::Throw(std::mem::replace(
            &mut frame.exception,
            Value::Undefined,
        ))),
        STATUS_SUSPEND => Err(interp.throw("Error", "unexpected native coroutine suspension")),
        _ => Err(interp.throw("Error", "invalid native function status")),
    }
}

pub(crate) fn prepare_frame(
    interp: &mut Interp,
    program: &Rc<NativeProgram>,
    function_index: u32,
    closure: &Env,
    this: Value,
    args: &[Value],
) -> Result<Box<NativeFrame>, Abrupt> {
    if program.image.entry(function_index as usize).is_none() {
        return Err(interp.throw("TypeError", "native function index is out of range"));
    }
    let Some(function) = program.metadata.functions.get(function_index as usize) else {
        return Err(interp.throw("TypeError", "native function metadata is missing"));
    };
    let cjs_top = program
        .metadata
        .units
        .iter()
        .any(|unit| unit.kind == 2 && unit.top_function == function_index);
    let wrapper_args = cjs_top.then(|| {
        let wrapper = closure.borrow();
        ["exports", "require", "module", "__filename", "__dirname"]
            .into_iter()
            .map(|name| {
                wrapper
                    .vars
                    .get(name)
                    .map_or(Value::Undefined, |binding| binding.value.clone())
            })
            .collect::<Vec<_>>()
    });
    let args = wrapper_args.as_deref().unwrap_or(args);
    let uses_this = function.frame_flags & 3 != 0;
    let this = if !uses_this {
        Value::Undefined
    } else if function.flags & 2 != 0 || interp.constructing {
        this
    } else {
        match this {
            Value::Undefined | Value::Null => interp.global_this_value(),
            object @ Value::Obj(_) => object,
            primitive => crate::builtins::box_primitive_pub(interp, primitive),
        }
    };
    let env = if function.flags & 64 != 0
        || (function.captures.is_empty() && function.frame_flags & 2 == 0)
    {
        closure.clone()
    } else {
        let env = new_var_scope(Some(closure.clone()));
        {
            let mut scope = env.borrow_mut();
            for capture in &function.captures {
                let (name, binding) = match capture {
                    metadata::Capture::Param(slot, name) => (
                        name,
                        Binding::data(
                            args.get(*slot as usize)
                                .cloned()
                                .unwrap_or(Value::Undefined),
                            true,
                            true,
                        ),
                    ),
                    metadata::Capture::Var(name) => {
                        if scope.vars.contains_key(name) {
                            continue;
                        }
                        (name, Binding::data(Value::Undefined, true, true))
                    }
                    metadata::Capture::Function(_, name) => {
                        (name, Binding::data(Value::Undefined, true, true))
                    }
                    metadata::Capture::Lexical(name, immutable) => {
                        (name, Binding::data(Value::Undefined, !immutable, false))
                    }
                };
                scope.vars.insert(name.as_str(), binding);
            }
            if function.frame_flags & 2 != 0 {
                scope.vars.insert(
                    "this",
                    Binding::data(this.clone(), false, function.frame_flags & 4 == 0),
                );
            }
        }
        for capture in &function.captures {
            if let metadata::Capture::Function(child, name) = capture {
                let index = function.children[*child as usize];
                let value = interp.make_native_function(program.clone(), index, env.clone());
                env.borrow_mut()
                    .vars
                    .insert(name.as_str(), Binding::data(value, true, true));
            }
        }
        env
    };
    let mut slots = args
        .iter()
        .take(function.params as usize)
        .cloned()
        .collect::<Vec<_>>();
    slots.resize(function.slots as usize, Value::Undefined);
    if let Some(slot) = function.arguments_slot {
        if function.virt_base.is_none() {
            slots[slot as usize] = Value::Obj(interp.make_compiled_arguments_object(args, &env));
        }
    }
    if let Some(slot) = function.rest_slot {
        if function.virt_base.is_none() {
            slots[slot as usize] =
                interp.make_array_iter(args.iter().skip(function.params as usize).cloned());
        }
    }
    for &slot in &function.var_resets {
        slots[slot as usize] = Value::Undefined;
    }
    if cjs_top {
        let wrapper = env.borrow();
        for name in ["module", "exports", "require", "__filename", "__dirname"] {
            if let Some(slot) = function.slot_names.iter().position(|slot| slot == name) {
                if let Some(binding) = wrapper.vars.get(name) {
                    slots[slot] = binding.value.clone();
                }
            }
        }
    }
    Ok(Box::new(NativeFrame {
        resume: 0,
        depth: 0,
        interp: interp as *mut Interp,
        program: program.clone(),
        function: function_index,
        slots,
        stack: Vec::with_capacity(function.max_stack as usize),
        args: args.to_vec(),
        env,
        pending_env: None,
        this_val: this,
        exception: Value::Undefined,
        result: Value::Undefined,
        pending_resume: None,
        parked: coroutines::Parked::Start,
        tail_allowed: std::mem::replace(&mut interp.native_tail_call, false),
    }))
}

pub(crate) fn execute_frame(frame: &mut NativeFrame) -> u32 {
    let function = &frame.program.metadata.functions[frame.function as usize];
    let Some(entry) = frame.program.image.entry(frame.function as usize) else {
        let interp = unsafe { &mut *frame.interp };
        return failed(
            frame,
            interp.throw("Error", "native function entry is missing"),
        );
    };
    let (saved_strict, saved_tco) = {
        let interp = unsafe { &mut *frame.interp };
        (
            std::mem::replace(&mut interp.strict, function.flags & 2 != 0),
            std::mem::replace(&mut interp.tco_ok, function.flags & 2 != 0),
        )
    };
    let saved_require = {
        let interp = unsafe { &mut *frame.interp };
        let program = frame.program.clone();
        let require = interp.new_native_fn(
            "require",
            1,
            Rc::new(move |interp, _, args| {
                modules::require(
                    interp,
                    &program,
                    None,
                    args.first().cloned().unwrap_or(Value::Undefined),
                )
                .map_err(crate::interpreter::abrupt_value)
            }),
        );
        let saved = interp
            .global
            .borrow()
            .props
            .get("require")
            .map(|property| (*property).clone());
        interp
            .global
            .borrow_mut()
            .props
            .insert("require", Property::plain(require));
        saved
    };
    let entry: unsafe extern "C" fn(*mut NativeFrame) -> u32 =
        unsafe { std::mem::transmute(entry) };
    let status = crate::native_ops::closed_world(|| unsafe { entry(frame) });
    let interp = unsafe { &mut *frame.interp };
    if let Some(require) = saved_require {
        interp.global.borrow_mut().props.insert("require", require);
    } else {
        interp.global.borrow_mut().props.remove("require");
    }
    interp.strict = saved_strict;
    interp.tco_ok = saved_tco;
    status
}

pub(crate) fn load_engine(
    interp: &mut Interp,
    bytes: Arc<[u8]>,
    signature: Option<[u8; 64]>,
    allowed_keys: &[[u8; 32]],
    allow_unsigned: bool,
) -> Result<Value, String> {
    let program = load_program(interp, bytes, signature, allowed_keys, allow_unsigned)?;
    execute_program(interp, program)
}

/// Execute an image whose text and GOT were placed by the system linker.
/// The caller keeps these ranges alive for every realm that uses the image.
#[cfg(all(feature = "embed", feature = "aot-native"))]
pub(crate) unsafe fn load_linked_engine(
    interp: &mut Interp,
    bytes: Arc<[u8]>,
    code: *const u8,
    code_len: usize,
    got: *mut usize,
    got_len: usize,
) -> Result<Value, String> {
    let program = load_program_impl(
        interp,
        bytes,
        None,
        &[],
        true,
        Some((code, code_len, got, got_len)),
        false,
    )?;
    execute_program(interp, program)
}

/// Load compiler-produced, executable-embedded glue at the portable CPU and
/// built-in catalog baseline. This is deliberately restricted to static bytes.
#[cfg(all(feature = "embed", feature = "aot-native"))]
pub(crate) fn load_static_glue_engine(
    interp: &mut Interp,
    bytes: &'static [u8],
) -> Result<Value, String> {
    let program = load_program_impl(interp, Arc::from(bytes), None, &[], true, None, true)?;
    execute_program(interp, program)
}

fn execute_program(interp: &mut Interp, program: Rc<NativeProgram>) -> Result<Value, String> {
    if let Some(snapshot_bytes) = program.metadata.snapshot.as_deref() {
        let graph = snapshot::decode(snapshot_bytes, program.image.entry_count())?;
        let restored = snapshot::restore(interp, &program, &graph)?;
        modules::install_snapshot_states(interp, &program, &restored)?;
        let function = program
            .metadata
            .snapshot_entry
            .ok_or("snapshot has no entry function")?;
        let env = match restored.functions.get(&function).map(Vec::as_slice) {
            Some([env]) => env.clone(),
            Some(_) => return Err("snapshot entry has multiple closure environments".into()),
            None => return Err("snapshot entry is not reachable in the initialized heap".into()),
        };
        let result = call(interp, &program, function, &env, Value::Undefined, &[]);
        return result.map_err(|error| {
            interp
                .to_string(&crate::interpreter::abrupt_value(error))
                .map(|message| message.to_string())
                .unwrap_or_else(|_| "native snapshot entry failed".into())
        });
    }
    let Some(entry) = program.metadata.entry_unit else {
        return Err("native JS image has no entry unit".into());
    };
    modules::run_unit(interp, &program, entry).map_err(|error| {
        interp
            .to_string(&crate::interpreter::abrupt_value(error))
            .map(|message| message.to_string())
            .unwrap_or_else(|_| "native entry failed".into())
    })
}

pub(crate) fn import_call(
    interp: &mut Interp,
    program: &Rc<NativeProgram>,
    current_function: u32,
    specifier: Value,
    options: Option<Value>,
    phase: u32,
) -> Result<Value, Abrupt> {
    modules::import_call(interp, program, current_function, specifier, options, phase)
}

pub(crate) fn load_program(
    interp: &mut Interp,
    bytes: Arc<[u8]>,
    signature: Option<[u8; 64]>,
    allowed_keys: &[[u8; 32]],
    allow_unsigned: bool,
) -> Result<Rc<NativeProgram>, String> {
    load_program_impl(
        interp,
        bytes,
        signature,
        allowed_keys,
        allow_unsigned,
        None,
        false,
    )
}

fn load_program_impl(
    interp: &mut Interp,
    bytes: Arc<[u8]>,
    signature: Option<[u8; 64]>,
    allowed_keys: &[[u8; 32]],
    allow_unsigned: bool,
    linked: Option<(*const u8, usize, *mut usize, usize)>,
    static_glue: bool,
) -> Result<Rc<NativeProgram>, String> {
    if !compiler_ready() {
        return Err("native AOT helper ABI is not complete".into());
    }
    let container = aot::NativeContainer::parse(&bytes).map_err(str::to_owned)?;
    if container.language != Language::JavaScript {
        return Err("native image is not JavaScript".into());
    }
    let data = container
        .sections
        .iter()
        .find(|section| section.kind == aot::SEC_NATIVE_DATA)
        .ok_or("native JS data section missing")?
        .data;
    let shared = aot::native_data::NativeData::parse(
        data,
        container
            .sections
            .iter()
            .find(|section| section.kind == aot::SEC_NATIVE_CODE)
            .ok_or("native JS code section missing")?
            .data
            .len(),
    )?;
    let metadata = metadata::decode(shared.payload, container.functions.len())?;
    if let Some(bytes) = metadata.snapshot.as_deref() {
        snapshot::decode(bytes, container.functions.len())?;
    }
    let mut target = crate::target::host();
    if target.native_fp == 0 {
        return Err("native AOT runtime is disabled".into());
    }
    if static_glue {
        if !container.required_imports.is_empty() {
            return Err("internal native glue has external imports".into());
        }
        target.features = 0;
        target.code_placement = lumen_common::target::Placement::Ram;
        target.profile = lumen_common::target::Profile::Aot;
        target.builtin_modules_hash = [
            crate::target::EMPTY_BUILTIN_MODULES_HASH,
            crate::target::PARALLEL_BUILTIN_MODULES_HASH,
            lumen_common::aot::builtin_catalog::hash(
                lumen_common::aot::builtin_catalog::Features {
                    parallel: true,
                    bitnest_process: true,
                    ..Default::default()
                },
            ),
        ]
        .into_iter()
        .find(|hash| {
            let mut candidate = target;
            candidate.builtin_modules_hash = *hash;
            candidate.native_fp = fingerprint(&candidate);
            candidate.native_fp == container.native_fp
        })
        .ok_or("internal native glue baseline fingerprint mismatch")?;
        target.native_fp = fingerprint(&target);
    }
    if signature.is_none() && !allow_unsigned {
        return Err("unsigned native image is not allowed".into());
    }
    let signed = signature;
    let import_table_known = target.builtin_modules_hash != 0;
    #[cfg(any(windows, unix))]
    {
        let _ = lumen_os::native::install_host_backend();
    }
    let verify = |blob: &[u8]| match signed {
        Some(signature) => {
            lumen_crypto::native_signature::verify(blob, &signature, allowed_keys).map_err(str::to_owned)
        }
        None if allow_unsigned => Ok(()),
        None => Err("unsigned native image is not allowed".into()),
    };
    let resolve_import = |module: &str, name: &str, signature: u64| {
        if !import_table_known {
            return None;
        }
        if name.is_empty() {
            return interp.native_module_names.contains(module).then_some(1);
        }
        interp
            .native_bindings
            .get(&(module.to_owned(), name.to_owned()))
            .and_then(|&(registered, address)| (registered == signature).then_some(address))
    };
    let resolve = |kind: aot::got::Kind, index: u32, _base: *const u8, _data: &[u8]| match kind {
        aot::got::Kind::Helper => helper_address(index),
        _ => None,
    };
    let image = if let Some((code, code_len, got, got_len)) = linked {
        Arc::new(unsafe {
            lumen_os::native::load_linked(
                &bytes,
                &target,
                code,
                code_len,
                got,
                got_len,
                verify,
                resolve_import,
                resolve,
            )?
        })
    } else {
        lumen_os::native::load_shared(&bytes, &target, verify, resolve_import, resolve)?
    };
    Ok(Rc::new(NativeProgram {
        #[cfg(feature = "parallel")]
        trusted_glue: static_glue,
        image,
        metadata,
        #[cfg(feature = "parallel")]
        bytes,
    }))
}

/// Re-map the immutable bytes retained by an already-authenticated native
/// program when creating another realm. This is not an install entry point.
#[cfg(feature = "parallel")]
pub(crate) fn load_authenticated_program(
    interp: &mut Interp,
    bytes: Arc<[u8]>,
    trusted_glue: bool,
) -> Result<Rc<NativeProgram>, String> {
    load_program_impl(interp, bytes, None, &[], true, None, trusted_glue)
}
