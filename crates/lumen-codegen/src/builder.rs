//! Building IR: an insertion cursor plus SSA construction for mutable variables.
//!
//! Front ends describe locals as [`Variable`]s and call [`FunctionBuilder::def_var`] /
//! [`FunctionBuilder::use_var`]; the builder turns them into SSA values and block parameters
//! with the algorithm of Braun et al., "Simple and Efficient Construction of Static Single
//! Assignment Form" (CC 2013). A block must be *sealed* once all its predecessors are known;
//! reads in unsealed blocks create provisional parameters that are completed on sealing.
//! Parameters that turn out to be trivial (every incoming argument is the same value) are left
//! in place and removed by [`crate::opt::simplify_params`].

use crate::ir::*;

/// A front-end mutable variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Variable(pub u32);

struct BlockState {
    sealed: bool,
    /// Whether the block is in the layout yet (`switch_to_block` appends it once).
    laid_out: bool,
    /// Predecessor edges as (block, terminator); one entry per terminator even if it targets
    /// this block twice.
    preds: Vec<(Block, Inst)>,
    /// Current definition of each variable at the end of the block (as far as built).
    defs: Vec<Option<Value>>,
    /// Parameters created while unsealed, completed by `seal_block`.
    incomplete: Vec<(Variable, Value)>,
    /// Parameters the front end declared itself (these come first).
    explicit_params: usize,
}

impl BlockState {
    fn new() -> BlockState {
        BlockState {
            sealed: false,
            laid_out: false,
            preds: Vec::new(),
            defs: Vec::new(),
            incomplete: Vec::new(),
            explicit_params: 0,
        }
    }
}

pub struct FunctionBuilder<'a> {
    pub func: &'a mut Function,
    cur: Option<Block>,
    var_types: Vec<Type>,
    blocks: Vec<BlockState>,
}

impl<'a> FunctionBuilder<'a> {
    pub fn new(func: &'a mut Function) -> FunctionBuilder<'a> {
        let mut blocks = Vec::new();
        blocks.resize_with(func.blocks.len(), BlockState::new);
        FunctionBuilder {
            func,
            cur: None,
            var_types: Vec::new(),
            blocks,
        }
    }

    // ----- blocks ----------------------------------------------------------------------------

    pub fn create_block(&mut self) -> Block {
        let b = self.func.create_block();
        self.blocks.push(BlockState::new());
        b
    }

    /// Create the entry block with the signature's parameters and switch to it.
    pub fn create_entry_block(&mut self) -> Block {
        let b = self.create_block();
        for ty in self.func.sig.params.clone() {
            self.append_block_param(b, ty);
        }
        self.switch_to_block(b);
        self.seal_block(b);
        b
    }

    /// Declare a parameter on `block`. All explicit parameters must be declared before any
    /// branch to the block is emitted.
    pub fn append_block_param(&mut self, block: Block, ty: Type) -> Value {
        let st = &mut self.blocks[block.index()];
        debug_assert!(st.preds.is_empty(), "explicit parameter added after a branch");
        debug_assert_eq!(
            st.explicit_params,
            self.func.blocks[block.index()].params.len(),
            "explicit parameter added after an SSA parameter"
        );
        st.explicit_params += 1;
        self.func.append_block_param(block, ty)
    }

    pub fn block_params(&self, block: Block) -> &[Value] {
        &self.func.blocks[block.index()].params
    }

    /// Continue emitting into `block` (appended to the layout on first use).
    pub fn switch_to_block(&mut self, block: Block) {
        let st = &mut self.blocks[block.index()];
        if !st.laid_out {
            st.laid_out = true;
            self.func.layout.push(block);
        }
        self.cur = Some(block);
    }

    pub fn current_block(&self) -> Option<Block> {
        self.cur
    }

    /// Whether the current block already ends in a terminator.
    pub fn is_filled(&self) -> bool {
        match self.cur {
            Some(b) => self.func.terminator(b).is_some(),
            None => true,
        }
    }

    /// Declare that every predecessor of `block` has been emitted.
    pub fn seal_block(&mut self, block: Block) {
        if self.blocks[block.index()].sealed {
            return;
        }
        let incomplete = std::mem::take(&mut self.blocks[block.index()].incomplete);
        for (var, param) in incomplete {
            self.add_param_args(block, var, param);
        }
        self.blocks[block.index()].sealed = true;
    }

    pub fn seal_all_blocks(&mut self) {
        for i in 0..self.blocks.len() {
            self.seal_block(Block(i as u32));
        }
    }

    // ----- variables -------------------------------------------------------------------------

    pub fn declare_var(&mut self, ty: Type) -> Variable {
        self.var_types.push(ty);
        Variable(self.var_types.len() as u32 - 1)
    }

    pub fn var_type(&self, var: Variable) -> Type {
        self.var_types[var.0 as usize]
    }

    pub fn def_var(&mut self, var: Variable, val: Value) {
        let block = self.cur.expect("no current block");
        debug_assert_eq!(self.func.value_type(val), self.var_type(var), "{var:?} type");
        self.set_def(block, var, val);
    }

    pub fn use_var(&mut self, var: Variable) -> Value {
        let block = self.cur.expect("no current block");
        self.read(block, var)
    }

    fn set_def(&mut self, block: Block, var: Variable, val: Value) {
        let defs = &mut self.blocks[block.index()].defs;
        let i = var.0 as usize;
        if defs.len() <= i {
            defs.resize(i + 1, None);
        }
        defs[i] = Some(val);
    }

    fn local_def(&self, block: Block, var: Variable) -> Option<Value> {
        self.blocks[block.index()]
            .defs
            .get(var.0 as usize)
            .copied()
            .flatten()
    }

    fn read(&mut self, mut block: Block, var: Variable) -> Value {
        // Walk single-predecessor chains iteratively; record the result on every block visited.
        let mut visited = Vec::new();
        let val = loop {
            if let Some(v) = self.local_def(block, var) {
                break v;
            }
            let st = &self.blocks[block.index()];
            if !st.sealed {
                let p = self.func.append_block_param(block, self.var_type(var));
                self.blocks[block.index()].incomplete.push((var, p));
                self.set_def(block, var, p);
                break p;
            }
            match st.preds.len() {
                0 => {
                    // Unreachable (or the entry without a definition): any value will do.
                    let v = self.zero_at_entry(self.var_type(var));
                    self.set_def(block, var, v);
                    break v;
                }
                1 => {
                    visited.push(block);
                    block = st.preds[0].0;
                }
                _ => {
                    let p = self.func.append_block_param(block, self.var_type(var));
                    self.set_def(block, var, p);
                    self.add_param_args(block, var, p);
                    break p;
                }
            }
        };
        for b in visited {
            self.set_def(b, var, val);
        }
        val
    }

    /// A zero constant placed at the start of the entry block (dominating every use).
    fn zero_at_entry(&mut self, ty: Type) -> Value {
        let data = match ty {
            Type::I32 | Type::I64 => InstData::Iconst { ty, imm: 0 },
            Type::F32 => InstData::F32const { bits: 0 },
            Type::F64 => InstData::F64const { bits: 0 },
        };
        let inst = self.func.make_inst(data);
        let entry = self.func.entry();
        self.func.blocks[entry.index()].insts.insert(0, inst);
        self.func.results(inst)[0]
    }

    /// Append the argument for SSA parameter `param` (for `var`) to every predecessor's branch.
    fn add_param_args(&mut self, block: Block, var: Variable, param: Value) {
        let preds = self.blocks[block.index()].preds.clone();
        for (pred, term) in preds {
            let arg = self.read(pred, var);
            for call in self.func.insts[term.index()].successors_mut() {
                if call.block == block {
                    call.args.push(arg);
                }
            }
        }
        let _ = param;
    }

    // ----- instructions ----------------------------------------------------------------------

    fn push(&mut self, data: InstData) -> Inst {
        let block = self.cur.expect("no current block");
        debug_assert!(
            self.func.terminator(block).is_none(),
            "emitting into a filled block {block}"
        );
        let term = data.is_terminator();
        let succs: Vec<Block> = data.successors().iter().map(|c| c.block).collect();
        let inst = self.func.make_inst(data);
        self.func.blocks[block.index()].insts.push(inst);
        if term {
            let mut seen = Vec::new();
            for s in succs {
                if seen.contains(&s) {
                    continue;
                }
                seen.push(s);
                debug_assert!(!self.blocks[s.index()].sealed, "branch to sealed {s}");
                self.blocks[s.index()].preds.push((block, inst));
            }
        }
        inst
    }

    fn push1(&mut self, data: InstData) -> Value {
        let inst = self.push(data);
        self.func.results(inst)[0]
    }

    /// Branch arguments for `block`'s explicit parameters; SSA parameters are appended later.
    fn call(&self, block: Block, args: &[Value]) -> BlockCall {
        let st = &self.blocks[block.index()];
        debug_assert_eq!(args.len(), st.explicit_params, "argument count for {block}");
        // SSA parameters already created on an unsealed block are completed on sealing, which
        // also covers this new edge; on a sealed block no new edges may appear.
        BlockCall {
            block,
            args: args.to_vec(),
        }
    }

    pub fn iconst(&mut self, ty: Type, imm: i64) -> Value {
        debug_assert!(ty.is_int());
        let imm = if ty == Type::I32 { imm as i32 as i64 } else { imm };
        self.push1(InstData::Iconst { ty, imm })
    }
    pub fn f32const(&mut self, v: f32) -> Value {
        self.push1(InstData::F32const { bits: v.to_bits() })
    }
    pub fn f64const(&mut self, v: f64) -> Value {
        self.push1(InstData::F64const { bits: v.to_bits() })
    }
    pub fn f32const_bits(&mut self, bits: u32) -> Value {
        self.push1(InstData::F32const { bits })
    }
    pub fn f64const_bits(&mut self, bits: u64) -> Value {
        self.push1(InstData::F64const { bits })
    }
    pub fn unary(&mut self, op: UnaryOp, arg: Value) -> Value {
        self.push1(InstData::Unary { op, arg })
    }
    pub fn binary(&mut self, op: BinaryOp, a: Value, b: Value) -> Value {
        debug_assert_eq!(self.func.value_type(a), self.func.value_type(b), "{op:?}");
        self.push1(InstData::Binary { op, args: [a, b] })
    }
    pub fn icmp(&mut self, cc: IntCC, a: Value, b: Value) -> Value {
        self.push1(InstData::IntCmp { cc, args: [a, b] })
    }
    pub fn fcmp(&mut self, cc: FloatCC, a: Value, b: Value) -> Value {
        self.push1(InstData::FloatCmp { cc, args: [a, b] })
    }
    pub fn select(&mut self, cond: Value, if_true: Value, if_false: Value) -> Value {
        self.push1(InstData::Select {
            cond,
            if_true,
            if_false,
        })
    }
    pub fn convert(&mut self, op: ConvOp, to: Type, arg: Value) -> Value {
        self.push1(InstData::Convert { op, to, arg })
    }
    pub fn load(&mut self, kind: MemKind, addr: Value, offset: i32) -> Value {
        self.push1(InstData::Load { kind, addr, offset })
    }
    pub fn store(&mut self, kind: MemKind, addr: Value, value: Value, offset: i32) {
        self.push(InstData::Store {
            kind,
            addr,
            value,
            offset,
        });
    }
    pub fn call_fn(&mut self, func: FuncRef, args: &[Value]) -> Vec<Value> {
        let inst = self.push(InstData::Call {
            func,
            args: args.to_vec(),
        });
        self.func.results(inst).to_vec()
    }
    pub fn call_indirect(&mut self, sig: SigRef, callee: Value, args: &[Value]) -> Vec<Value> {
        let inst = self.push(InstData::CallIndirect {
            sig,
            callee,
            args: args.to_vec(),
        });
        self.func.results(inst).to_vec()
    }
    pub fn trap(&mut self, code: u32) {
        self.push(InstData::Trap { code });
    }
    pub fn trap_if(&mut self, cond: Value, code: u32) {
        self.push(InstData::TrapIf { cond, code });
    }
    pub fn jump(&mut self, block: Block, args: &[Value]) {
        let dest = self.call(block, args);
        self.push(InstData::Jump { dest });
    }
    pub fn brif(
        &mut self,
        cond: Value,
        then: Block,
        then_args: &[Value],
        else_: Block,
        else_args: &[Value],
    ) {
        let then = self.call(then, then_args);
        let else_ = self.call(else_, else_args);
        self.push(InstData::Brif { cond, then, else_ });
    }
    pub fn br_table(&mut self, index: Value, targets: &[(Block, Vec<Value>)], default: (Block, &[Value])) {
        let targets = targets.iter().map(|(b, a)| self.call(*b, a)).collect();
        let default = self.call(default.0, default.1);
        self.push(InstData::BrTable {
            index,
            targets,
            default,
        });
    }
    pub fn ret(&mut self, args: &[Value]) {
        self.push(InstData::Return {
            args: args.to_vec(),
        });
    }

    /// Seal every block and check that every block in the layout is terminated.
    pub fn finish(mut self) {
        self.seal_all_blocks();
        for &b in &self.func.layout {
            assert!(
                self.func.terminator(b).is_some(),
                "{} ends without a terminator",
                b
            );
        }
    }
}
