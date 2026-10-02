//! `_opcode`: facts about CPython 3.14's instruction set (opcode validity, which instructions take
//! names, constants, jump targets, ..., and stack effects) that `opcode` and `dis` build on. The
//! numbers are CPython's, not this VM's own: the VM has no `co_code`, so nothing here describes
//! code that actually runs.

/// `base + per_arg * oparg + odd * (oparg & 1)`.
#[derive(Clone, Copy)]
struct E(i32, i32, i32);

impl E {
    fn at(self, arg: i32) -> i32 {
        self.0 + self.1 * arg + self.2 * (arg & 1)
    }
}

const ARG: u8 = 1;
const CONST: u8 = 2;
const NAME: u8 = 4;
const JUMP: u8 = 8;
const FREE: u8 = 16;
const LOCAL: u8 = 32;
const EXC: u8 = 64;

/// Stack effect of a specialized instruction: it has none.
const NONE: E = E(i32::MIN, 0, 0);

/// `(opcode, flags, effect when the jump is taken, effect when it is not)` for every valid opcode,
/// generated from CPython 3.14's `_opcode` (`is_valid`, `has_*`, `stack_effect`).
const OPS: &[(i32, u8, E, E)] = &[
    (0, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (1, 0x00, E(-2, 0, 0), E(-2, 0, 0)),
    (2, 0x00, E(-1, 0, 0), E(-1, 0, 0)),
    (3, 0x20, NONE, NONE),
    (4, 0x00, E(-3, 0, 0), E(-3, 0, 0)),
    (5, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (6, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (7, 0x00, E(-1, 0, 0), E(-1, 0, 0)),
    (8, 0x00, E(-2, 0, 0), E(-2, 0, 0)),
    (9, 0x00, E(-1, 0, 0), E(-1, 0, 0)),
    (10, 0x00, E(-1, 0, 0), E(-1, 0, 0)),
    (11, 0x00, E(-1, 0, 0), E(-1, 0, 0)),
    (12, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (13, 0x00, E(-1, 0, 0), E(-1, 0, 0)),
    (14, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (15, 0x00, E(1, 0, 0), E(1, 0, 0)),
    (16, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (17, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (18, 0x00, E(1, 0, 0), E(1, 0, 0)),
    (19, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (20, 0x00, E(-1, 0, 0), E(-1, 0, 0)),
    (21, 0x00, E(1, 0, 0), E(1, 0, 0)),
    (22, 0x00, E(1, 0, 0), E(1, 0, 0)),
    (23, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (24, 0x00, E(1, 0, 0), E(1, 0, 0)),
    (25, 0x00, E(1, 0, 0), E(1, 0, 0)),
    (26, 0x00, E(1, 0, 0), E(1, 0, 0)),
    (27, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (28, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (29, 0x00, E(-1, 0, 0), E(-1, 0, 0)),
    (30, 0x00, E(-1, 0, 0), E(-1, 0, 0)),
    (31, 0x00, E(-1, 0, 0), E(-1, 0, 0)),
    (32, 0x00, E(1, 0, 0), E(1, 0, 0)),
    (33, 0x00, E(1, 0, 0), E(1, 0, 0)),
    (34, 0x00, E(1, 0, 0), E(1, 0, 0)),
    (35, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (36, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (37, 0x00, E(-4, 0, 0), E(-4, 0, 0)),
    (38, 0x00, E(-3, 0, 0), E(-3, 0, 0)),
    (39, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (40, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (41, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (42, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (43, 0x00, E(1, 0, 0), E(1, 0, 0)),
    (44, 0x01, E(-1, 0, 0), E(-1, 0, 0)),
    (45, 0x01, E(-1, 0, -1), E(-1, 0, -1)),
    (46, 0x01, E(1, -1, 0), E(1, -1, 0)),
    (47, 0x01, E(1, -2, 0), E(1, -2, 0)),
    (48, 0x01, E(1, -1, 0), E(1, -1, 0)),
    (49, 0x01, E(1, -1, 0), E(1, -1, 0)),
    (50, 0x01, E(1, -1, 0), E(1, -1, 0)),
    (51, 0x01, E(1, -1, 0), E(1, -1, 0)),
    (52, 0x01, E(-1, -1, 0), E(-1, -1, 0)),
    (53, 0x01, E(0, 0, 0), E(0, 0, 0)),
    (54, 0x01, E(-1, 0, 0), E(-1, 0, 0)),
    (55, 0x01, E(-2, -1, 0), E(-2, -1, 0)),
    (56, 0x01, E(-1, 0, 0), E(-1, 0, 0)),
    (57, 0x01, E(-1, 0, 0), E(-1, 0, 0)),
    (58, 0x01, E(0, 0, 0), E(0, 0, 0)),
    (59, 0x01, E(1, 0, 0), E(1, 0, 0)),
    (60, 0x01, E(0, 0, 0), E(0, 0, 0)),
    (61, 0x05, E(-1, 0, 0), E(-1, 0, 0)),
    (62, 0x11, E(0, 0, 0), E(0, 0, 0)),
    (63, 0x21, E(0, 0, 0), E(0, 0, 0)),
    (64, 0x05, E(0, 0, 0), E(0, 0, 0)),
    (65, 0x05, E(0, 0, 0), E(0, 0, 0)),
    (66, 0x01, E(-1, 0, 0), E(-1, 0, 0)),
    (67, 0x01, E(-1, 0, 0), E(-1, 0, 0)),
    (68, 0x09, E(-2, 0, 0), E(-2, 0, 0)),
    (69, 0x01, E(0, 0, 0), E(0, 0, 0)),
    (70, 0x09, E(1, 0, 0), E(1, 0, 0)),
    (71, 0x01, E(0, 0, 0), E(0, 0, 0)),
    (72, 0x05, E(1, 0, 0), E(1, 0, 0)),
    (73, 0x05, E(-1, 0, 0), E(-1, 0, 0)),
    (74, 0x01, E(-1, 0, 0), E(-1, 0, 0)),
    (75, 0x09, E(0, 0, 0), E(0, 0, 0)),
    (76, 0x09, E(0, 0, 0), E(0, 0, 0)),
    (77, 0x09, E(0, 0, 0), E(0, 0, 0)),
    (78, 0x01, E(-1, 0, 0), E(-1, 0, 0)),
    (79, 0x01, E(-1, 0, 0), E(-1, 0, 0)),
    (80, 0x05, E(0, 0, 1), E(0, 0, 1)),
    (81, 0x01, E(1, 0, 0), E(1, 0, 0)),
    (82, 0x03, E(1, 0, 0), E(1, 0, 0)),
    (83, 0x21, E(1, 0, 0), E(1, 0, 0)),
    (84, 0x21, E(1, 0, 0), E(1, 0, 0)),
    (85, 0x21, E(1, 0, 0), E(1, 0, 0)),
    (86, 0x21, E(1, 0, 0), E(1, 0, 0)),
    (87, 0x21, E(2, 0, 0), E(2, 0, 0)),
    (88, 0x21, E(1, 0, 0), E(1, 0, 0)),
    (89, 0x21, E(2, 0, 0), E(2, 0, 0)),
    (90, 0x11, E(0, 0, 0), E(0, 0, 0)),
    (91, 0x05, E(0, 0, 0), E(0, 0, 0)),
    (92, 0x05, E(1, 0, 1), E(1, 0, 1)),
    (93, 0x05, E(1, 0, 0), E(1, 0, 0)),
    (94, 0x01, E(1, 0, 0), E(1, 0, 0)),
    (95, 0x01, E(1, 0, 0), E(1, 0, 0)),
    (96, 0x05, E(-2, 0, 1), E(-2, 0, 1)),
    (97, 0x11, E(0, 0, 0), E(0, 0, 0)),
    (98, 0x01, E(-2, 0, 0), E(-2, 0, 0)),
    (99, 0x01, E(-2, 0, 0), E(-2, 0, 0)),
    (100, 0x09, E(-1, 0, 0), E(-1, 0, 0)),
    (101, 0x09, E(-1, 0, 0), E(-1, 0, 0)),
    (102, 0x09, E(-1, 0, 0), E(-1, 0, 0)),
    (103, 0x09, E(-1, 0, 0), E(-1, 0, 0)),
    (104, 0x01, E(0, -1, 0), E(0, -1, 0)),
    (105, 0x01, E(-1, 0, 0), E(-1, 0, 0)),
    (106, 0x09, E(0, 0, 0), E(0, 0, 0)),
    (107, 0x01, E(-1, 0, 0), E(-1, 0, 0)),
    (108, 0x01, E(-1, 0, 0), E(-1, 0, 0)),
    (109, 0x01, E(-1, 0, 0), E(-1, 0, 0)),
    (110, 0x05, E(-2, 0, 0), E(-2, 0, 0)),
    (111, 0x11, E(-1, 0, 0), E(-1, 0, 0)),
    (112, 0x21, E(-1, 0, 0), E(-1, 0, 0)),
    (113, 0x21, E(0, 0, 0), E(0, 0, 0)),
    (114, 0x21, E(-2, 0, 0), E(-2, 0, 0)),
    (115, 0x05, E(-1, 0, 0), E(-1, 0, 0)),
    (116, 0x05, E(-1, 0, 0), E(-1, 0, 0)),
    (117, 0x01, E(0, 0, 0), E(0, 0, 0)),
    (118, 0x01, E(0, 1, 0), E(0, 1, 0)),
    (119, 0x01, E(-1, 1, 0), E(-1, 1, 0)),
    (120, 0x01, E(0, 0, 0), E(0, 0, 0)),
    (128, 0x01, E(0, 0, 0), E(0, 0, 0)),
    (129, 0x00, NONE, NONE),
    (130, 0x00, NONE, NONE),
    (131, 0x00, NONE, NONE),
    (132, 0x00, NONE, NONE),
    (133, 0x00, NONE, NONE),
    (134, 0x00, NONE, NONE),
    (135, 0x00, NONE, NONE),
    (136, 0x00, NONE, NONE),
    (137, 0x00, NONE, NONE),
    (138, 0x00, NONE, NONE),
    (139, 0x00, NONE, NONE),
    (140, 0x00, NONE, NONE),
    (141, 0x00, NONE, NONE),
    (142, 0x00, NONE, NONE),
    (143, 0x01, NONE, NONE),
    (144, 0x01, NONE, NONE),
    (145, 0x01, NONE, NONE),
    (146, 0x01, NONE, NONE),
    (147, 0x01, NONE, NONE),
    (148, 0x01, NONE, NONE),
    (149, 0x01, NONE, NONE),
    (150, 0x01, NONE, NONE),
    (151, 0x01, NONE, NONE),
    (152, 0x01, NONE, NONE),
    (153, 0x01, NONE, NONE),
    (154, 0x00, NONE, NONE),
    (155, 0x01, NONE, NONE),
    (156, 0x01, NONE, NONE),
    (157, 0x01, NONE, NONE),
    (158, 0x01, NONE, NONE),
    (159, 0x01, NONE, NONE),
    (160, 0x01, NONE, NONE),
    (161, 0x01, NONE, NONE),
    (162, 0x01, NONE, NONE),
    (163, 0x01, NONE, NONE),
    (164, 0x01, NONE, NONE),
    (165, 0x01, NONE, NONE),
    (166, 0x01, NONE, NONE),
    (167, 0x01, NONE, NONE),
    (168, 0x01, NONE, NONE),
    (169, 0x01, NONE, NONE),
    (170, 0x01, NONE, NONE),
    (171, 0x01, NONE, NONE),
    (172, 0x09, NONE, NONE),
    (173, 0x09, NONE, NONE),
    (174, 0x09, NONE, NONE),
    (175, 0x09, NONE, NONE),
    (176, 0x09, NONE, NONE),
    (177, 0x01, NONE, NONE),
    (178, 0x01, NONE, NONE),
    (179, 0x05, NONE, NONE),
    (180, 0x01, NONE, NONE),
    (181, 0x01, NONE, NONE),
    (182, 0x01, NONE, NONE),
    (183, 0x01, NONE, NONE),
    (184, 0x01, NONE, NONE),
    (185, 0x01, NONE, NONE),
    (186, 0x01, NONE, NONE),
    (187, 0x01, NONE, NONE),
    (188, 0x01, NONE, NONE),
    (189, 0x05, NONE, NONE),
    (190, 0x03, NONE, NONE),
    (191, 0x03, NONE, NONE),
    (192, 0x01, NONE, NONE),
    (193, 0x01, NONE, NONE),
    (194, 0x05, NONE, NONE),
    (195, 0x05, NONE, NONE),
    (196, 0x00, NONE, NONE),
    (197, 0x01, NONE, NONE),
    (198, 0x00, NONE, NONE),
    (199, 0x00, NONE, NONE),
    (200, 0x05, NONE, NONE),
    (201, 0x00, NONE, NONE),
    (202, 0x00, NONE, NONE),
    (203, 0x00, NONE, NONE),
    (204, 0x00, NONE, NONE),
    (205, 0x00, NONE, NONE),
    (206, 0x00, NONE, NONE),
    (207, 0x00, NONE, NONE),
    (208, 0x00, NONE, NONE),
    (209, 0x01, NONE, NONE),
    (210, 0x01, NONE, NONE),
    (211, 0x01, NONE, NONE),
    (234, 0x00, E(-1, 0, 0), E(-1, 0, 0)),
    (235, 0x00, E(-1, 0, 0), E(-1, 0, 0)),
    (236, 0x00, E(-1, 0, 0), E(-1, 0, 0)),
    (237, 0x09, E(1, 0, 0), E(1, 0, 0)),
    (238, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (239, 0x01, E(0, 0, 0), E(0, 0, 0)),
    (240, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (241, 0x01, E(-1, 0, 0), E(-1, 0, 0)),
    (242, 0x01, E(-1, 0, 0), E(-1, 0, 0)),
    (243, 0x01, E(-1, 0, 0), E(-1, 0, 0)),
    (244, 0x01, E(-1, 0, 0), E(-1, 0, 0)),
    (245, 0x01, E(0, 0, 0), E(0, 0, 0)),
    (246, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (247, 0x01, E(0, 0, 0), E(0, 0, 0)),
    (248, 0x09, E(-2, 0, 0), E(-2, 0, 0)),
    (249, 0x05, E(-2, 0, 1), E(-2, 0, 1)),
    (250, 0x01, E(-1, -1, 0), E(-1, -1, 0)),
    (251, 0x01, E(-2, -1, 0), E(-2, -1, 0)),
    (252, 0x00, E(-3, 0, 0), E(-3, 0, 0)),
    (253, 0x01, E(0, 0, 0), E(0, 0, 0)),
    (254, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (255, 0x01, E(0, 0, 0), E(0, 0, 0)),
    (256, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (257, 0x09, E(0, 0, 0), E(0, 0, 0)),
    (258, 0x09, E(0, 0, 0), E(0, 0, 0)),
    (259, 0x09, E(0, 0, 0), E(0, 0, 0)),
    (260, 0x09, E(0, 0, 0), E(0, 0, 0)),
    (261, 0x21, E(1, 0, 0), E(1, 0, 0)),
    (262, 0x00, E(0, 0, 0), E(0, 0, 0)),
    (263, 0x41, E(2, 0, 0), E(0, 0, 0)),
    (264, 0x41, E(1, 0, 0), E(0, 0, 0)),
    (265, 0x41, E(1, 0, 0), E(0, 0, 0)),
    (266, 0x21, E(-1, 0, 0), E(-1, 0, 0)),
];

fn info(op: i32) -> Option<&'static (i32, u8, E, E)> {
    OPS.binary_search_by_key(&op, |r| r.0).ok().map(|i| &OPS[i])
}

fn flag(op: i32, f: u8) -> bool {
    info(op).is_some_and(|r| r.1 & f != 0)
}

/// Facts about the instruction set of CPython 3.14.
#[lumen_bind::module(name = "_opcode")]
pub mod _opcode {
    use super::*;
    use crate::object::*;
    use crate::vm::Interp;

    #[constant(name = "ENABLE_SPECIALIZATION")]
    const ENABLE_SPECIALIZATION: bool = false;
    #[constant(name = "ENABLE_SPECIALIZATION_FT")]
    const ENABLE_SPECIALIZATION_FT: bool = false;

    /// Compute the stack effect of the opcode.
    #[op(hint(py(text_signature = "($module, opcode, oparg=None, /, *, jump=None)")))]
    fn stack_effect(it: &mut Interp, opcode: i32, oparg: Option<&Value>, #[kwonly] jump: Option<&Value>) -> R<i32> {
        let arg = match oparg.filter(|v| !v.is_none()) {
            Some(v) => it.index_of(v)? as i32,
            None => 0,
        };
        let jump = match jump {
            None | Some(Value::None) => None,
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => return Err(it.value_error("stack_effect: jump must be False, True or None")),
        };
        let Some(&(_, _, taken, fall)) = info(opcode).filter(|r| r.2 .0 != NONE.0) else {
            return Err(it.value_error("invalid opcode or oparg"));
        };
        Ok(match jump {
            Some(true) => taken.at(arg),
            Some(false) => fall.at(arg),
            None => taken.at(arg).max(fall.at(arg)),
        })
    }

    /// Return True if opcode is valid, False otherwise.
    #[op]
    fn is_valid(#[kw] opcode: i32) -> bool {
        info(opcode).is_some()
    }

    /// Return True if the opcode uses its oparg, False otherwise.
    #[op]
    fn has_arg(#[kw] opcode: i32) -> bool {
        flag(opcode, ARG)
    }

    /// Return True if the opcode accesses a constant, False otherwise.
    #[op]
    fn has_const(#[kw] opcode: i32) -> bool {
        flag(opcode, CONST)
    }

    /// Return True if the opcode accesses an attribute by name, False otherwise.
    #[op]
    fn has_name(#[kw] opcode: i32) -> bool {
        flag(opcode, NAME)
    }

    /// Return True if the opcode has a jump target, False otherwise.
    #[op]
    fn has_jump(#[kw] opcode: i32) -> bool {
        flag(opcode, JUMP)
    }

    /// Return True if the opcode accesses a free variable, False otherwise.
    ///
    /// Note that 'free' in this context refers to names in the current scope
    /// that are referenced by inner scopes or names in outer scopes that are
    /// referenced from this scope. It does not include references to global
    /// or builtin scopes.
    #[op]
    fn has_free(#[kw] opcode: i32) -> bool {
        flag(opcode, FREE)
    }

    /// Return True if the opcode accesses a local variable, False otherwise.
    #[op]
    fn has_local(#[kw] opcode: i32) -> bool {
        flag(opcode, LOCAL)
    }

    /// Return True if the opcode sets an exception handler, False otherwise.
    #[op]
    fn has_exc(#[kw] opcode: i32) -> bool {
        flag(opcode, EXC)
    }

    /// Return the specialization stats
    #[op]
    fn get_specialization_stats() -> Value {
        Value::None
    }

    /// Return array of symbols of binary ops.
    ///
    /// Indexed by the BINARY_OP oparg value.
    #[op]
    fn get_nb_ops() -> Value {
        const NB: &[(&str, &str)] = &[
            ("NB_ADD", "+"), ("NB_AND", "&"), ("NB_FLOOR_DIVIDE", "//"), ("NB_LSHIFT", "<<"),
            ("NB_MATRIX_MULTIPLY", "@"), ("NB_MULTIPLY", "*"), ("NB_REMAINDER", "%"), ("NB_OR", "|"),
            ("NB_POWER", "**"), ("NB_RSHIFT", ">>"), ("NB_SUBTRACT", "-"), ("NB_TRUE_DIVIDE", "/"),
            ("NB_XOR", "^"), ("NB_INPLACE_ADD", "+="), ("NB_INPLACE_AND", "&="),
            ("NB_INPLACE_FLOOR_DIVIDE", "//="), ("NB_INPLACE_LSHIFT", "<<="),
            ("NB_INPLACE_MATRIX_MULTIPLY", "@="), ("NB_INPLACE_MULTIPLY", "*="),
            ("NB_INPLACE_REMAINDER", "%="), ("NB_INPLACE_OR", "|="), ("NB_INPLACE_POWER", "**="),
            ("NB_INPLACE_RSHIFT", ">>="), ("NB_INPLACE_SUBTRACT", "-="), ("NB_INPLACE_TRUE_DIVIDE", "/="),
            ("NB_INPLACE_XOR", "^="), ("NB_SUBSCR", "[]"),
        ];
        Value::list(NB.iter().map(|&(a, b)| Value::tuple(vec![Value::str(a), Value::str(b)])).collect())
    }

    fn names(v: &[&str]) -> Value {
        Value::list(v.iter().map(|s| Value::str(s)).collect())
    }

    /// Return a list of names of the unary intrinsics.
    #[op]
    fn get_intrinsic1_descs() -> Value {
        names(&[
            "INTRINSIC_1_INVALID", "INTRINSIC_PRINT", "INTRINSIC_IMPORT_STAR", "INTRINSIC_STOPITERATION_ERROR",
            "INTRINSIC_ASYNC_GEN_WRAP", "INTRINSIC_UNARY_POSITIVE", "INTRINSIC_LIST_TO_TUPLE", "INTRINSIC_TYPEVAR",
            "INTRINSIC_PARAMSPEC", "INTRINSIC_TYPEVARTUPLE", "INTRINSIC_SUBSCRIPT_GENERIC", "INTRINSIC_TYPEALIAS",
        ])
    }

    /// Return a list of names of the binary intrinsics.
    #[op]
    fn get_intrinsic2_descs() -> Value {
        names(&[
            "INTRINSIC_2_INVALID", "INTRINSIC_PREP_RERAISE_STAR", "INTRINSIC_TYPEVAR_WITH_BOUND",
            "INTRINSIC_TYPEVAR_WITH_CONSTRAINTS", "INTRINSIC_SET_FUNCTION_TYPE_PARAMS", "INTRINSIC_SET_TYPEPARAM_DEFAULT",
        ])
    }

    /// Return a list of special method names.
    #[op]
    fn get_special_method_names() -> Value {
        names(&["__enter__", "__exit__", "__aenter__", "__aexit__"])
    }

    /// Return the executor object at offset in code if exists, None otherwise.
    #[op]
    fn get_executor(it: &mut Interp, #[kw] code: &Value, #[kw] _offset: i32) -> R<Value> {
        if !matches!(code, Value::Obj(o) if matches!(o.kind, Kind::Code(_))) {
            let t = it.type_name_of(code);
            return Err(it.type_error(&format!("expected a code object, not '{t}'")));
        }
        Err(it.runtime_error("Executors are not available in this build"))
    }
}
