//! Differential tests: every function is run by the wasm interpreter (`exec`) and, translated to
//! IR, by the `lumen-codegen` interpreter; results, traps and final memory must agree.

use std::rc::Rc;

use super::exec::{Host, Imports, Store, Val, PAGE_SIZE};
use super::parse::{self, ValType};
use super::translate::{self, *};
use lumen_codegen::interp::{self, Env};
use lumen_codegen::{opt, ExtFunc, Function, Signature};

// ---- a tiny module assembler ------------------------------------------------------------------

fn uleb(mut v: u64, out: &mut Vec<u8>) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn sleb(mut v: i64, out: &mut Vec<u8>) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        let done = (v == 0 && b & 0x40 == 0) || (v == -1 && b & 0x40 != 0);
        out.push(if done { b } else { b | 0x80 });
        if done {
            return;
        }
    }
}

fn section(id: u8, body: Vec<u8>, out: &mut Vec<u8>) {
    out.push(id);
    uleb(body.len() as u64, out);
    out.extend(body);
}

fn vt(t: ValType) -> u8 {
    match t {
        ValType::I32 => 0x7f,
        ValType::I64 => 0x7e,
        ValType::F32 => 0x7d,
        ValType::F64 => 0x7c,
        _ => unreachable!(),
    }
}

struct Func {
    params: Vec<ValType>,
    results: Vec<ValType>,
    locals: Vec<ValType>,
    body: Vec<u8>,
}

/// A module with one memory (1 page, `data` at 0), the given mutable globals, and every function
/// exported as `f{i}`. Function `i` has type index `i`; `extra_types` follow (for block types
/// and `call_indirect`).
fn module(funcs: &[Func], globals: &[(ValType, i64)], data: &[u8]) -> Vec<u8> {
    let mut m = vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00];
    let mut types = Vec::new();
    uleb(funcs.len() as u64, &mut types);
    for f in funcs {
        types.push(0x60);
        uleb(f.params.len() as u64, &mut types);
        types.extend(f.params.iter().map(|&t| vt(t)));
        uleb(f.results.len() as u64, &mut types);
        types.extend(f.results.iter().map(|&t| vt(t)));
    }
    section(1, types, &mut m);
    let mut fs = Vec::new();
    uleb(funcs.len() as u64, &mut fs);
    for i in 0..funcs.len() {
        uleb(i as u64, &mut fs);
    }
    section(3, fs, &mut m);
    section(5, vec![1, 0x00, 1], &mut m); // one memory, min 1 page
    if !globals.is_empty() {
        let mut gs = Vec::new();
        uleb(globals.len() as u64, &mut gs);
        for &(t, v) in globals {
            gs.push(vt(t));
            gs.push(1);
            match t {
                ValType::I32 => {
                    gs.push(0x41);
                    sleb(v, &mut gs);
                }
                ValType::I64 => {
                    gs.push(0x42);
                    sleb(v, &mut gs);
                }
                ValType::F32 => {
                    gs.push(0x43);
                    gs.extend((v as f32).to_le_bytes());
                }
                ValType::F64 => {
                    gs.push(0x44);
                    gs.extend((v as f64).to_le_bytes());
                }
                _ => unreachable!(),
            }
            gs.push(0x0b);
        }
        section(6, gs, &mut m);
    }
    let mut ex = Vec::new();
    uleb(funcs.len() as u64, &mut ex);
    for i in 0..funcs.len() {
        let name = format!("f{i}");
        uleb(name.len() as u64, &mut ex);
        ex.extend(name.bytes());
        ex.push(0x00);
        uleb(i as u64, &mut ex);
    }
    section(7, ex, &mut m);
    let mut code = Vec::new();
    uleb(funcs.len() as u64, &mut code);
    for f in funcs {
        let mut body = Vec::new();
        uleb(f.locals.len() as u64, &mut body);
        for &l in &f.locals {
            body.push(1);
            body.push(vt(l));
        }
        body.extend(&f.body);
        body.push(0x0b);
        uleb(body.len() as u64, &mut code);
        code.extend(body);
    }
    section(10, code, &mut m);
    if !data.is_empty() {
        let mut d = vec![1, 0x00, 0x41, 0x00, 0x0b];
        uleb(data.len() as u64, &mut d);
        d.extend(data);
        section(11, d, &mut m);
    }
    m
}

// Opcode helpers.
fn i32c(v: i32) -> Vec<u8> {
    let mut o = vec![0x41];
    sleb(v as i64, &mut o);
    o
}
fn i64c(v: i64) -> Vec<u8> {
    let mut o = vec![0x42];
    sleb(v, &mut o);
    o
}
fn f64c(v: f64) -> Vec<u8> {
    let mut o = vec![0x44];
    o.extend(v.to_le_bytes());
    o
}
fn get(i: u32) -> Vec<u8> {
    let mut o = vec![0x20];
    uleb(i as u64, &mut o);
    o
}
fn set(i: u32) -> Vec<u8> {
    let mut o = vec![0x21];
    uleb(i as u64, &mut o);
    o
}
fn tee(i: u32) -> Vec<u8> {
    let mut o = vec![0x22];
    uleb(i as u64, &mut o);
    o
}
fn mem(op: u8, offset: u32) -> Vec<u8> {
    let mut o = vec![op, 0];
    uleb(offset as u64, &mut o);
    o
}
fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
    parts.concat()
}

use ValType::{F32, F64, I32, I64};

// ---- running both ways ------------------------------------------------------------------------

struct NoHost;
impl Host for NoHost {
    fn call_host(&mut self, _: usize, _: &[Val], _: &[ValType]) -> Result<Vec<Val>, String> {
        Err("no host".into())
    }
}

fn val_bits(v: Val) -> u64 {
    match v {
        Val::I32(x) => x as u32 as u64,
        Val::I64(x) => x as u64,
        Val::F32(x) => x.to_bits() as u64,
        Val::F64(x) => x.to_bits(),
        Val::Ref(_) => panic!("ref"),
    }
}

fn bits_val(t: ValType, b: u64) -> Val {
    match t {
        I32 => Val::I32(b as u32 as i32),
        I64 => Val::I64(b as i64),
        F32 => Val::F32(f32::from_bits(b as u32)),
        F64 => Val::F64(f64::from_bits(b)),
        _ => panic!("ref"),
    }
}

/// Buffer layout: VmCtx at 0, the global-cell table at 64, cells at 1024, memory at 4096.
const GLOBAL_TABLE: u64 = 64;
const CELLS: u64 = 1024;
const MEM: u64 = 4096;

struct TestEnv {
    buf: Vec<u8>,
    funcs: Vec<Function>,
}

impl TestEnv {
    fn rd(&self, a: u64) -> u64 {
        u64::from_le_bytes(self.buf[a as usize..a as usize + 8].try_into().unwrap())
    }
    fn wr(&mut self, a: u64, v: u64) {
        self.buf[a as usize..a as usize + 8].copy_from_slice(&v.to_le_bytes());
    }
    fn mem_len(&self) -> u64 {
        self.rd(VMCTX_MEM_LEN as u64)
    }
    fn memory(&self) -> &[u8] {
        &self.buf[MEM as usize..(MEM + self.mem_len()) as usize]
    }
}

impl Env for TestEnv {
    fn load(&mut self, addr: u64, bytes: u32) -> u64 {
        let a = addr as usize;
        assert!(a + bytes as usize <= self.buf.len(), "load outside the test buffer: {a:#x}");
        let mut b = [0u8; 8];
        b[..bytes as usize].copy_from_slice(&self.buf[a..a + bytes as usize]);
        u64::from_le_bytes(b)
    }
    fn store(&mut self, addr: u64, bytes: u32, value: u64) {
        let a = addr as usize;
        assert!(a + bytes as usize <= self.buf.len(), "store outside the test buffer: {a:#x}");
        self.buf[a..a + bytes as usize].copy_from_slice(&value.to_le_bytes()[..bytes as usize]);
    }
    fn call(&mut self, func: &ExtFunc, _: &Signature, args: &[u64]) -> Result<Vec<u64>, u32> {
        match func.id {
            HELPER_MEMORY_GROW => {
                let delta = args[1] as u32 as i32;
                let old = self.mem_len() / PAGE_SIZE as u64;
                if delta < 0 || old + delta as u64 > 4 {
                    return Ok(vec![u32::MAX as u64]);
                }
                let new_len = (old + delta as u64) * PAGE_SIZE as u64;
                self.buf.resize((MEM + new_len) as usize, 0);
                self.wr(VMCTX_MEM_LEN as u64, new_len);
                Ok(vec![old])
            }
            HELPER_MEMORY_COPY | HELPER_MEMORY_FILL => {
                let (d, s, n) = (args[1] as u32 as u64, args[2] as u32 as u64, args[3] as u32 as u64);
                let len = self.mem_len();
                let fill = func.id == HELPER_MEMORY_FILL;
                if d + n > len || (!fill && s + n > len) {
                    return Ok(vec![1]);
                }
                let (d, s, n) = ((MEM + d) as usize, (MEM + s) as usize, n as usize);
                if fill {
                    self.buf[d..d + n].fill(s as u8);
                } else {
                    self.buf.copy_within(s..s + n, d);
                }
                Ok(vec![0])
            }
            id if id < HELPER_BASE => {
                let f = self.funcs[id as usize].clone();
                interp::run(&f, self, args)
            }
            id => panic!("unexpected helper {id:#x}"),
        }
    }
    fn call_indirect(&mut self, _: &Signature, callee: u64, _: &[u64]) -> Result<Vec<u64>, u32> {
        panic!("no indirect calls in these tests ({callee:#x})")
    }
}

/// Run every `(function, args)` case through both engines and compare.
fn differential(bytes: &[u8], cases: &[(u32, Vec<Val>)]) {
    for optimize in [false, true] {
        let module = parse::decode(bytes).expect("decode");
        let funcs: Vec<Function> = (0..module.code.len() as u32)
            .map(|f| {
                let mut func = translate::translate(&module, f).unwrap_or_else(|e| panic!("f{f}: {e}"));
                if optimize {
                    opt::optimize(&mut func);
                }
                func
            })
            .collect();
        for (f, args) in cases {
            // Fresh instances per case so memory and globals start equal.
            let mut store = Store::default();
            let inst = store.instantiate(Rc::clone(&module), Imports::default()).unwrap();
            let (_, addr) = store.export_addr(inst, &format!("f{f}")).unwrap();
            let instance = Rc::clone(&store.instances[inst]);

            let mut env = TestEnv {
                buf: vec![0; MEM as usize],
                funcs: funcs.clone(),
            };
            let mem = &store.memories[instance.mem_addrs[0]].bytes;
            env.buf.extend_from_slice(mem);
            env.wr(VMCTX_MEM_BASE as u64, MEM);
            env.wr(VMCTX_MEM_LEN as u64, mem.len() as u64);
            env.wr(VMCTX_GLOBALS as u64, GLOBAL_TABLE);
            for (i, &ga) in instance.global_addrs.iter().enumerate() {
                let cell = CELLS + 8 * i as u64;
                env.wr(GLOBAL_TABLE + 8 * i as u64, cell);
                env.wr(cell, val_bits(store.globals[ga].val));
            }

            let want = store.invoke(addr, args.clone(), &mut NoHost, 0);
            let mut ir_args = vec![0u64];
            ir_args.extend(args.iter().map(|&v| val_bits(v)));
            let got = interp::run(&funcs[*f as usize], &mut env, &ir_args);
            let ty = translate::func_type(&module, *f).unwrap();
            let ctx = format!("f{f}{args:?} (optimize={optimize})\n{}", funcs[*f as usize]);
            match (want, got) {
                (Ok(w), Ok(g)) => {
                    let g: Vec<Val> = g.iter().zip(&ty.results).map(|(&b, &t)| bits_val(t, b)).collect();
                    let same = w.len() == g.len()
                        && w.iter().zip(&g).all(|(&a, &b)| {
                            let (a, b) = (val_bits(a), val_bits(b));
                            a == b || (is_nan_bits(a) && is_nan_bits(b))
                        });
                    assert!(same, "results differ: interpreter {w:?}, IR {g:?} for {ctx}");
                }
                (Err(w), Err(code)) => {
                    assert_eq!(w, trap::message(code), "trap kinds differ for {ctx}");
                    continue;
                }
                (w, g) => panic!("outcomes differ: interpreter {w:?}, IR {g:?} for {ctx}"),
            }
            let imem = &store.memories[instance.mem_addrs[0]].bytes;
            assert!(imem == env.memory(), "memory differs after {ctx}");
            for (i, &ga) in instance.global_addrs.iter().enumerate() {
                let cell = env.rd(CELLS + 8 * i as u64);
                let t = match store.globals[ga].val {
                    Val::I32(_) | Val::F32(_) => cell & 0xffff_ffff,
                    _ => cell,
                };
                assert_eq!(val_bits(store.globals[ga].val), t, "global {i} differs after {ctx}");
            }
        }
    }
}

fn is_nan_bits(b: u64) -> bool {
    f64::from_bits(b).is_nan() || (b >> 32 == 0 && f32::from_bits(b as u32).is_nan())
}

// ---- tests ------------------------------------------------------------------------------------

#[test]
fn arithmetic_and_traps() {
    let bin = |op: u8, t: ValType| Func {
        params: vec![t, t],
        results: vec![t],
        locals: vec![],
        body: cat(&[get(0), get(1), vec![op]]),
    };
    let funcs = [
        bin(0x6d, I32), // 0: div_s
        bin(0x6e, I32), // 1: div_u
        bin(0x6f, I32), // 2: rem_s
        bin(0x70, I32), // 3: rem_u
        bin(0x74, I32), // 4: shl
        bin(0x75, I32), // 5: shr_s
        bin(0x77, I32), // 6: rotl
        bin(0x7f, I64), // 7: i64.div_s
        bin(0x81, I64), // 8: i64.rem_s
        bin(0x78, I32), // 9: rotr
    ];
    let m = module(&funcs, &[], &[]);
    let mut cases = Vec::new();
    let vals = [0, 1, -1, 7, -7, 31, 33, i32::MIN, i32::MAX];
    for f in 0..7u32 {
        for &a in &vals {
            for &b in &vals {
                cases.push((f, vec![Val::I32(a), Val::I32(b)]));
            }
        }
    }
    cases.push((9, vec![Val::I32(1), Val::I32(1)]));
    for &(a, b) in &[(i64::MIN, -1), (10, 0), (-9, 4), (i64::MIN, 1)] {
        cases.push((7, vec![Val::I64(a), Val::I64(b)]));
        cases.push((8, vec![Val::I64(a), Val::I64(b)]));
    }
    differential(&m, &cases);
}

#[test]
fn loops_blocks_and_branches() {
    // f0: sum 0..n with a loop and br_if.
    let sum = Func {
        params: vec![I32],
        results: vec![I32],
        locals: vec![I32, I32],
        body: cat(&[
            vec![0x02, 0x40], // block
            vec![0x03, 0x40], //   loop
            get(1), get(0), vec![0x4e], vec![0x0d, 1], // if i >= n br 1
            get(2), get(1), vec![0x6a], set(2),
            get(1), i32c(1), vec![0x6a], set(1),
            vec![0x0c, 0],    //   br 0
            vec![0x0b],       //   end loop
            vec![0x0b],       // end block
            get(2),
        ]),
    };
    // f1: br_table dispatch; every target has arity 0, the result comes from the arms.
    let table = Func {
        params: vec![I32],
        results: vec![I32],
        locals: vec![],
        body: cat(&[
            vec![0x02, 0x7f], // block $out (result i32)
            vec![0x02, 0x40], //   block $d
            vec![0x02, 0x40], //     block $b1
            vec![0x02, 0x40], //       block $b0
            get(0),
            vec![0x0e, 2, 0, 1, 2], // br_table $b0 $b1 default $d
            vec![0x0b],
            i32c(100), vec![0x0c, 2], // case 0 → 100
            vec![0x0b],
            i32c(200), vec![0x0c, 1], // case 1 → 200
            vec![0x0b],
            i32c(-1),                 // default
            vec![0x0b],
        ]),
    };
    // f2: if/else with results, nested, plus select and early return.
    let ifs = Func {
        params: vec![I32, I32],
        results: vec![I32],
        locals: vec![],
        body: cat(&[
            get(0), vec![0x45], vec![0x04, 0x40], i32c(-5), vec![0x0f], vec![0x0b], // if !a return -5
            get(0), get(1), vec![0x48], // a < b
            vec![0x04, 0x7f],
            get(0), get(1), get(0), get(1), vec![0x4a], vec![0x1b], // select(max)
            vec![0x05],
            get(1), i32c(3), vec![0x6c],
            vec![0x0b],
            i32c(1), vec![0x6a],
        ]),
    };
    // f3: unreachable code after br, including nested blocks.
    let dead = Func {
        params: vec![I32],
        results: vec![I32],
        locals: vec![],
        body: cat(&[
            vec![0x02, 0x7f],
            get(0),
            vec![0x0c, 0],
            vec![0x02, 0x40], i32c(9), vec![0x1a], vec![0x0b], // dead nested block
            i32c(1), i32c(2), vec![0x6a],
            vec![0x0b],
            i32c(10), vec![0x6a],
        ]),
    };
    // f4: loop carrying a value via block params is multi-value; use locals + tee instead.
    let fib = Func {
        params: vec![I32],
        results: vec![I64],
        locals: vec![I64, I64, I64],
        body: cat(&[
            i64c(1), set(2),
            vec![0x02, 0x40],
            vec![0x03, 0x40],
            get(0), vec![0x45], vec![0x0d, 1],
            get(1), get(2), vec![0x7c], set(3),
            get(2), set(1),
            get(3), set(2),
            get(0), i32c(1), vec![0x6b], tee(0), vec![0x1a],
            vec![0x0c, 0],
            vec![0x0b],
            vec![0x0b],
            get(1),
        ]),
    };
    let m = module(&[sum, table, ifs, dead, fib], &[], &[]);
    let mut cases = Vec::new();
    for n in [0, 1, 10, 1000] {
        cases.push((0, vec![Val::I32(n)]));
        cases.push((4, vec![Val::I32(n % 90)]));
    }
    for i in [-1, 0, 1, 2, 3, 100] {
        cases.push((1, vec![Val::I32(i)]));
        cases.push((3, vec![Val::I32(i)]));
    }
    for (a, b) in [(0, 1), (1, 2), (2, 1), (-4, 3), (5, 5)] {
        cases.push((2, vec![Val::I32(a), Val::I32(b)]));
    }
    differential(&m, &cases);
}

#[test]
fn memory_globals_and_calls() {
    // f0: store a at [p+4] as i32, load it back as i8_s/u16 and i64 variants, bump a global.
    let mem_ops = Func {
        params: vec![I32, I32],
        results: vec![I64],
        locals: vec![],
        body: cat(&[
            get(0), get(1), mem(0x36, 4),               // i32.store offset=4
            get(0), mem(0x2c, 4),                       // i32.load8_s offset=4
            get(0), mem(0x2f, 4), vec![0x6a],           // + i32.load16_u
            vec![0xac],                                 // i64.extend_i32_s
            get(0), mem(0x35, 4), vec![0x7c],           // + i64.load32_u
            vec![0x23, 0], i32c(1), vec![0x6a], vec![0x24, 0], // g0 += 1
        ]),
    };
    // f1: calls f2 in a loop, accumulating into memory at 0 (i64) and returning it.
    let caller = Func {
        params: vec![I32],
        results: vec![I64],
        locals: vec![],
        body: cat(&[
            vec![0x02, 0x40],
            vec![0x03, 0x40],
            get(0), vec![0x45], vec![0x0d, 1],
            i32c(0),
            i32c(0), mem(0x29, 0),
            get(0), vec![0x10, 2], vec![0xad], vec![0x7c],
            mem(0x37, 0),
            get(0), i32c(1), vec![0x6b], set(0),
            vec![0x0c, 0],
            vec![0x0b],
            vec![0x0b],
            i32c(0), mem(0x29, 0),
        ]),
    };
    // f2: x*x + global g1 (i64 global read, wrapped).
    let sq = Func {
        params: vec![I32],
        results: vec![I32],
        locals: vec![],
        body: cat(&[get(0), get(0), vec![0x6c], vec![0x23, 1], vec![0xa7], vec![0x6a]]),
    };
    // f3: memory.size, memory.grow, fill, copy, and an out-of-bounds load at the end.
    let bulk = Func {
        params: vec![I32],
        results: vec![I32],
        locals: vec![],
        body: cat(&[
            i32c(16), i32c(0xab), i32c(8), vec![0xfc, 11, 0],    // fill [16..24) = 0xab
            i32c(32), i32c(12), i32c(16), vec![0xfc, 10, 0, 0],  // copy [12..28) → [32..48)
            i32c(1), vec![0x40, 0], vec![0x1a],                  // grow 1
            vec![0x3f, 0],                                       // size
            get(0), mem(0x28, 0), vec![0x6a],                    // + load [a]
        ]),
    };
    let data: Vec<u8> = (0..64u8).collect();
    let m = module(&[mem_ops, caller, sq, bulk], &[(I32, 5), (I64, 1 << 33 | 7)], &data);
    let mut cases = vec![
        (0, vec![Val::I32(0), Val::I32(-2)]),
        (0, vec![Val::I32(100), Val::I32(0x12345678)]),
        (0, vec![Val::I32(65528), Val::I32(1)]), // 65532+4 > len → trap
        (0, vec![Val::I32(-1), Val::I32(1)]),
        (1, vec![Val::I32(0)]),
        (1, vec![Val::I32(20)]),
        (2, vec![Val::I32(9)]),
    ];
    for a in [0, 30, 65536, 131068, 131069, -4] {
        cases.push((3, vec![Val::I32(a)]));
    }
    differential(&m, &cases);
}

#[test]
fn float_ops_and_conversions() {
    let un = |ops: Vec<u8>, p: ValType, r: ValType| Func {
        params: vec![p],
        results: vec![r],
        locals: vec![],
        body: cat(&[get(0), ops]),
    };
    let funcs = [
        un(vec![0xaa], F64, I32),       // 0: i32.trunc_f64_s
        un(vec![0xab], F64, I32),       // 1: i32.trunc_f64_u
        un(vec![0xb0], F64, I64),       // 2: i64.trunc_f64_s
        un(vec![0xb1], F64, I64),       // 3: i64.trunc_f64_u
        un(vec![0xfc, 2], F64, I32),    // 4: i32.trunc_sat_f64_s
        un(vec![0xfc, 7], F64, I64),    // 5: i64.trunc_sat_f64_u
        un(vec![0x9e], F64, F64),       // 6: f64.nearest
        un(vec![0x9c], F64, F64),       // 7: f64.floor
        un(vec![0xb6, 0xa8], F64, I32), // 8: demote then i32.trunc_f32_s
        Func {
            params: vec![F64, F64],
            results: vec![F64],
            locals: vec![],
            body: cat(&[get(0), get(1), vec![0xa4], get(0), get(1), vec![0xa5], vec![0xa0]]), // min+max
        },
        Func {
            params: vec![F64, F64],
            results: vec![I32],
            locals: vec![],
            body: cat(&[get(0), get(1), vec![0x63], get(0), get(1), vec![0x62], i32c(2), vec![0x6c], vec![0x6a]]), // lt + 2*ne
        },
        un(vec![0xb9], I64, F64),       // 11: f64.convert_i64_s — note param is i64
        un(vec![0xba], I64, F64),       // 12: f64.convert_i64_u
        Func {
            params: vec![F64],
            results: vec![F64],
            locals: vec![],
            body: cat(&[get(0), f64c(-0.0), vec![0xa6]]), // copysign(x, -0)
        },
    ];
    let m = module(&funcs, &[], &[]);
    let fs = [
        0.0, -0.0, 0.5, -0.5, 1.5, 2.5, -2.5, 1e10, -1e10, 2147483647.9, 2147483648.0,
        -2147483648.9, -2147483649.0, 4294967295.5, 4294967296.0, 9.3e18, -9.3e18, 1.8e19,
        f64::INFINITY, f64::NEG_INFINITY, f64::NAN,
    ];
    let mut cases = Vec::new();
    for &x in &fs {
        for f in [0u32, 1, 2, 3, 4, 5, 6, 7, 8, 13] {
            cases.push((f, vec![Val::F64(x)]));
        }
        for &y in &[0.0, -0.0, 1.0, f64::NAN] {
            cases.push((9, vec![Val::F64(x), Val::F64(y)]));
            cases.push((10, vec![Val::F64(x), Val::F64(y)]));
        }
    }
    for i in [0i64, -1, i64::MIN, i64::MAX, 1 << 53 | 1] {
        cases.push((11, vec![Val::I64(i)]));
        cases.push((12, vec![Val::I64(i)]));
    }
    differential(&m, &cases);
}
