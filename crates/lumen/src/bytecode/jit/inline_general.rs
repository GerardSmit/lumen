//! General inlining: a direct call site (see [`super::call`]) whose callee is a small plain
//! function translates the callee's bytecode in place of the call of its code.
//!
//! # What stays the same
//!
//! The site keeps everything a direct call does except the call itself: the callee identity
//! guard (or the same-chunk rebind), the depth / GC-poll / shadow-stack guards, the engine
//! flags, the call record linked into `Interp::jit_frames`, and the callee's [`JitFrame`] on
//! the shadow stack with the arguments seeded into its slots. Every helper the inlined body
//! calls therefore sees a real callee frame (chunk, constants, environment, `this`), stack
//! traces and `f.caller` see the callee's record, and the GC sees the frame's values as for any
//! direct call. Anything a guard rejects takes the ordinary [`Helper::Call`] path.
//!
//! # The body
//!
//! Instead of entering the callee's code, the translator swaps its per-chunk state
//! ([`ChunkCtx`]) for the callee's — its chunk, analysis, plan, slot kinds and SSA variables —
//! with the callee frame as `frame` / `slots` / `stack`, and translates the callee as
//! whole-function code entered at pc 0. Parameters kept in SSA take the argument entries
//! directly (an unboxed argument stays unboxed; a Boxed one is checked before the frame is
//! built, a mismatch taking the ordinary path); locals start `undefined` as at any entry.
//! A `Return` stores the result in the callee frame's `stack[0]` and joins the code after the
//! call, which completes it exactly as a return exit of the callee's code would (result moved
//! to the caller's stack, slots dropped, record popped, flags restored).
//!
//! # Exits
//!
//! An exit inside the body writes the callee state back into the callee frame (SSA slots, the
//! operand stack, `exit_depth`) and goes to [`Helper::InlineFinish`] — [`Helper::CallFinish`]
//! for a frame the interpreter finishes from the exit pc — after which the caller continues
//! after the call with the result, as when a called callee's code exits. A throw propagates
//! like a callee throw. The interpreter never needs more than the one callee frame, which
//! exists.
//!
//! # Deopt accounting
//!
//! Exits of an inlined body and argument-check misses are counted per call site (in the
//! caller chunk's site feedback, see [`super::super::inline_note`]); past a limit the site is
//! marked not to inline and the running code is retired, so the recompile makes the ordinary
//! direct call. Exits at a `throw` and helper throws are not speculation failures and are not
//! counted.
//!
//! # Limits
//!
//! Callees with an activation environment, `arguments` / rest parameters, handlers,
//! generators / async, `super`, `new.target`, tail calls, block scopes or direct `eval` are not
//! inlined, nor constructors, adaptors, recursive calls (of any function already being inlined
//! into the unit) or callees past [`max_ops`] ops; bodies nest at most [`MAX_DEPTH`] deep and a
//! unit inlines at most [`MAX_UNIT_OPS`] ops. `LUMEN_JIT_INLINE=0` turns it off and
//! `LUMEN_JIT_INLINE_MAX` sets the per-callee op limit.

use super::call::Adaptor;
use super::*;
use std::rc::Rc;
use std::sync::OnceLock;

/// Inlined bodies nest at most this deep (a body inlined into an inlined body is depth 2).
const MAX_DEPTH: usize = 2;

/// Ops of inlined bodies per compiled unit.
const MAX_UNIT_OPS: usize = 640;

/// Whether general inlining is on (`LUMEN_JIT_INLINE`, default on).
pub(super) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("LUMEN_JIT_INLINE").map_or(true, |v| v != "0"))
}

/// Ops beyond which a callee is not inlined (`LUMEN_JIT_INLINE_MAX`, default 96).
fn max_ops() -> usize {
    static MAX: OnceLock<usize> = OnceLock::new();
    *MAX.get_or_init(|| {
        std::env::var("LUMEN_JIT_INLINE_MAX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(96)
    })
}

/// Where an inlined body's control leaves it: `exit` (one I64 parameter, the exit word) for an
/// exit, `ret` (one I32 parameter, status 0) for a return with the result in the callee
/// frame's `stack[0]`.
#[derive(Clone, Copy)]
pub(super) struct InlCx {
    pub exit: Block,
    pub ret: Block,
}

/// The general-inlining state of a compiled unit, shared by its own code and every body
/// inlined into it.
pub(super) struct Shared<'a> {
    interp: &'a Interp,
    root: &'a Chunk,
    on: bool,
    /// Call sites inlined so far.
    pub inlined: u32,
    ops: usize,
    /// The chunks being translated, the unit's own first (recursion is never inlined).
    chain: Vec<usize>,
    /// Frame-size constants of inlined calls and their sizes (known once a body is done).
    pub sizes: Vec<(V, i64)>,
    /// What the inlined bodies' plans pin, keep and import (see [`Built`]): kept with the code.
    pins: Vec<Value>,
    boxes: Vec<Box<dyn std::any::Any>>,
    externs: Vec<u64>,
}

impl<'a> Shared<'a> {
    /// `on`: inline at all. `externs`: the unit's own plan's.
    pub fn new(interp: &'a Interp, root: &'a Chunk, on: bool, externs: Vec<u64>) -> Self {
        Shared {
            interp,
            root,
            on,
            inlined: 0,
            ops: 0,
            chain: vec![root as *const Chunk as usize],
            sizes: Vec::new(),
            pins: Vec::new(),
            boxes: Vec::new(),
            externs,
        }
    }

    /// Hand what the unit keeps to its [`Built`] (the externs replace the unit plan's, which
    /// they extend) and count the inlined sites.
    pub fn finish(
        self,
        pins: &mut Vec<Value>,
        boxes: &mut Vec<Box<dyn std::any::Any>>,
        externs: &mut Vec<u64>,
    ) {
        pins.extend(self.pins);
        boxes.extend(self.boxes);
        *externs = self.externs;
        if self.inlined > 0 {
            let mut stats = self.interp.jit_stats.get();
            stats.inlined_calls += u64::from(self.inlined);
            self.interp.jit_stats.set(stats);
        }
    }
}

/// A callee planned for inlining at one call site (see [`Tr::inline_plan`]).
pub(super) struct InlBody {
    chunk: Rc<Chunk>,
    an: Analysis,
    kinds: Vec<Kind>,
    fresh: Vec<bool>,
    defd: Defd,
    tdzd: Defd,
    plan: Plan,
    ta_kinds: Vec<u32>,
    ta_slots: Vec<bool>,
    /// The address of the site's [`InlineSite`] (kept in the unit's boxes).
    info: usize,
}

/// Whether nothing in `cc` keeps it from being inlined (see the module docs).
fn body_ok(cc: &Chunk) -> bool {
    if cc.activation_layout.is_some()
        || cc.makes_env()
        || cc.arguments_slot.is_some()
        || cc.rest_slot.is_some()
        || cc.virt_base.is_some()
        || cc.derived
        || !cc.var_force_resets.is_empty()
    {
        return false;
    }
    let name = |n: u32| cc.names.get(n as usize).map(|x| &**x);
    cc.ops.iter().all(|op| match *op {
        Op::LoadName(n, _) | Op::LoadNameForCall(n, _) | Op::TypeofName(n) => {
            !matches!(name(n), Some("eval" | "arguments"))
        }
        Op::PushHandler(_)
        | Op::PopHandler
        | Op::Await
        | Op::Yield
        | Op::YieldDelegate(_)
        | Op::InitialYield
        | Op::TailCall(..)
        | Op::TailCallSpread(..)
        | Op::SuperGet(_)
        | Op::SuperGetElem
        | Op::SuperBase
        | Op::SuperMethod(..)
        | Op::SuperMethodElem(_)
        | Op::SuperCtor
        | Op::SuperCall(_)
        | Op::SuperCallSpread(_)
        | Op::DerivedReturn
        | Op::LoadCallee
        | Op::LoadNewTarget
        | Op::LoadLexicalThis
        | Op::ArgsLen(..)
        | Op::ArgsGet(..)
        | Op::ApplyArgs(..)
        | Op::BlkNew(..)
        | Op::BlkDecl(..)
        | Op::BlkCopy(_)
        | Op::BlkLoad(..)
        | Op::BlkStore(..)
        | Op::BlkInit(..)
        | Op::BlkUpdate(..)
        | Op::InEnv(_)
        | Op::ForInKeys
        | Op::ForInStepL(..)
        | Op::MakeClass(..)
        | Op::ImportCall(..)
        | Op::GetAsyncIter
        | Op::AsyncIterNext(..)
        | Op::AsyncIterResult(_)
        | Op::AsyncCloseCall(..)
        | Op::AsyncCloseCheck(_)
        | Op::AsyncDelegateInit
        | Op::AsyncDelegateCall(..)
        | Op::AsyncDelegateResult(..)
        | Op::AsyncDelegateCloseReject(..)
        | Op::AsyncDelegateSpecial(..) => false,
        _ => true,
    })
}

impl<'a> Tr<'a, '_> {
    /// The callee of the direct site at `pc` (a `Call` / `CallWithThis` of `argc` arguments with
    /// its operands on the stack), planned for inlining with the current argument entries —
    /// when the site and the callee qualify (see the module docs).
    pub(super) fn inline_plan(&mut self, pc: usize, argc: usize, with_this: bool) -> Option<InlBody> {
        if !self.ginl.on || self.ginl.chain.len() > MAX_DEPTH {
            return None;
        }
        let site = self.plan.direct.get(&pc)?.clone();
        if site.word == 0
            || site.construct
            || site.self_call
            || site.tail
            || site.tails
            || site.act
            // (The flag never clears: the site's guard would always take the ordinary call.)
            || (site.reflect && crate::bytecode::reflect::enabled())
            || site.rest.is_some()
            || site.virt.is_some()
            || site.adaptor != Adaptor::None
            || !matches!(self.ops[pc], Op::Call(_) | Op::CallWithThis(_))
            || self.chunk.jit.fb_get::<NoInline>(pc).is_some()
        {
            return None;
        }
        // SAFETY: `pin` is the address of the site's pinned callee `Value` (kept in the plan's
        // boxes, so alive for the compile).
        let callee = unsafe { &*(site.pin as *const Value) };
        let interp = self.ginl.interp;
        let ic = crate::bytecode::inline_callee(interp, callee)?;
        let cc = ic.chunk.clone();
        let cp = Rc::as_ptr(&cc) as usize;
        let n = cc.ops.len();
        if cp != site.chunk
            || n == 0
            || n > max_ops()
            || self.ginl.ops + n > MAX_UNIT_OPS
            || self.ginl.chain.contains(&cp)
            || cc.jit.fstate.get() == FS_DEAD
            || !body_ok(&cc)
        {
            return None;
        }
        let d = self.stack.len();
        let wt = with_this as usize;
        let base = d.checked_sub(argc + 1 + wt)?;
        let ab = base + wt + 1;
        if matches!(self.stack[base + wt], Entry::Num(_) | Entry::Bool(_))
            || (with_this
                && site.uses_this
                && matches!(self.stack[base], Entry::Num(_) | Entry::Bool(_)))
        {
            return None;
        }
        // The parameters' kinds: an unboxed argument's own; a Boxed one's what the callee's
        // function code was specialized on (checked before the call); never a widened slot's.
        let (widen, not_num, fed) = {
            let fs = cc.jit.func.try_borrow().ok()?;
            (
                fs.widen.clone(),
                fs.not_num.clone(),
                fs.code.as_ref().map(|c| c.kinds.clone()),
            )
        };
        let mut slots = vec![Value::Undefined; cc.n_slots];
        for k in 0..cc.n_params.min(argc).min(cc.n_slots) {
            if widen.get(k).is_some_and(|w| *w != Widen::None) {
                continue;
            }
            slots[k] = match self.stack[ab + k] {
                Entry::Num(_) => Value::Num(0.5),
                Entry::Bool(_) => Value::Bool(false),
                Entry::Boxed | Entry::Ref(..) => match fed.as_ref().and_then(|f| f.get(k)) {
                    Some(Kind::Num | Kind::Int32) => Value::Num(0.5),
                    Some(Kind::Bool) => Value::Bool(false),
                    _ => Value::Undefined,
                },
            };
        }
        let an = analyze(&cc, 0, n - 1, true).ok()?;
        if an.hs.iter().any(|h| !h.is_empty()) || !an.resumes.is_empty() {
            return None;
        }
        let (mut kinds, fresh) = fn_kinds(&cc, &an, &slots, &widen, &not_num, false);
        int32_kinds(&cc, &slots, &mut kinds, &fresh, &widen);
        if kinds
            .iter()
            .take(cc.n_params)
            .any(|&k| k == Kind::Int32)
        {
            return None;
        }
        let (defd, tdzd, mut plan) = plan_unit(
            interp,
            &ic.env,
            &cc,
            0,
            n - 1,
            &slots,
            &Value::Undefined,
            &an,
            &kinds,
            &fresh,
            true,
        );
        // The body's recursive calls call the callee's own code, not this unit's.
        for s in plan.direct.values_mut() {
            s.self_call = false;
        }
        plan.tail_loops.clear();
        let (ta_kinds, ta_slots) = ta_slot_info(interp, &cc, 0, n - 1, &slots, true);
        // What the plan keeps goes with the unit; its imports join the unit's.
        let g = &mut self.ginl;
        g.pins.append(&mut plan.pins);
        g.boxes.append(&mut plan.boxes);
        for f in &mut plan.fast {
            let addr = plan.externs[f.ext];
            f.ext = match g.externs.iter().position(|&a| a == addr) {
                Some(k) => k,
                None => {
                    g.externs.push(addr);
                    g.externs.len() - 1
                }
            };
        }
        plan.externs.clear();
        let info = Box::new(InlineSite {
            root: g.root as *const Chunk,
            entry: self.self_entry,
            chunk: self.chunk as *const Chunk,
            site: pc,
        });
        let info_addr = &*info as *const InlineSite as usize;
        g.boxes.push(info);
        g.boxes.push(Box::new(cc.clone()));
        g.ops += n;
        if log_enabled() {
            eprintln!("[jit] inlining a {n}-op callee at {pc}");
        }
        Some(InlBody {
            chunk: cc,
            an,
            kinds,
            fresh,
            defd,
            tdzd,
            plan,
            ta_kinds,
            ta_slots,
            info: info_addr,
        })
    }

    /// Before an inlined call commits: branch to `slow` unless each Boxed argument of a
    /// parameter the body keeps in SSA has the parameter's kind (a miss is counted against the
    /// site, see [`super::super::inline_note`]).
    pub(super) fn inl_arg_guards(
        &mut self,
        b: &InlBody,
        entries: &[Entry],
        ab: usize,
        nargs: usize,
        slow: Block,
    ) {
        let mut miss = None;
        for k in 0..nargs.min(b.chunk.n_params) {
            let tag = match b.kinds[k] {
                Kind::Num | Kind::Int32 => TAG_NUM,
                Kind::Bool => TAG_BOOL,
                Kind::Boxed => continue,
            };
            let t = match entries[ab + k] {
                Entry::Num(_) | Entry::Bool(_) => continue,
                Entry::Boxed => self.stack_tag(ab + k),
                Entry::Ref(p, _) => self.fb.load(MemKind::I32U8, p, 0),
            };
            let want = self.i32c(tag as i64);
            let ok = self.fb.icmp(IntCC::Eq, t, want);
            let m = *miss.get_or_insert_with(|| self.fb.create_block());
            self.guard_to(ok, m);
        }
        if let Some(m) = miss {
            let cont = self.fb.current_block().expect("in a block");
            self.fb.seal_block(m);
            self.fb.switch_to_block(m);
            self.fb.mark_cold();
            let ip = self.ptrc(b.info as i64);
            self.call(Helper::InlineMiss, &[ip]);
            self.fb.jump(slow, &[]);
            self.fb.switch_to_block(cont);
        }
    }

    /// The inlined body of `b` on the committed callee frame `nf` (its slots seeded from
    /// `args`, the call's argument entries up to the parameters; `size` its frame-size
    /// constant): returns and exits continue at `post` with the call's status, the result in
    /// the callee frame's `stack[0]` (see the module docs).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn inline_run(
        &mut self,
        b: InlBody,
        nf: V,
        slots_p: V,
        stack_p: V,
        args: &[Entry],
        size: V,
        post: Block,
    ) {
        let InlBody {
            chunk: cc,
            an,
            kinds,
            fresh,
            defd,
            tdzd,
            plan,
            ta_kinds,
            ta_slots,
            info,
        } = b;
        // SAFETY: the unit's boxes hold the chunk for the code's lifetime (see `inline_plan`).
        let chunk: &'a Chunk = unsafe { &*Rc::as_ptr(&cc) };
        let n_slots = chunk.n_slots;
        let exit = self.fb.create_block();
        let word = self.fb.append_block_param(exit, Type::I64);
        let consts = self.ptrc(chunk.consts.as_ptr() as i64);
        self.flush_seals();
        let leaders = leader_blocks(&mut self.fb, &an);
        let mut cx = ChunkCtx {
            num_exits: Vec::new(),
            exit_tails: [None, None],
            chunk,
            ops: &chunk.ops,
            header: 0,
            backedge: chunk.ops.len() - 1,
            frame: nf,
            slots: slots_p,
            stackp: stack_p,
            consts,
            kinds,
            vars: vec![None; n_slots],
            tdz: vec![None; n_slots],
            undef: vec![None; n_slots],
            fresh,
            defd,
            tdzd,
            func_mode: true,
            tdz_slots: tdz_slot_set(chunk),
            leaders,
            stack: Vec::new(),
            max_stack: 1,
            plan,
            ptr_vars: HashMap::new(),
            arr_vars: HashMap::new(),
            idx_vars: HashMap::new(),
            math_vars: HashMap::new(),
            ta_vars: HashMap::new(),
            ta_slots,
            ta_kinds,
            ta_len_vars: HashMap::new(),
            dv_vars: HashMap::new(),
            bl_vars: HashMap::new(),
            push_vars: HashMap::new(),
            chain_vars: HashMap::new(),
            handlers: false,
            hs_cur: Vec::new(),
            skip: None,
            pending_append: None,
            chaining: false,
            hsets: Vec::new(),
            call_flags: None,
            inl: Some(InlCx { exit, ret: post }),
        };
        self.swap_ctx(&mut cx);
        self.ginl.chain.push(chunk as *const Chunk as usize);
        let r = self.inl_entry(&an, args).and_then(|()| self.body(&an));
        if let Err(e) = r {
            self.err.get_or_insert(e);
        }
        self.flush_seals();
        self.ginl.chain.pop();
        let max_stack = self.max_stack;
        self.swap_ctx(&mut cx);
        self.ginl
            .sizes
            .push((size, frame_bytes(n_slots, max_stack) as i64));
        self.ginl.inlined += 1;
        // ---- an exit: the interpreter finishes the callee frame ----
        self.fb.seal_block(exit);
        self.fb.switch_to_block(exit);
        self.fb.mark_cold();
        let ip = self.ptrc(info as i64);
        let st = self
            .call(Helper::InlineFinish, &[self.frame, nf, word, ip])
            .expect("InlineFinish returns a status");
        self.fb.jump(post, &[st]);
    }

    /// The entry of an inlined body (its context swapped in): the SSA slots from the argument
    /// entries (parameters) or `undefined` (locals), the region caches, then pc 0. A failing
    /// entry-time guard exits at pc 0, where the interpreter runs the callee from its start.
    fn inl_entry(&mut self, an: &Analysis, args: &[Entry]) -> Result<(), String> {
        self.declare_ssa(an);
        for s in 0..self.kinds.len() {
            let Some(var) = self.vars[s] else { continue };
            if let Some(f) = self.tdz[s] {
                let z = self.i32c(0);
                self.fb.def_var(f, z);
            }
            let off = s as i32 * VALUE_SIZE;
            if s < self.chunk.n_params {
                let v = match (args.get(s), self.kinds[s]) {
                    (Some(Entry::Num(x)), Kind::Num) => *x,
                    (Some(Entry::Bool(b)), Kind::Bool) => *b,
                    // Checked before the call (`inl_arg_guards`): the seeded slot holds one.
                    (Some(Entry::Boxed | Entry::Ref(..)), Kind::Num) => {
                        self.fb
                            .load(MemKind::F64, self.slots, off + VALUE_PAYLOAD)
                    }
                    (Some(Entry::Boxed | Entry::Ref(..)), Kind::Bool) => {
                        self.fb
                            .load(MemKind::I32U8, self.slots, off + VALUE_BOOL)
                    }
                    (_, k) => return Err(format!("inlined parameter {s} kept {k:?}")),
                };
                self.fb.def_var(var, v);
            } else if self.fresh[s] {
                let z = if self.kinds[s] == Kind::Int32 {
                    self.i32c(0)
                } else {
                    self.fb.f64const(0.0)
                };
                self.fb.def_var(var, z);
                let one = self.i32c(1);
                self.fb.def_var(self.undef[s].expect("declared"), one);
            } else {
                return Err(format!("inlined local {s} has no entry value"));
            }
        }
        let fail = self.fb.create_block();
        self.entry_caches(fail);
        let start = self.edge(false, 0)?;
        self.fb.jump(start, &[]);
        self.fb.seal_block(fail);
        self.fb.switch_to_block(fail);
        self.emit_exit(0, EXIT_RESUME, &[], 0, None);
        Ok(())
    }

    /// A `Return` (`Some(entry)`, the operand stack's one entry) or `ReturnUndef` of an inlined
    /// body: the result into the callee frame's `stack[0]`, then the code after the call.
    pub(super) fn inl_return(&mut self, cx: InlCx, e: Option<Entry>) {
        match e {
            Some(e) => self.store_entry(0, e),
            None => self.set_stack_tag(0, TAG_UNDEFINED),
        }
        let z = self.i32c(STATUS_OK as i64);
        self.fb.jump(cx.ret, &[z]);
    }
}
