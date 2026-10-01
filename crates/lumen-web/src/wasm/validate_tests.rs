use super::super::exec::{Host, Imports, MemEntity, Store, Val};
use super::super::parse::{decode, validate, ValType};
use crate::wasm::test_util::{self as t, I32, I64};

fn ok(bytes: &[u8]) {
    if let Err(e) = decode(bytes) {
        panic!("expected a valid module: {e}");
    }
}

fn invalid(bytes: &[u8]) {
    assert!(decode(bytes).is_err(), "expected an invalid module");
    assert!(!validate(bytes));
}

#[test]
fn operand_stack_underflow_is_a_compile_error() {
    // i32.add with an empty stack in a () -> i32 function.
    invalid(&t::func(&[], &[I32], &[0x6a]));
    invalid(&t::func(&[], &[I32], &[]));
    invalid(&t::func(&[], &[], &[0x1a])); // drop
    invalid(&t::func(&[], &[], &[0x41, 1, 0x04, 0x40, 0x1a, 0x0b])); // drop inside if
    invalid(&t::func(&[], &[I32], &[0x41, 0, 0x0e, 0, 0])); // br_table index only
    ok(&t::func(&[I32, I32], &[I32], &[0x20, 0, 0x20, 1, 0x6a]));
}

#[test]
fn stack_types_and_heights_are_checked() {
    invalid(&t::func(&[], &[I32], &[0x42, 1])); // i64 result for i32
    invalid(&t::func(&[], &[I32], &[0x41, 1, 0x42, 1, 0x6a])); // i32.add of i64
    invalid(&t::func(&[], &[], &[0x41, 1])); // value left over
    invalid(&t::func(&[], &[], &[0x02, 0x7f, 0x0b])); // block missing its result
    invalid(&t::func(&[], &[I32], &[0x41, 1, 0x04, 0x7f, 0x41, 2, 0x0b])); // if without else
    invalid(&t::func(&[], &[I32], &[0x41, 1, 0x41, 2, 0x42, 3, 0x1b])); // select of mixed types
    ok(&t::func(
        &[],
        &[I32],
        &[0x41, 1, 0x04, 0x7f, 0x41, 2, 0x05, 0x41, 3, 0x0b],
    ));
    // Unreachable code is stack-polymorphic.
    ok(&t::func(&[], &[I32], &[0x00, 0x6a]));
    ok(&t::func(
        &[],
        &[I32],
        &[0x02, 0x7f, 0x41, 1, 0x0c, 0, 0x1a, 0x1a, 0x0b],
    )); // after br
    invalid(&t::func(&[], &[I32], &[0x00, 0x42, 1, 0x6a])); // known i64 operand still checked
}

#[test]
fn control_structure_is_checked() {
    invalid(&t::func(&[], &[], &[0x02, 0x40])); // unterminated block
    invalid(&t::func(&[], &[], &[0x0b, 0x01])); // operators after the function's end
    invalid(&t::func(&[], &[], &[0x05])); // else without if
    invalid(&t::func(&[], &[], &[0x0c, 1])); // branch depth out of range
    invalid(&t::func(
        &[],
        &[],
        &[0x02, 0x7f, 0x41, 0, 0x0e, 1, 0, 1, 0x0b, 0x1a],
    )); // arity
    ok(&t::func(&[], &[], &[0x03, 0x40, 0x41, 0, 0x0d, 0, 0x0b])); // loop + br_if
    ok(&t::func(
        &[],
        &[],
        &[0x02, 0x40, 0x41, 0, 0x0e, 2, 0, 1, 0, 0x0b],
    ));
}

#[test]
fn indices_and_immediates_are_checked() {
    invalid(&t::func(&[], &[], &[0x20, 0, 0x1a])); // local out of range
    invalid(&t::func(&[], &[], &[0x10, 1])); // call out of range
    invalid(&t::func(&[], &[], &[0x23, 0, 0x1a])); // global out of range
    invalid(&t::func(&[], &[I32], &[0x41, 0, 0x28, 2, 0])); // load without memory
    invalid(&t::funcs(&[(&[], &[I32], &[0x41, 0, 0x28, 3, 0])], true)); // align > natural
    invalid(&t::funcs(&[(&[], &[I32], &[0x3f, 1])], true)); // memory.size reserved byte
    ok(&t::funcs(&[(&[], &[I32], &[0x41, 0, 0x28, 2, 0])], true));
    invalid(&t::func(&[], &[], &[0xff])); // unknown opcode
    invalid(&t::func(&[], &[], &[0xfc, 9, 0])); // data.drop without a data count
}

#[test]
fn module_level_references_are_checked() {
    let ty = || (1, t::vec(&[t::func_type(&[], &[])]));
    let f = || (3, vec![1, 0]);
    let code = || (10, t::vec(&[t::body(&[])]));
    // Export index out of range, duplicate export names.
    invalid(&t::module(&[
        ty(),
        f(),
        (7, t::vec(&[t::export("a", 0, 1)])),
        code(),
    ]));
    invalid(&t::module(&[
        ty(),
        f(),
        (7, t::vec(&[t::export("a", 0, 0), t::export("a", 0, 0)])),
        code(),
    ]));
    // Start function out of range / of the wrong type.
    invalid(&t::module(&[ty(), f(), (8, vec![1]), code()]));
    invalid(&t::module(&[
        (1, t::vec(&[t::func_type(&[I32], &[])])),
        f(),
        (8, vec![0]),
        code(),
    ]));
    // Element segment naming a missing function.
    let table = (4, vec![1, 0x70, 0, 1]);
    invalid(&t::module(&[
        ty(),
        f(),
        table.clone(),
        (9, vec![1, 0, 0x41, 0, 0x0b, 1, 5]),
        code(),
    ]));
    ok(&t::module(&[
        ty(),
        f(),
        table,
        (9, vec![1, 0, 0x41, 0, 0x0b, 1, 0]),
        code(),
    ]));
    // Global initializer of the wrong type; global.get of a later global.
    invalid(&t::module(&[(6, vec![1, I32, 0, 0x42, 0, 0x0b])]));
    invalid(&t::module(&[(
        6,
        vec![2, I32, 0, 0x23, 1, 0x0b, I32, 0, 0x41, 0, 0x0b],
    )]));
    // global.set of an immutable global.
    invalid(&t::module(&[
        ty(),
        f(),
        (6, vec![1, I32, 0, 0x41, 0, 0x0b]),
        (10, t::vec(&[t::body(&[0x41, 0, 0x24, 0])])),
    ]));
}

// ---- no input panics ---------------------------------------------------------------------------

struct NoHost;
impl Host for NoHost {
    fn call_host(
        &mut self,
        _: usize,
        _: &[Val],
        _: &[ValType],
        _: &mut [MemEntity],
    ) -> Result<Vec<Val>, String> {
        Err("no imports".into())
    }
}

/// xorshift64*: deterministic, no dependencies.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn seeds() -> Vec<Vec<u8>> {
    let table = (4, vec![1, 0x70, 0, 2]);
    let mut with_everything = t::module(&[
        (
            1,
            t::vec(&[t::func_type(&[I32], &[I32]), t::func_type(&[], &[])]),
        ),
        (3, vec![2, 0, 1]),
        table,
        (5, vec![1, 0, 1]),
        (6, vec![1, I64, 1, 0x42, 7, 0x0b]),
        (
            7,
            t::vec(&[
                t::export("f0", 0, 0),
                t::export("f1", 0, 1),
                t::export("m", 2, 0),
            ]),
        ),
        (9, vec![1, 0, 0x41, 0, 0x0b, 2, 0, 1]),
        (12, vec![1]),
        (
            10,
            t::vec(&[
                t::body(&[
                    0x02, 0x7f, 0x41, 5, 0x20, 0, 0x0e, 2, 0, 1, 0, 0x0b, // block (br_table)
                    0x41, 8, 0x28, 2, 0, 0x6a, // i32.load
                    0x41, 0, 0x41, 0, 0x41, 4, 0xfc, 10, 0, 0, // memory.copy
                    0x41, 0, 0x11, 1, 0, // call_indirect type 1
                    0x23, 0, 0xa7, 0x6a, // global.get, i32.wrap
                    0x41, 1, 0x40, 0, 0x1a, // memory.grow
                ]),
                t::body(&[0x41, 0, 0x41, 7, 0x41, 3, 0xfc, 11, 0]), // memory.fill
            ]),
        ),
        (11, vec![1, 0, 0x41, 4, 0x0b, 4, 1, 2, 3, 4]),
    ]);
    with_everything.extend(t::module(&[(0, vec![1, b'x', 9, 9])])[8..].iter());
    vec![
        with_everything,
        t::func(&[I32, I32], &[I32], &[0x20, 0, 0x20, 1, 0x6d]), // i32.div_s
        t::funcs(
            &[
                (&[I32], &[I64], &[0x20, 0, 0xac, 0x42, 3, 0x7e]),
                (
                    &[],
                    &[I32],
                    &[0x41, 0x7f, 0x04, 0x7f, 0x41, 1, 0x05, 0x41, 2, 0x0b],
                ),
            ],
            true,
        ),
    ]
}

type Body = (&'static [u8], &'static [u8], &'static [u8]);
const BODIES: &[Body] = &[
    (
        &[I32, I32],
        &[I32],
        &[0x20, 0, 0x20, 1, 0x6a, 0x41, 3, 0x6d],
    ),
    (
        &[I32],
        &[I32],
        &[0x02, 0x7f, 0x41, 1, 0x20, 0, 0x0e, 1, 0, 1, 0x0b],
    ),
    (
        &[I32],
        &[I32],
        &[
            0x20, 0, 0x04, 0x7f, 0x41, 1, 0x05, 0x41, 2, 0x0b, 0x41, 4, 0x6a,
        ],
    ),
    (
        &[I32],
        &[],
        &[0x20, 0, 0x41, 8, 0x36, 2, 0, 0x41, 4, 0x28, 2, 0, 0x1a],
    ),
    (
        &[],
        &[I32],
        &[0x41, 0, 0x41, 9, 0x41, 4, 0xfc, 11, 0, 0x41, 1, 0x40, 0],
    ),
    (
        &[],
        &[I32],
        &[0x41, 4, 0x41, 0, 0x41, 4, 0xfc, 10, 0, 0, 0x3f, 0],
    ),
    (
        &[I64],
        &[I32],
        &[
            0x20, 0, 0x42, 1, 0x7c, 0xa7, 0x41, 1, 0x41, 2, 0x20, 0, 0x50, 0x1b, 0x6a,
        ],
    ),
    (
        &[],
        &[I64],
        &[
            0x44, 0, 0, 0, 0, 0, 0, 0xf0, 0x3f, 0xb0, 0x43, 0, 0, 0x80, 0x3f, 0xfc, 5, 0x7c,
        ],
    ),
    (
        &[I32],
        &[I32],
        &[0x02, 0x40, 0x20, 0, 0x0d, 0, 0x41, 7, 0x0f, 0x0b, 0x41, 9],
    ),
];

fn mutate(rng: &mut Rng, m: &mut Vec<u8>) {
    for _ in 0..1 + rng.below(4) {
        if m.is_empty() {
            m.push(rng.next() as u8);
        }
        let i = rng.below(m.len());
        match rng.below(6) {
            0 => m[i] ^= 1 << rng.below(8),
            1 => m[i] = [0x00, 0x0b, 0x40, 0x7f, 0x80, 0xff][rng.below(6)],
            2 => m.insert(i, rng.next() as u8),
            3 => {
                m.remove(i);
            }
            4 => m.truncate(i),
            _ => m[i] = rng.next() as u8,
        }
    }
}

/// Decode, validate, translate, instantiate and run mutated modules: none may panic (release
/// builds abort on panic, so a panic on untrusted bytes would kill the process).
#[test]
fn mutated_modules_never_panic() {
    let seeds = seeds();
    for s in &seeds {
        ok(s);
    }
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let mut decoded = 0;
    for n in 0..5000 {
        let mut bytes = seeds[n % seeds.len()].clone();
        if n % 10 == 9 {
            bytes = (0..rng.below(64)).map(|_| rng.next() as u8).collect();
        } else if n % 2 == 0 {
            // Mutate one function's instructions only, keeping the module well-formed around
            // them, so most mutants reach validation and many run.
            let (p, r, code) = &BODIES[rng.below(BODIES.len())];
            let mut code = code.to_vec();
            mutate(&mut rng, &mut code);
            bytes = t::funcs(&[(p, r, &code)], true);
        } else {
            mutate(&mut rng, &mut bytes);
        }
        let r = std::panic::catch_unwind(|| {
            let Ok(m) = decode(&bytes) else { return false };
            for f in 0..m.func_types.len() as u32 {
                let _ = super::super::translate::translate(&m, m.imported_func_count + f);
            }
            // Loops and calls could run (practically) forever; run straight-line code only.
            let terminates = m
                .code
                .iter()
                .all(|b| !b.code.iter().any(|&c| matches!(c, 0x03 | 0x10 | 0x11)));
            let mut store = Store::default();
            let Ok(inst) = store.instantiate(m.clone(), Imports::default()) else {
                return true;
            };
            if terminates {
                for e in &m.exports {
                    let Some((super::super::ExportKind::Func, addr)) =
                        store.export_addr(inst, &e.name)
                    else {
                        continue;
                    };
                    let args = store.funcs[addr]
                        .ty()
                        .params
                        .iter()
                        .map(|&t| Val::default_for(t))
                        .collect();
                    let _ = store.invoke(addr, args, &mut NoHost, 0);
                }
            }
            true
        });
        match r {
            Ok(d) => decoded += d as usize,
            Err(_) => panic!("panic on input #{n}: {bytes:02x?}"),
        }
    }
    assert!(
        decoded > 200,
        "too few mutants decoded ({decoded}) to be meaningful"
    );
}
