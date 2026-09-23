//! Native execution against the IR interpreter: random programs (straight-line code, diamonds,
//! loops, switches, memory, calls, traps) must produce the same result, trap and memory.

use super::*;
use crate::interp::{self, Env};
use crate::ir::*;
use crate::jitmem::ExecMemory;
use crate::FunctionBuilder;

#[repr(C)]
struct Ctx {
    entry_sp: u64,
    stack_limit: u64,
}

const STACK_OVERFLOW: u32 = 99;

fn config() -> Config {
    Config::host(Some(TrapConfig {
        entry_sp_offset: 0,
        stack_limit: Some((8, STACK_OVERFLOW)),
    }))
}

extern "C" fn h_mix(a: i64, b: f64, c: i32, d: f32) -> i64 {
    a.wrapping_mul(31) ^ b.to_bits() as i64 ^ ((c as i64) << 3) ^ d.to_bits() as i64
}

#[allow(clippy::too_many_arguments)]
extern "C" fn h_many(
    a0: i32,
    f1: f64,
    a2: i64,
    f3: f32,
    a4: i32,
    f5: f64,
    a6: i64,
    a7: i32,
    f8: f64,
    a9: i64,
    a10: i32,
    a11: i64,
) -> f64 {
    a0 as f64 * 1.5 + f1 - a2 as f64 + f3 as f64 * 2.0 + a4 as f64 - f5 * 0.25 + a6 as f64
        + a7 as f64 * 3.0
        - f8
        + (a9 >> 7) as f64
        + a10 as f64
        + (a11 % 1000) as f64
}

fn mix_sig() -> Signature {
    Signature::new(vec![Type::I64, Type::F64, Type::I32, Type::F32], vec![Type::I64])
}

fn many_sig() -> Signature {
    use Type::*;
    Signature::new(
        vec![I32, F64, I64, F32, I32, F64, I64, I32, F64, I64, I32, I64],
        vec![F64],
    )
}

fn call_host(id: u32, a: &[u64]) -> Vec<u64> {
    let f32_ = |x: u64| f32::from_bits(x as u32);
    let f64_ = f64::from_bits;
    match id {
        1 => vec![h_mix(a[0] as i64, f64_(a[1]), a[2] as i32, f32_(a[3])) as u64],
        2 => vec![h_many(
            a[0] as i32,
            f64_(a[1]),
            a[2] as i64,
            f32_(a[3]),
            a[4] as i32,
            f64_(a[5]),
            a[6] as i64,
            a[7] as i32,
            f64_(a[8]),
            a[9] as i64,
            a[10] as i32,
            a[11] as i64,
        )
        .to_bits()],
        _ => panic!("no host fn {id}"),
    }
}

fn resolve(id: u32) -> Option<u64> {
    match id {
        1 => Some(h_mix as *const () as u64),
        2 => Some(h_many as *const () as u64),
        _ => None,
    }
}

struct TestEnv {
    mem: Vec<u8>,
}

impl Env for TestEnv {
    fn load(&mut self, addr: u64, bytes: u32) -> u64 {
        let a = addr as usize;
        let mut b = [0u8; 8];
        b[..bytes as usize].copy_from_slice(&self.mem[a..a + bytes as usize]);
        u64::from_le_bytes(b)
    }
    fn store(&mut self, addr: u64, bytes: u32, value: u64) {
        let a = addr as usize;
        self.mem[a..a + bytes as usize].copy_from_slice(&value.to_le_bytes()[..bytes as usize]);
    }
    fn call(&mut self, func: &ExtFunc, _: &Signature, args: &[u64]) -> Result<Vec<u64>, u32> {
        Ok(call_host(func.id, args))
    }
    fn call_indirect(&mut self, _: &Signature, callee: u64, args: &[u64]) -> Result<Vec<u64>, u32> {
        let id = (1..=2).find(|&i| resolve(i) == Some(callee)).expect("callee");
        Ok(call_host(id, args))
    }
}

type Tramp = unsafe extern "C" fn(*mut Ctx, *const u8, *mut u64) -> u32;

/// Compile and run `f` natively. `args[0]` is replaced by the context pointer.
fn run_native(f: &Function, cfg: &Config, args: &[u64], ctx: &mut Ctx) -> Result<Vec<u64>, u32> {
    let mut c = compile(f, cfg).unwrap_or_else(|e| panic!("{e}\n{f}"));
    c.link(resolve).unwrap();
    let code = ExecMemory::new(&c.code).unwrap();
    let tramp = ExecMemory::new(&trampoline(&f.sig, cfg).unwrap()).unwrap();
    let mut slots: Vec<u64> = args.to_vec();
    slots[0] = ctx as *mut Ctx as u64;
    slots.resize(slots.len().max(1), 0);
    let entry: Tramp = unsafe { std::mem::transmute(tramp.as_ptr()) };
    let rc = unsafe { entry(ctx, code.as_ptr(), slots.as_mut_ptr()) };
    if rc != 0 {
        return Err(rc - 1);
    }
    Ok(slots[..f.sig.results.len()].to_vec())
}

// ----- random programs -----

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len() as u64) as usize]
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

const MEM_SIZE: usize = 4096;

struct Gen<'a> {
    b: FunctionBuilder<'a>,
    r: Rng,
    vars: Vec<(crate::Variable, Type)>,
    mem: Value,
    fptr: Value,
    mix: FuncRef,
    many: FuncRef,
    mix_sig: SigRef,
    stmts: u32,
}

const TYPES: [Type; 4] = [Type::I32, Type::I64, Type::F32, Type::F64];

impl Gen<'_> {
    fn var(&mut self, t: Type) -> crate::Variable {
        let vs: Vec<_> = self.vars.iter().filter(|v| v.1 == t).map(|v| v.0).collect();
        self.r.pick(&vs)
    }

    fn konst(&mut self, t: Type) -> Value {
        match t {
            Type::I32 | Type::I64 => {
                let imm = match self.r.below(4) {
                    0 => self.r.pick(&[0, 1, -1, 2, 7, 31, 32, 63, 64, 255, 4096]),
                    1 => self.r.pick(&[
                        i32::MIN as i64,
                        i32::MAX as i64,
                        i64::MIN,
                        i64::MAX,
                        u32::MAX as i64,
                        0x1234_5678_9abc,
                    ]),
                    2 => self.r.below(100) as i64 - 50,
                    _ => self.r.next() as i64,
                };
                self.b.iconst(t, imm)
            }
            _ => {
                let rnd = (self.r.below(2000) as f64 - 1000.0) / 7.0;
                let v = self.r.pick(&[
                    0.0,
                    -0.0,
                    1.0,
                    -1.5,
                    0.5,
                    2.5,
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    f64::NAN,
                    1e10,
                    -3e9,
                    4294967296.0,
                    9.3e18,
                    -9.3e18,
                    123.456,
                    rnd,
                ]);
                if t == Type::F32 {
                    self.b.f32const(v as f32)
                } else {
                    self.b.f64const(v)
                }
            }
        }
    }

    /// Replace any NaN by the canonical one, so NaN payloads (which hardware and the
    /// interpreter may choose differently) never reach observable bits.
    fn canon(&mut self, v: Value, t: Type) -> Value {
        let ne = self.b.fcmp(FloatCC::Ne, v, v);
        let nan = if t == Type::F32 {
            self.b.f32const_bits(0x7fc0_0000)
        } else {
            self.b.f64const_bits(0x7ff8_0000_0000_0000)
        };
        self.b.select(ne, nan, v)
    }

    fn address(&mut self) -> (Value, i32) {
        if self.r.chance(50) {
            (self.mem, self.r.below(MEM_SIZE as u64 / 2) as i32)
        } else {
            let v = self.var(Type::I32);
            let i = self.b.use_var(v);
            let m = self.b.iconst(Type::I32, 0x3f8);
            let i = self.b.binary(BinaryOp::Band, i, m);
            let i = self.b.convert(ConvOp::Uext, Type::I64, i);
            let a = self.b.binary(BinaryOp::Iadd, self.mem, i);
            (a, self.r.below(64) as i32)
        }
    }

    fn mem_kind(&mut self, t: Type) -> MemKind {
        use MemKind::*;
        match t {
            Type::I32 => self.r.pick(&[I32, I32S8, I32U8, I32S16, I32U16]),
            Type::I64 => self.r.pick(&[I64, I64S8, I64U8, I64S16, I64U16, I64S32, I64U32]),
            Type::F32 => F32,
            Type::F64 => F64,
        }
    }

    fn leaf(&mut self, t: Type) -> Value {
        if self.r.chance(25) {
            self.konst(t)
        } else {
            let v = self.var(t);
            self.b.use_var(v)
        }
    }

    fn expr(&mut self, t: Type, d: u32) -> Value {
        if d == 0 || self.r.chance(25) {
            return self.leaf(t);
        }
        match t {
            Type::I32 | Type::I64 => self.int_expr(t, d - 1),
            _ => self.float_expr(t, d - 1),
        }
    }

    fn int_expr(&mut self, t: Type, d: u32) -> Value {
        use BinaryOp::*;
        let wide = if t == Type::I32 { Type::I64 } else { Type::I32 };
        match self.r.below(12) {
            0..=3 => {
                let op = self.r.pick(&[
                    Iadd, Isub, Imul, Band, Bor, Bxor, Ishl, Ushr, Sshr, Rotl, Rotr, Iadd, Isub,
                ]);
                let a = self.expr(t, d);
                let b = self.expr(t, d);
                self.b.binary(op, a, b)
            }
            4 => {
                let op = self.r.pick(&[Sdiv, Udiv, Srem, Urem]);
                let a = self.expr(t, d);
                let b = self.expr(t, d);
                // A positive odd divisor: never zero, never -1.
                let m = self.b.iconst(t, 0xffff);
                let b = self.b.binary(Band, b, m);
                let one = self.b.iconst(t, 1);
                let b = self.b.binary(Bor, b, one);
                self.b.binary(op, a, b)
            }
            5 => {
                let mut ops = vec![UnaryOp::Clz, UnaryOp::Ctz, UnaryOp::Popcnt, UnaryOp::Sext8, UnaryOp::Sext16];
                if t == Type::I64 {
                    ops.push(UnaryOp::Sext32);
                }
                let op = self.r.pick(&ops);
                let a = self.expr(t, d);
                self.b.unary(op, a)
            }
            6 if t == Type::I32 => {
                let ct = self.r.pick(&TYPES);
                let a = self.expr(ct, d);
                let b = self.expr(ct, d);
                if ct.is_int() {
                    let cc = self.r.pick(&[
                        IntCC::Eq,
                        IntCC::Ne,
                        IntCC::Slt,
                        IntCC::Sle,
                        IntCC::Sgt,
                        IntCC::Sge,
                        IntCC::Ult,
                        IntCC::Ule,
                        IntCC::Ugt,
                        IntCC::Uge,
                    ]);
                    self.b.icmp(cc, a, b)
                } else {
                    let cc = self.r.pick(&[
                        FloatCC::Eq,
                        FloatCC::Ne,
                        FloatCC::Lt,
                        FloatCC::Le,
                        FloatCC::Gt,
                        FloatCC::Ge,
                    ]);
                    self.b.fcmp(cc, a, b)
                }
            }
            6 => {
                let a = self.expr(Type::I64, d);
                let z = self.b.unary(UnaryOp::Eqz, a);
                self.b.convert(ConvOp::Uext, Type::I64, z)
            }
            7 => {
                let c = self.expr(Type::I32, d);
                let a = self.expr(t, d);
                let b = self.expr(t, d);
                self.b.select(c, a, b)
            }
            8 => {
                let a = self.expr(wide, d);
                if t == Type::I32 {
                    self.b.convert(ConvOp::Wrap, t, a)
                } else {
                    let op = self.r.pick(&[ConvOp::Sext, ConvOp::Uext]);
                    self.b.convert(op, t, a)
                }
            }
            9 => {
                let ft = self.r.pick(&[Type::F32, Type::F64]);
                let a = self.expr(ft, d);
                let op = self.r.pick(&[ConvOp::ToSintSat, ConvOp::ToUintSat]);
                self.b.convert(op, t, a)
            }
            10 => {
                let ft = if t == Type::I32 { Type::F32 } else { Type::F64 };
                let a = self.expr(ft, d);
                let a = self.canon(a, ft);
                self.b.convert(ConvOp::Bitcast, t, a)
            }
            _ => {
                let (a, off) = self.address();
                let k = self.mem_kind(t);
                self.b.load(k, a, off)
            }
        }
    }

    fn float_expr(&mut self, t: Type, d: u32) -> Value {
        use BinaryOp::*;
        let other = if t == Type::F32 { Type::F64 } else { Type::F32 };
        match self.r.below(9) {
            0..=2 => {
                let op = self.r.pick(&[Fadd, Fsub, Fmul, Fdiv, Fmin, Fmax, Fcopysign]);
                let a = self.expr(t, d);
                let mut b = self.expr(t, d);
                if op == Fcopysign {
                    b = self.canon(b, t);
                }
                self.b.binary(op, a, b)
            }
            3 => {
                use UnaryOp::*;
                let op = self.r.pick(&[Fneg, Fabs, Sqrt, Ceil, Floor, Trunc, Nearest]);
                let a = self.expr(t, d);
                self.b.unary(op, a)
            }
            4 => {
                let it = self.r.pick(&[Type::I32, Type::I64]);
                let a = self.expr(it, d);
                let op = self.r.pick(&[ConvOp::FromSint, ConvOp::FromUint]);
                self.b.convert(op, t, a)
            }
            5 => {
                let a = self.expr(other, d);
                let op = if t == Type::F64 { ConvOp::Promote } else { ConvOp::Demote };
                self.b.convert(op, t, a)
            }
            6 => {
                let it = if t == Type::F32 { Type::I32 } else { Type::I64 };
                let a = self.expr(it, d);
                self.b.convert(ConvOp::Bitcast, t, a)
            }
            7 => {
                let c = self.expr(Type::I32, d);
                let a = self.expr(t, d);
                let b = self.expr(t, d);
                self.b.select(c, a, b)
            }
            _ => {
                let (a, off) = self.address();
                self.b.load(if t == Type::F32 { MemKind::F32 } else { MemKind::F64 }, a, off)
            }
        }
    }

    fn arg(&mut self, t: Type) -> Value {
        let v = self.expr(t, 1);
        if t.is_float() {
            self.canon(v, t)
        } else {
            v
        }
    }

    fn body(&mut self, depth: u32) {
        let n = 1 + self.r.below(5);
        for _ in 0..n {
            self.stmt(depth);
        }
    }

    fn stmt(&mut self, depth: u32) {
        self.stmts += 1;
        let budget = self.stmts < 120;
        match self.r.below(100) {
            55..=62 => {
                let t = self.r.pick(&TYPES);
                let k = self.mem_kind(t);
                let mut v = self.expr(t, 2);
                if t.is_float() {
                    v = self.canon(v, t);
                }
                let (a, off) = self.address();
                self.b.store(k, a, v, off);
            }
            63..=67 => {
                let args: Vec<Value> = mix_sig().params.iter().map(|&t| self.arg(t)).collect();
                let r = if self.r.chance(50) {
                    self.b.call_fn(self.mix, &args)[0]
                } else {
                    self.b.call_indirect(self.mix_sig, self.fptr, &args)[0]
                };
                let v = self.var(Type::I64);
                self.b.def_var(v, r);
            }
            68..=70 => {
                let args: Vec<Value> = many_sig().params.iter().map(|&t| self.arg(t)).collect();
                let r = self.b.call_fn(self.many, &args)[0];
                let v = self.var(Type::F64);
                self.b.def_var(v, r);
            }
            71..=72 => {
                let a = self.expr(Type::I32, 1);
                let m = self.b.iconst(Type::I32, 31);
                let a = self.b.binary(BinaryOp::Band, a, m);
                let k = self.b.iconst(Type::I32, 5);
                let c = self.b.icmp(IntCC::Eq, a, k);
                let code = self.r.below(8) as u32;
                self.b.trap_if(c, code);
            }
            73..=82 if depth > 0 && budget => {
                let c = self.expr(Type::I32, 2);
                let (then, else_, merge) =
                    (self.b.create_block(), self.b.create_block(), self.b.create_block());
                self.b.brif(c, then, &[], else_, &[]);
                self.b.seal_block(then);
                self.b.seal_block(else_);
                self.b.switch_to_block(then);
                self.body(depth - 1);
                if self.r.chance(5) {
                    self.b.trap(7);
                } else {
                    self.b.jump(merge, &[]);
                }
                self.b.switch_to_block(else_);
                self.body(depth - 1);
                self.b.jump(merge, &[]);
                self.b.seal_block(merge);
                self.b.switch_to_block(merge);
            }
            83..=90 if depth > 0 && budget => {
                let i = self.b.declare_var(Type::I32);
                let z = self.b.iconst(Type::I32, 0);
                self.b.def_var(i, z);
                let n = self.b.iconst(Type::I32, self.r.below(4) as i64);
                let (header, body, exit) =
                    (self.b.create_block(), self.b.create_block(), self.b.create_block());
                self.b.jump(header, &[]);
                self.b.switch_to_block(header);
                let iv = self.b.use_var(i);
                let c = self.b.icmp(IntCC::Slt, iv, n);
                self.b.brif(c, body, &[], exit, &[]);
                self.b.seal_block(body);
                self.b.seal_block(exit);
                self.b.switch_to_block(body);
                self.body(depth - 1);
                let iv = self.b.use_var(i);
                let one = self.b.iconst(Type::I32, 1);
                let ni = self.b.binary(BinaryOp::Iadd, iv, one);
                self.b.def_var(i, ni);
                self.b.jump(header, &[]);
                self.b.seal_block(header);
                self.b.switch_to_block(exit);
            }
            91..=94 if depth > 0 && budget => {
                let idx = self.expr(Type::I32, 1);
                let m = self.b.iconst(Type::I32, 3);
                let idx = self.b.binary(BinaryOp::Band, idx, m);
                let ts: Vec<Block> = (0..4).map(|_| self.b.create_block()).collect();
                let merge = self.b.create_block();
                let targets: Vec<(Block, Vec<Value>)> = ts[..3].iter().map(|&t| (t, vec![])).collect();
                self.b.br_table(idx, &targets, (ts[3], &[]));
                for &t in &ts {
                    self.b.seal_block(t);
                    self.b.switch_to_block(t);
                    self.body(depth - 1);
                    self.b.jump(merge, &[]);
                }
                self.b.seal_block(merge);
                self.b.switch_to_block(merge);
            }
            _ => {
                let t = self.r.pick(&TYPES);
                let e = self.expr(t, 3);
                let v = self.var(t);
                self.b.def_var(v, e);
            }
        }
    }
}

fn params() -> Vec<Type> {
    use Type::*;
    vec![I64, I64, I64, I32, I64, F32, F64]
}

fn random_program(seed: u64) -> Function {
    let mut f = Function::new(format!("rand{seed}"), Signature::new(params(), vec![Type::I64]));
    let mix = f.import_function(mix_sig(), 1);
    let many = f.import_function(many_sig(), 2);
    let mix_sig = f.import_signature(mix_sig());
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let ps = b.block_params(entry).to_vec();
    let mut g = Gen {
        b,
        r: Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1),
        vars: Vec::new(),
        mem: ps[1],
        fptr: ps[2],
        mix,
        many,
        mix_sig,
        stmts: 0,
    };
    for (k, &t) in TYPES.iter().enumerate() {
        for j in 0..5 {
            let v = g.b.declare_var(t);
            let init = if j == 0 { ps[3 + k] } else { g.konst(t) };
            g.b.def_var(v, init);
            g.vars.push((v, t));
        }
    }
    g.body(3);
    g.body(2);
    // Fold every variable into the result.
    let mut acc = g.b.iconst(Type::I64, 0);
    for (v, t) in g.vars.clone() {
        let x = g.b.use_var(v);
        let x = match t {
            Type::I32 => g.b.convert(ConvOp::Uext, Type::I64, x),
            Type::I64 => x,
            Type::F32 => {
                let c = g.canon(x, t);
                let i = g.b.convert(ConvOp::Bitcast, Type::I32, c);
                g.b.convert(ConvOp::Uext, Type::I64, i)
            }
            Type::F64 => {
                let c = g.canon(x, t);
                g.b.convert(ConvOp::Bitcast, Type::I64, c)
            }
        };
        let r = g.b.iconst(Type::I64, 7);
        let acc2 = g.b.binary(BinaryOp::Rotl, acc, r);
        acc = g.b.binary(BinaryOp::Bxor, acc2, x);
    }
    g.b.ret(&[acc]);
    g.b.finish();
    f
}

fn check(f: &Function, args: &[u64]) {
    crate::verify::verify(f).unwrap_or_else(|e| panic!("{e}\n{f}"));
    let mut seed_mem = vec![0u8; MEM_SIZE];
    let mut r = Rng(0x1234_5678);
    for b in seed_mem.iter_mut() {
        *b = r.next() as u8;
    }
    let mut env = TestEnv {
        mem: seed_mem.clone(),
    };
    let mut iargs = args.to_vec();
    iargs[1] = 0;
    let want = interp::run(f, &mut env, &iargs);
    if std::env::var("LUMEN_CODEGEN_STATS").is_ok() {
        let mut fx = f.clone();
        crate::legalize::split_critical_edges(&mut fx);
        let c = compile(f, &config()).unwrap();
        eprintln!(
            "STAT blocks={} code={} trap={:?}",
            f.layout.len(),
            c.code.len(),
            want.as_ref().err()
        );
    }

    let mut opt = f.clone();
    crate::opt::optimize(&mut opt);
    let mut baseline = config();
    baseline.features = Features {
        popcnt: true,
        sse41: true,
        ..Features::default()
    };
    let host = config();
    for (label, func, cfg) in [
        ("unoptimized", f, &host),
        ("optimized", &opt, &host),
        ("baseline ISA", &opt, &baseline),
    ] {
        let mut mem = seed_mem.clone();
        let mut nargs = args.to_vec();
        nargs[1] = mem.as_mut_ptr() as u64;
        let mut ctx = Ctx {
            entry_sp: 0,
            stack_limit: 0,
        };
        let got = run_native(func, cfg, &nargs, &mut ctx);
        assert_eq!(got, want, "{label} result, args {args:x?}\n{func}");
        assert!(mem == env.mem, "{label} memory differs, args {args:x?}\n{func}");
        assert_eq!(ctx.entry_sp, 0, "entry sp restored");
    }
}

fn arg_sets() -> Vec<Vec<u64>> {
    let fp = h_mix as *const () as u64;
    let f32b = |x: f32| x.to_bits() as u64;
    vec![
        vec![0, 0, fp, 5, 17, f32b(1.5), 2.25f64.to_bits()],
        vec![0, 0, fp, u32::MAX as u64, i64::MIN as u64, f32b(-0.0), f64::NAN.to_bits()],
        vec![0, 0, fp, 0x8000_0000, 0x7fff_ffff_ffff_ffff, f32b(f32::INFINITY), (-1e300f64).to_bits()],
        vec![0, 0, fp, 12345, 3, f32b(3e9), 4294967296.5f64.to_bits()],
    ]
}

#[test]
fn random_programs_match_the_interpreter() {
    let n = std::env::var("LUMEN_CODEGEN_SEEDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300u64);
    for seed in 1..=n {
        let f = random_program(seed);
        for args in arg_sets() {
            check(&f, &args);
        }
    }
}

/// Many values live at once across a call: spills, callee-saved registers, both classes.
#[test]
fn high_pressure_across_calls() {
    use Type::*;
    let mut f = Function::new("pressure", Signature::new(params(), vec![I64]));
    let many = f.import_function(many_sig(), 2);
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let ps = b.block_params(entry).to_vec();
    let mut ints = Vec::new();
    let mut floats = Vec::new();
    for k in 0..24 {
        let c = b.iconst(I64, k * 7919 + 1);
        ints.push(b.binary(BinaryOp::Imul, ps[4], c));
        let fc = b.f64const(k as f64 * 0.75);
        floats.push(b.binary(BinaryOp::Fadd, ps[6], fc));
    }
    let args: Vec<Value> = many_sig()
        .params
        .iter()
        .enumerate()
        .map(|(i, &t)| match t {
            I32 => ps[3],
            I64 => ints[i],
            F32 => ps[5],
            F64 => floats[i],
        })
        .collect();
    let r = b.call_fn(many, &args)[0];
    let mut acc = b.convert(ConvOp::ToSintSat, I64, r);
    for k in 0..24 {
        acc = b.binary(BinaryOp::Bxor, acc, ints[k]);
        let t = b.convert(ConvOp::ToSintSat, I64, floats[k]);
        acc = b.binary(BinaryOp::Iadd, acc, t);
    }
    b.ret(&[acc]);
    b.finish();
    for args in arg_sets() {
        let mut args = args;
        args[6] = 2.5f64.to_bits();
        check(&f, &args);
    }
}

#[test]
fn stack_limit_traps() {
    let mut f = Function::new("leaf", Signature::new(vec![Type::I64], vec![Type::I64]));
    let mut b = FunctionBuilder::new(&mut f);
    let entry = b.create_entry_block();
    let p = b.block_params(entry)[0];
    b.ret(&[p]);
    b.finish();
    let mut ctx = Ctx {
        entry_sp: 0,
        stack_limit: u64::MAX,
    };
    assert_eq!(run_native(&f, &config(), &[0], &mut ctx), Err(STACK_OVERFLOW));
    assert_eq!(ctx.entry_sp, 0);
    ctx.stack_limit = 0;
    let got = run_native(&f, &config(), &[0], &mut ctx).unwrap();
    assert_eq!(got, vec![&ctx as *const Ctx as u64]);
}
