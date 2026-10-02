//! `_opcode`: facts about CPython 3.12's instruction set (stack effects and which instructions
//! take names, constants, jump targets, ...) that `opcode` and `dis` build on. The numbers are
//! CPython's, not this VM's own instruction numbers: the VM has no `co_code`.

#[derive(Clone, Copy)]
enum Eff {
    Fixed(i64),
    Arg(fn(i64) -> i64),
    Jump { taken: i64, fallthrough: i64 },
}

const HAVE_ARGUMENT: i64 = 90;
const MIN_PSEUDO_OPCODE: i64 = 256;

const JUMPS: &[i64] = &[93, 110, 114, 115, 123, 128, 129, 134, 140, 260, 261];
const CONSTS: &[i64] = &[100, 121, 172];
const NAMES: &[i64] = &[90, 91, 95, 96, 97, 98, 101, 106, 108, 109, 116, 141, 175, 262, 263, 264, 265];
const LOCALS: &[i64] = &[124, 125, 126, 127, 143, 266];
const FREES: &[i64] = &[135, 136, 137, 138, 139, 148, 176];
const EXCS: &[i64] = &[256, 257, 258];

/// The stack effect of `op` in CPython 3.12; `None` for numbers that are no instruction.
fn lookup(op: i64) -> Option<Eff> {
    use Eff::*;
    Some(match op {
        0 | 9 | 11 | 12 | 15 | 17 | 36 | 37 | 50 | 68 | 69 | 75 | 85 | 91 | 98 | 99 | 110 | 121 | 126 | 131 | 134 | 135 | 139 | 140 | 144 | 149 | 150 | 151 | 172 | 173 | 175 | 176 => Fixed(0),
        1 | 3 | 5 | 25 | 83 | 89 | 90 | 96 | 97 | 107 | 108 | 114 | 115 | 117 | 118 | 119 | 122 | 125 | 128 | 129 | 138 | 145 | 146 | 162 | 163 | 164 | 165 | 174 => Fixed(-1),
        2 | 30 | 31 | 32 | 33 | 35 | 49 | 51 | 52 | 53 | 71 | 74 | 87 | 100 | 101 | 109 | 120 | 124 | 127 | 136 | 137 | 143 => Fixed(1),
        4 | 26 | 54 | 55 | 61 | 95 | 147 | 152 => Fixed(-2),
        27 => Fixed(-4),
        60 => Fixed(-3),
        92 => Arg(|a| a - 1),
        93 => Jump { taken: 1, fallthrough: 1 },
        94 => Arg(|a| (a & 0xff) + (a >> 8)),
        102..=104 => Arg(|a| 1 - a),
        105 => Arg(|a| 1 - 2 * a),
        106 => Arg(|a| a & 1),
        116 => Arg(|a| (a & 1) + 1),
        123 => Jump { taken: -1, fallthrough: 0 },
        130 => Arg(|a| -a),
        132 => Arg(|a| -((a & 1 != 0) as i64 + (a & 2 != 0) as i64 + (a & 4 != 0) as i64 + (a & 8 != 0) as i64)),
        133 => Arg(|a| if a == 3 { -2 } else { -1 }),
        141 => Arg(|a| -2 + (a & 1)),
        142 => Arg(|a| -2 - (a & 1)),
        155 => Arg(|a| if a & 4 == 4 { -1 } else { 0 }),
        156 => Arg(|a| -a),
        157 => Arg(|a| 1 - a),
        171 => Arg(|a| -1 - a),
        237 => lookup(141)?,
        238 => lookup(129)?,
        239 => lookup(128)?,
        240 => lookup(151)?,
        241 => lookup(171)?,
        242 => lookup(83)?,
        243 => lookup(150)?,
        244 => lookup(142)?,
        245 => lookup(110)?,
        246 => lookup(140)?,
        247 => lookup(121)?,
        248 => lookup(93)?,
        249 => lookup(114)?,
        250 => lookup(115)?,
        251 => lookup(4)?,
        252 => lookup(5)?,
        253 | 254 => Fixed(0),
        256 => Jump { taken: 1, fallthrough: 0 },
        257 => Jump { taken: 2, fallthrough: 0 },
        258 => Jump { taken: 1, fallthrough: 0 },
        259 | 260 | 261 => Fixed(0),
        262 => Fixed(1),
        263 | 264 => Fixed(-1),
        265 => Fixed(-2),
        266 => Fixed(-1),
        _ => return None,
    })
}

fn takes_arg(op: i64) -> bool {
    (HAVE_ARGUMENT..MIN_PSEUDO_OPCODE).contains(&op) || (256..=258).contains(&op)
}

/// Facts about the instruction set of CPython 3.12.
#[lumen_bind::module(name = "_opcode")]
pub mod _opcode {
    use super::*;
    use crate::object::*;
    use crate::vm::Interp;

    /// Compute the stack effect of the opcode.
    #[op(hint(py(text_signature = "($module, opcode, oparg=None, /, *, jump=None)")))]
    fn stack_effect(it: &mut Interp, opcode: i64, oparg: Option<&Value>, #[kwonly] jump: Option<&Value>) -> R<i64> {
        let oparg = oparg.filter(|v| !v.is_none());
        let Some(eff) = lookup(opcode) else {
            return Err(it.value_error("invalid opcode or oparg"));
        };
        let arg = if takes_arg(opcode) {
            let Some(v) = oparg else {
                return Err(it.value_error("stack_effect: opcode requires oparg but oparg was not specified"));
            };
            it.index_of(v)?
        } else {
            if oparg.is_some() {
                return Err(it.value_error("stack_effect: opcode does not permit oparg but oparg was specified"));
            }
            0
        };
        let jump = match jump.filter(|v| !v.is_none()) {
            Some(v) => Some(it.truthy(v)?),
            None => None,
        };
        Ok(match eff {
            Eff::Fixed(n) => n,
            Eff::Arg(f) => f(arg),
            Eff::Jump { taken, fallthrough } => match jump {
                Some(true) => taken,
                Some(false) => fallthrough,
                None => taken.max(fallthrough),
            },
        })
    }

    /// Return True if the opcode takes an argument.
    #[op]
    fn has_arg(opcode: i64) -> bool {
        lookup(opcode).is_some() && takes_arg(opcode)
    }

    /// Return True if the opcode accesses a constant.
    #[op]
    fn has_const(opcode: i64) -> bool {
        CONSTS.contains(&opcode)
    }

    /// Return True if the opcode accesses an attribute or a global/builtin name.
    #[op]
    fn has_name(opcode: i64) -> bool {
        NAMES.contains(&opcode)
    }

    /// Return True if the opcode has a jump target.
    #[op]
    fn has_jump(opcode: i64) -> bool {
        JUMPS.contains(&opcode)
    }

    /// Return True if the opcode accesses a free variable.
    #[op]
    fn has_free(opcode: i64) -> bool {
        FREES.contains(&opcode)
    }

    /// Return True if the opcode accesses a local variable.
    #[op]
    fn has_local(opcode: i64) -> bool {
        LOCALS.contains(&opcode)
    }

    /// Return True if the opcode sets up an exception handler.
    #[op]
    fn has_exc(opcode: i64) -> bool {
        EXCS.contains(&opcode)
    }

    /// Return the specialization statistics, or None when the interpreter keeps none.
    #[op]
    fn get_specialization_stats() -> Value {
        Value::None
    }
}
