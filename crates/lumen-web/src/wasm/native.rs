//! The native tier: an instance's functions compiled to x86-64 by `lumen-codegen` when it is
//! instantiated.
//!
//! Every function index gets native code: a translated body, or — for imports and functions the
//! translator rejects — a bridge stub ([`translate::bridge_stub`]) that calls back into the store,
//! which runs the host function or the interpreter. So one untranslatable function never keeps the
//! rest of the instance interpreted, and native code calls native code directly.
//!
//! Both tiers share state: globals live in cells both address, and the memory's base and length
//! are refreshed in `VmCtx` on entry and after every helper call (memory can only move there).
//!
//! `LUMEN_WASM_JIT=0` disables the tier; `LUMEN_WASM_JIT_LOG=1` reports what compiled.

use super::exec::{FuncEntity, Host, Store, Val};
use super::parse::FuncType;
use super::translate::{self, *};
use lumen_codegen::jitmem::ExecMemory;
use lumen_codegen::{opt, x64, Signature};
use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};

#[repr(C)]
pub struct VmCtx {
    mem_base: *mut u8,
    mem_len: u64,
    globals: *const *mut u64,
    entry_sp: u64,
    stack_limit: u64,
    pending: u64,
    _pad: [u64; 2],
    args: [u64; VMCTX_ARGS_LEN],
    // Runtime-only fields (native code never reads these).
    store: *mut Store,
    /// `*mut &mut dyn Host` for the innermost active call.
    host: *mut (),
    inst: usize,
    mem_addr: Option<usize>,
    error: Option<String>,
    depth: u32,
}

const _: () = {
    assert!(std::mem::offset_of!(VmCtx, mem_base) == VMCTX_MEM_BASE as usize);
    assert!(std::mem::offset_of!(VmCtx, mem_len) == VMCTX_MEM_LEN as usize);
    assert!(std::mem::offset_of!(VmCtx, globals) == VMCTX_GLOBALS as usize);
    assert!(std::mem::offset_of!(VmCtx, entry_sp) == VMCTX_ENTRY_SP as usize);
    assert!(std::mem::offset_of!(VmCtx, stack_limit) == VMCTX_STACK_LIMIT as usize);
    assert!(std::mem::offset_of!(VmCtx, pending) == VMCTX_PENDING as usize);
    assert!(std::mem::offset_of!(VmCtx, args) == VMCTX_ARGS as usize);
};

/// What `call_indirect` receives from [`HELPER_RESOLVE_INDIRECT`].
#[repr(C)]
struct FuncSlot {
    code: u64,
    vmctx: u64,
}

pub struct NativeInstance {
    vm: Box<VmCtx>,
    _code: ExecMemory,
    /// Per function index: entry address, whether it is a real body (not a bridge), its type.
    addrs: Vec<u64>,
    is_body: Vec<bool>,
    sigs: Vec<Signature>,
    types: Vec<FuncType>,
    slots: Box<[FuncSlot]>,
    /// Per type index: a bridge taking its callee from `VmCtx::pending`.
    bridge_slots: Box<[Option<FuncSlot>]>,
    _globals: Box<[*mut u64]>,
    by_addr: HashMap<usize, usize>,
    tramps: HashMap<Signature, ExecMemory>,
}

pub fn enabled() -> bool {
    cfg!(target_arch = "x86_64") && std::env::var("LUMEN_WASM_JIT").map_or(true, |v| v != "0")
}

fn log() -> bool {
    std::env::var("LUMEN_WASM_JIT_LOG").is_ok_and(|v| v != "0")
}

fn config() -> x64::Config {
    x64::Config::host(Some(x64::TrapConfig {
        entry_sp_offset: VMCTX_ENTRY_SP,
        stack_limit: Some((VMCTX_STACK_LIMIT, trap::STACK_OVERFLOW)),
    }))
}

/// Compile instance `inst_idx`, or `None` to leave it to the interpreter.
pub fn build(store: &mut Store, inst_idx: usize) -> Option<Box<NativeInstance>> {
    if !enabled() {
        return None;
    }
    let start = std::time::Instant::now();
    let inst = store.instances[inst_idx].clone();
    let m = &inst.module;
    let cfg = config();
    let nfuncs = inst.func_addrs.len();
    let mut compiled = Vec::with_capacity(nfuncs + m.types.len());
    let mut is_body = vec![false; nfuncs];
    let mut types = Vec::with_capacity(nfuncs);
    let mut sigs = Vec::with_capacity(nfuncs);
    let mut rejected = Vec::new();
    for f in 0..nfuncs {
        let ty = translate::func_type(m, f as u32).ok()?.clone();
        let body = if f as u32 >= m.imported_func_count {
            translate::translate(m, f as u32)
                .and_then(|mut func| {
                    opt::optimize(&mut func);
                    x64::compile(&func, &cfg)
                })
                .map_err(|e| rejected.push((f, e)))
                .ok()
        } else {
            None
        };
        let code = match body {
            Some(c) => {
                is_body[f] = true;
                c
            }
            None => {
                let stub = translate::bridge_stub(&ty, Some(inst.func_addrs[f] as u64)).ok()?;
                x64::compile(&stub, &cfg).ok()?
            }
        };
        compiled.push(code);
        sigs.push(native_signature(&ty).ok()?);
        types.push(ty);
    }
    let mut generic = vec![None; m.types.len()];
    for (t, ty) in m.types.iter().enumerate() {
        let c = translate::bridge_stub(ty, None).and_then(|f| x64::compile(&f, &cfg));
        if let Ok(c) = c {
            generic[t] = Some(compiled.len());
            compiled.push(c);
        }
    }
    let (code, addrs) = x64::load(&compiled, |id, addrs| {
        Some(match id {
            HELPER_MEMORY_GROW => h_grow as *const () as u64,
            HELPER_MEMORY_COPY => h_copy as *const () as u64,
            HELPER_MEMORY_FILL => h_fill as *const () as u64,
            HELPER_RESOLVE_INDIRECT => h_resolve as *const () as u64,
            HELPER_CALL_SLOW => h_call_slow as *const () as u64,
            id if (id as usize) < nfuncs => addrs[id as usize],
            _ => return None,
        })
    })
    .ok()?;

    let globals: Box<[*mut u64]> = inst
        .global_addrs
        .iter()
        .map(|&a| store.globals[a].cell())
        .collect();
    let mut vm = Box::new(VmCtx {
        mem_base: std::ptr::null_mut(),
        mem_len: 0,
        globals: globals.as_ptr(),
        entry_sp: 0,
        stack_limit: 0,
        pending: 0,
        _pad: [0; 2],
        args: [0; VMCTX_ARGS_LEN],
        store: std::ptr::null_mut(),
        host: std::ptr::null_mut(),
        inst: inst_idx,
        mem_addr: inst.mem_addrs.first().copied(),
        error: None,
        depth: 0,
    });
    let vmp = &mut *vm as *mut VmCtx as u64;
    let slots = addrs[..nfuncs]
        .iter()
        .map(|&code| FuncSlot { code, vmctx: vmp })
        .collect();
    let bridge_slots = generic
        .iter()
        .map(|g| g.map(|i| FuncSlot {
            code: addrs[i],
            vmctx: vmp,
        }))
        .collect();
    if log() {
        let n = is_body.iter().filter(|&&b| b).count();
        eprintln!(
            "wasm jit: instance {inst_idx}: {n}/{} bodies native, {} bytes, {:?}",
            nfuncs - m.imported_func_count as usize,
            code.len(),
            start.elapsed()
        );
        for (f, e) in &rejected {
            eprintln!("wasm jit:   f{f} interpreted: {e}");
        }
    }
    Some(Box::new(NativeInstance {
        vm,
        _code: code,
        addrs,
        is_body,
        sigs,
        types,
        slots,
        bridge_slots,
        _globals: globals,
        by_addr: inst.func_addrs.iter().enumerate().map(|(i, &a)| (a, i)).collect(),
        tramps: HashMap::new(),
    }))
}

type Tramp = unsafe extern "C" fn(*mut VmCtx, u64, *mut u64) -> u32;

/// Run store function `func_addr` of instance `inst_idx` natively, or `None` when it has no
/// native body (the caller interprets it).
pub fn invoke(
    store: &mut Store,
    inst_idx: usize,
    func_addr: usize,
    args: &[Val],
    host: &mut dyn Host,
) -> Option<Result<Vec<Val>, String>> {
    let ni: *mut NativeInstance = &mut **store.native.get_mut(inst_idx)?.as_mut()?;
    let store: *mut Store = store;
    // SAFETY: the NativeInstance is boxed and outlives the call; the store is only reached
    // through `store` (by helpers) until this returns.
    let ni = unsafe { &mut *ni };
    let f = *ni.by_addr.get(&func_addr)?;
    if !ni.is_body[f] {
        return None;
    }
    let tramp = match ni.tramps.get(&ni.sigs[f]) {
        Some(t) => t.as_ptr(),
        None => {
            let code = x64::trampoline(&ni.sigs[f], &config()).ok()?;
            let mem = ExecMemory::new(&code).ok()?;
            let p = mem.as_ptr();
            ni.tramps.insert(ni.sigs[f].clone(), mem);
            p
        }
    };
    let vm: *mut VmCtx = &mut *ni.vm;
    let mut host: &mut dyn Host = host;
    let mut slots = Vec::with_capacity(1 + args.len());
    slots.push(vm as u64);
    slots.extend(args.iter().map(|v| v.to_bits()));
    let rc = unsafe {
        let v = &mut *vm;
        let saved = (v.store, v.host);
        v.store = store;
        v.host = &mut host as *mut &mut dyn Host as *mut ();
        if v.depth == 0 {
            v.stack_limit = stack_limit();
        }
        v.depth += 1;
        sync_mem(v);
        let entry: Tramp = std::mem::transmute(tramp);
        let rc = entry(vm, ni.addrs[f], slots.as_mut_ptr());
        let v = &mut *vm;
        v.depth -= 1;
        (v.store, v.host) = saved;
        rc
    };
    let ty = &ni.types[f];
    Some(if rc == 0 {
        Ok(ty
            .results
            .iter()
            .zip(&slots)
            .map(|(&t, &b)| Val::from_bits(t, b))
            .collect())
    } else if rc - 1 == trap::HELPER {
        Err(unsafe { (*vm).error.take() }.unwrap_or_else(|| "wasm: trap".into()))
    } else {
        Err(trap::message(rc - 1).into())
    })
}

// ----- helpers called from native code ----------------------------------------------------------

unsafe fn sync_mem(vm: &mut VmCtx) {
    match vm.mem_addr {
        Some(a) => {
            let m = &mut (&mut *vm.store).memories[a].bytes;
            vm.mem_base = m.as_mut_ptr();
            vm.mem_len = m.len() as u64;
        }
        None => {
            vm.mem_base = std::ptr::null_mut();
            vm.mem_len = 0;
        }
    }
}

/// Run `f` so a panic cannot unwind through native frames: it becomes a helper trap.
fn guarded<R>(vm: *mut VmCtx, fail: R, f: impl FnOnce(&mut VmCtx, &mut Store) -> R) -> R {
    let r = catch_unwind(AssertUnwindSafe(|| unsafe {
        let vm = &mut *vm;
        let store = &mut *vm.store;
        f(vm, store)
    }));
    r.unwrap_or_else(|_| {
        unsafe { (*vm).error = Some("wasm: internal error in a runtime helper".into()) };
        fail
    })
}

extern "C" fn h_grow(vm: *mut VmCtx, delta: i32) -> i32 {
    guarded(vm, -1, |vm, store| {
        let Some(a) = vm.mem_addr else { return -1 };
        let r = store.mem_grow(a, delta);
        unsafe { sync_mem(vm) };
        r
    })
}

fn copy_fill(vm: *mut VmCtx, d: i32, s: i32, n: i32, fill: bool) -> i32 {
    guarded(vm, 1, |vm, store| {
        let Some(a) = vm.mem_addr else { return 1 };
        let m = &mut store.memories[a].bytes;
        let (d, s, n) = (d as u32 as usize, s as u32 as usize, n as u32 as usize);
        if d + n > m.len() || (!fill && s + n > m.len()) {
            return 1;
        }
        if fill {
            m[d..d + n].fill(s as u8);
        } else {
            m.copy_within(s..s + n, d);
        }
        0
    })
}

extern "C" fn h_copy(vm: *mut VmCtx, d: i32, s: i32, n: i32) -> i32 {
    copy_fill(vm, d, s, n, false)
}

extern "C" fn h_fill(vm: *mut VmCtx, d: i32, v: i32, n: i32) -> i32 {
    copy_fill(vm, d, v, n, true)
}

extern "C" fn h_resolve(vm: *mut VmCtx, type_idx: i32, table: i32, elem: i32) -> i64 {
    guarded(vm, trap::HELPER as i64, |vm, store| {
        let inst = &store.instances[vm.inst];
        let Some(&ta) = inst.table_addrs.get(table as usize) else {
            return trap::TABLE_OOB as i64;
        };
        let Some(e) = store.tables[ta].elems.get(elem as u32 as usize) else {
            return trap::TABLE_OOB as i64;
        };
        let Some(fa) = *e else {
            return trap::NULL_ELEMENT as i64;
        };
        let expected = &inst.module.types[type_idx as usize];
        let (actual, target_inst) = match &store.funcs[fa] {
            FuncEntity::Wasm { compiled, instance } => (&compiled.ty, Some(*instance)),
            FuncEntity::Host { ty, .. } => (ty, None),
        };
        if actual.params != expected.params || actual.results != expected.results {
            return trap::SIGNATURE_MISMATCH as i64;
        }
        if let Some(ti) = target_inst {
            if let Some(Some(nt)) = store.native.get(ti) {
                if let Some(&local) = nt.by_addr.get(&fa) {
                    return &nt.slots[local] as *const FuncSlot as i64;
                }
            }
        }
        let caller = store.native[vm.inst].as_ref().expect("native caller");
        match &caller.bridge_slots[type_idx as usize] {
            Some(slot) => {
                vm.pending = fa as u64;
                slot as *const FuncSlot as i64
            }
            None => {
                vm.error = Some("wasm: indirect call target has no native bridge".into());
                trap::HELPER as i64
            }
        }
    })
}

extern "C" fn h_call_slow(vm: *mut VmCtx) -> i32 {
    guarded(vm, 1, |vm, store| {
        let callee = vm.pending as usize;
        let ty = store.funcs[callee].ty();
        let args = ty
            .params
            .iter()
            .enumerate()
            .map(|(i, &t)| Val::from_bits(t, vm.args[i]))
            .collect();
        let host = unsafe { &mut *(vm.host as *mut &mut dyn Host) };
        let r = store.invoke(callee, args, *host, 0);
        unsafe { sync_mem(vm) };
        match r {
            Ok(vals) => {
                for (i, v) in vals.iter().enumerate() {
                    vm.args[i] = v.to_bits();
                }
                0
            }
            Err(e) => {
                vm.error = Some(e);
                1
            }
        }
    })
}

// ----- stack bounds -----------------------------------------------------------------------------

/// Headroom kept below native frames for helpers, host calls and the interpreter.
const STACK_MARGIN: u64 = 256 * 1024;

fn stack_limit() -> u64 {
    let here = &0u8 as *const u8 as u64;
    match stack_low() {
        Some(low) if low + STACK_MARGIN < here => low + STACK_MARGIN,
        Some(_) => here,
        None => here.saturating_sub(STACK_MARGIN),
    }
}

#[cfg(windows)]
fn stack_low() -> Option<u64> {
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentThreadStackLimits(low: *mut usize, high: *mut usize);
    }
    let (mut low, mut high) = (0usize, 0usize);
    unsafe { GetCurrentThreadStackLimits(&mut low, &mut high) };
    Some(low as u64)
}

#[cfg(target_os = "linux")]
fn stack_low() -> Option<u64> {
    extern "C" {
        fn pthread_self() -> usize;
        fn pthread_getattr_np(thread: usize, attr: *mut u64) -> i32;
        fn pthread_attr_getstack(attr: *const u64, addr: *mut *mut u8, size: *mut usize) -> i32;
        fn pthread_attr_destroy(attr: *mut u64) -> i32;
    }
    let mut attr = [0u64; 16];
    unsafe {
        if pthread_getattr_np(pthread_self(), attr.as_mut_ptr()) != 0 {
            return None;
        }
        let (mut addr, mut size) = (std::ptr::null_mut(), 0usize);
        let ok = pthread_attr_getstack(attr.as_ptr(), &mut addr, &mut size) == 0;
        pthread_attr_destroy(attr.as_mut_ptr());
        ok.then_some(addr as u64)
    }
}

#[cfg(target_os = "macos")]
fn stack_low() -> Option<u64> {
    extern "C" {
        fn pthread_self() -> usize;
        fn pthread_get_stackaddr_np(thread: usize) -> *mut u8;
        fn pthread_get_stacksize_np(thread: usize) -> usize;
    }
    unsafe {
        let t = pthread_self();
        Some(pthread_get_stackaddr_np(t) as u64 - pthread_get_stacksize_np(t) as u64)
    }
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn stack_low() -> Option<u64> {
    None
}
