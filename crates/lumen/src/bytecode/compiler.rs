// ---------------------------------------------------------------------------------------------
// Capture analysis
// ---------------------------------------------------------------------------------------------

/// Which names the body's *inner functions* can resolve to the outer function's locals — the set
/// that must live in a real activation environment instead of VM slots. Also whether any inner
/// arrow chain reads the outer `this`.
///
/// Soundness rule: a name wrongly treated as local to an inner function would silently resolve
/// past the activation to the wrong binding, so everything not fully understood returns `None`
/// (direct eval, `with`, sloppy block function declarations, module syntax, …) and the caller
/// bails to the tree-walker.
#[derive(Clone)]
struct CaptureScan {
    /// Declared-name scopes, innermost last, each tagged with the function-nesting depth it
    /// belongs to (0 = the function being compiled) and its push serial.
    scopes: Vec<(std::collections::HashSet<String>, u32, u32)>,
    fn_depth: u32,
    /// Names resolving from depth > 0 to a depth-0 scope.
    captured: std::collections::HashSet<String>,
    /// Names declared by a depth-0 scope that is NOT the function's top scope (block lexicals,
    /// for-head lexicals, catch params). If one of these is captured, per-block binding
    /// freshness matters — unless the name qualifies for activation homing (see
    /// `homable_inner_lets`), the caller bails.
    depth0_inner_decls: std::collections::HashSet<String>,
    /// Activation-homing candidates: a plain `let` declared by a once-per-call depth-0 scope
    /// (a block/switch outside every loop — freshness never matters) with no ENCLOSING
    /// declaration of the same name (an enclosing slot would wrongly shadow the env binding
    /// inside the block), keyed name → declaring scope serial. Same-name declarations nested
    /// INSIDE the candidate's scope are fine (their slots shadow the env binding correctly);
    /// any other same-name declaration poisons the entry. At the end a candidate homes only
    /// if every capture of the name resolved through ITS scope (see `captured_serials`) and
    /// the name was never a free/global reference.
    candidates: std::collections::HashMap<String, u32>,
    /// Names that ever entered (or were disqualified from) candidacy — a second same-name
    /// once-per-call `let` cannot home (both would map to ONE activation binding).
    ever_candidates: std::collections::HashSet<String>,
    /// For candidate names: the scope serials their captures resolved through.
    captured_serials: std::collections::HashMap<String, std::collections::HashSet<u32>>,
    /// Candidates declared by `const` (homed as immutable activation bindings).
    const_candidates: std::collections::HashSet<String>,
    /// Names referenced somewhere they did NOT resolve to a scope (free/global uses).
    free_refs: std::collections::HashSet<String>,
    /// A named function expression's self-name, and whether anything in the body (nested
    /// functions included, shadowed or not) references that name: the tail-call guard (see
    /// [`self_tail`]) applies only to a function that can name itself.
    self_name: Option<String>,
    self_named: bool,
    /// Innermost loop nesting at the current walk position (for-head scopes and every scope
    /// pushed inside a loop body are never homable).
    loop_depth: u32,
    /// Scope-push counter (the serial stored per scope for capture attribution).
    next_serial: u32,
    /// Whether `this` is read from an inner arrow chain rooted at the outer function.
    env_this: bool,
    /// Arrow-ness of each enclosing function on the current path (index 0 = the outer function).
    arrow_path: Vec<bool>,
}

/// Collect every binding a `Pattern` introduces.
fn pat_idents(p: &Pattern, out: &mut std::collections::HashSet<String>) {
    match p {
        Pattern::Ident(n) => {
            out.insert(n.clone());
        }
        Pattern::Array(elems) => {
            for e in elems {
                match e {
                    ArrayPatElem::Hole => {}
                    ArrayPatElem::Elem { pattern, .. } => pat_idents(pattern, out),
                    ArrayPatElem::Rest(p) => pat_idents(p, out),
                }
            }
        }
        Pattern::Object(o) => {
            for pr in &o.props {
                pat_idents(&pr.value, out);
            }
            if let Some(r) = &o.rest {
                out.insert(r.clone());
            }
        }
        Pattern::Member(_) => {}
    }
}

/// Collect the function-scoped `var` names (and direct top-level function-declaration names) of a
/// body: recurses through blocks/loops/switch/try but never into nested functions or classes.
/// `top` distinguishes direct body statements (whose FuncDecls hoist) from block-level ones.
/// Returns false on a construct whose hoisting we don't model (sloppy Annex B block functions).
fn hoisted_vars(
    stmts: &[Stmt],
    top: bool,
    strict: bool,
    out: &mut std::collections::HashSet<String>,
) -> bool {
    for s in stmts {
        if !hoisted_vars_stmt(s, top, strict, out) {
            return false;
        }
    }
    true
}

fn hoisted_vars_stmt(
    s: &Stmt,
    top: bool,
    strict: bool,
    out: &mut std::collections::HashSet<String>,
) -> bool {
    match s {
        Stmt::VarDecl {
            kind: DeclKind::Var,
            decls,
        } => {
            for (p, _) in decls {
                pat_idents(p, out);
            }
            true
        }
        Stmt::FuncDecl(f) => {
            if top {
                if let Some(n) = &f.name {
                    out.insert(n.clone());
                }
                true
            } else {
                // Block-level function declaration: strict = block-scoped lexical (handled by the
                // block scope in the walker); sloppy = Annex B promotion we don't model — bail.
                strict
            }
        }
        Stmt::Block(b) => hoisted_vars(b, false, strict, out),
        Stmt::If { cons, alt, .. } => {
            hoisted_vars_stmt(cons, false, strict, out)
                && alt
                    .as_deref()
                    .map(|a| hoisted_vars_stmt(a, false, strict, out))
                    .unwrap_or(true)
        }
        Stmt::While { body, .. } | Stmt::DoWhile { body, .. } | Stmt::Labeled { body, .. } => {
            hoisted_vars_stmt(body, false, strict, out)
        }
        Stmt::For { init, body, .. } => {
            if let Some(ForInit::VarDecl {
                kind: DeclKind::Var,
                decls,
            }) = init.as_deref()
            {
                for (p, _) in decls {
                    pat_idents(p, out);
                }
            }
            hoisted_vars_stmt(body, false, strict, out)
        }
        Stmt::ForInOf {
            decl, left, body, ..
        } => {
            if matches!(decl, Some(DeclKind::Var)) {
                pat_idents(left, out);
            }
            hoisted_vars_stmt(body, false, strict, out)
        }
        Stmt::Try {
            block,
            handler,
            finalizer,
        } => {
            hoisted_vars(block, false, strict, out)
                && handler
                    .as_ref()
                    .map(|h| &h.1).map(|b| hoisted_vars(b, false, strict, out))
                    .unwrap_or(true)
                && finalizer
                    .as_ref()
                    .map(|b| hoisted_vars(b, false, strict, out))
                    .unwrap_or(true)
        }
        Stmt::Switch { cases, .. } => cases
            .iter()
            .all(|c| hoisted_vars(&c.body, false, strict, out)),
        _ => true,
    }
}

impl CaptureScan {
    #[cfg(feature = "parallel")]
    fn free_identifiers(func: &Function) -> Option<std::collections::HashSet<String>> {
        let mut scan = Self {
            scopes: Vec::new(), fn_depth: 0, captured: Default::default(),
            depth0_inner_decls: Default::default(), candidates: Default::default(),
            ever_candidates: Default::default(), captured_serials: Default::default(),
            const_candidates: Default::default(), free_refs: Default::default(),
            self_name: func.name.clone().filter(|_| func.is_fn_expr), self_named: false,
            loop_depth: 0, next_serial: 0, env_this: false, arrow_path: vec![func.is_arrow],
        };
        let mut defaults = scan.clone();
        scan.fn_body(func)?;
        // Parameter initializers cannot see declarations from the function body.
        let mut header = func.clone();
        header.body = std::cell::RefCell::new(Some(Rc::new(Vec::new())));
        header.lazy = std::cell::RefCell::new(None);
        defaults.fn_body(&header)?;
        let mut parameters = std::collections::HashSet::new();
        for parameter in &func.params { pat_idents(&parameter.pattern, &mut parameters); }
        if !func.is_arrow { parameters.insert("arguments".into()); }
        if func.is_fn_expr {
            if let Some(name) = &func.name { parameters.insert(name.clone()); }
        }
        defaults.push_scope(parameters);
        for parameter in &func.params { defaults.pat_decl_exprs(&parameter.pattern)?; }
        scan.free_refs.extend(defaults.free_refs);
        Some(scan.free_refs)
    }

    /// Analyze `func`, returning (captured names, inner-arrow-reads-this, activation-homed block
    /// lets, self-named, captured block-scoped names needing per-entry block envs) or `None` to
    /// bail.
    #[allow(clippy::type_complexity)]
    fn run(
        func: &Function,
    ) -> Option<(
        std::collections::HashSet<String>,
        bool,
        Vec<(String, bool)>,
        bool,
        std::collections::HashSet<String>,
    )> {
        let mut sc = CaptureScan {
            scopes: Vec::new(),
            fn_depth: 0,
            captured: Default::default(),
            depth0_inner_decls: Default::default(),
            candidates: Default::default(),
            ever_candidates: Default::default(),
            captured_serials: Default::default(),
            const_candidates: Default::default(),
            free_refs: Default::default(),
            self_name: func.name.clone().filter(|_| func.is_fn_expr),
            self_named: false,
            loop_depth: 0,
            next_serial: 0,
            env_this: false,
            arrow_path: vec![func.is_arrow],
        };
        sc.fn_body(func)?;
        // A captured name declared by an inner depth-0 scope needs per-block freshness —
        // except a candidate whose EVERY capture resolved through its own scope (a same-name
        // capture through any other binding — a for-of head, another block — captured a
        // DIFFERENT binding, which one activation slot can't express), which the compiler
        // homes activation-wide instead.
        let mut homed: Vec<String> = Vec::new();
        // The rest need a fresh binding per block entry / loop iteration: every block-level
        // declaration of such a name homes in a per-entry block env (see [`block_env`]).
        let mut blk = std::collections::HashSet::new();
        for n in &sc.captured {
            if sc.depth0_inner_decls.contains(n) {
                let ok = sc.candidates.get(n).is_some_and(|cs| {
                    !sc.free_refs.contains(n)
                        && sc
                            .captured_serials
                            .get(n)
                            .is_some_and(|set| set.len() == 1 && set.contains(cs))
                });
                if !ok {
                    blk.insert(n.clone());
                    continue;
                }
                homed.push(n.clone());
            }
        }
        // Homed names leave `captured`: the remaining consumers (param/var/body-lexical
        // homing, the for-of gates) concern OTHER bindings of the name, and any same-name
        // binding that could conflict already poisoned candidacy above.
        for n in &homed {
            sc.captured.remove(n);
        }
        homed.sort(); // deterministic cap_init order
        let homed = homed
            .into_iter()
            .map(|n| {
                let k = sc.const_candidates.contains(&n);
                (n, k)
            })
            .collect::<Vec<_>>();
        Some((sc.captured, sc.env_this, homed, sc.self_named, blk))
    }

    fn push_scope(&mut self, names: std::collections::HashSet<String>) {
        self.push_scope_lets(names, Default::default());
    }

    /// Like [`CaptureScan::push_scope`]; `lets` is the subset of `names` declared by plain
    /// `let`s / `const`s (→ is const), which qualify for activation homing when the scope runs
    /// at most once per call (outside every loop) and the name is unique/unambiguous (see
    /// `homable_inner_lets`).
    fn push_scope_lets(
        &mut self,
        names: std::collections::HashSet<String>,
        lets: std::collections::HashMap<String, bool>,
    ) {
        let serial = self.next_serial;
        self.next_serial += 1;
        if self.fn_depth == 0 && !self.scopes.is_empty() {
            for n in &names {
                self.depth0_inner_decls.insert(n.clone());
                let enclosed = self.scopes.iter().any(|(s, _, _)| s.contains(n));
                if self.loop_depth == 0
                    && lets.contains_key(n)
                    && !enclosed
                    && self.ever_candidates.insert(n.clone())
                {
                    self.candidates.insert(n.clone(), serial);
                    if lets[n] {
                        self.const_candidates.insert(n.clone());
                    }
                } else {
                    // A non-qualifying declaration doesn't poison an existing candidate: a
                    // later same-name SLOT declaration (nested or sibling) shadows the env
                    // binding correctly, and a capture through it fails the serial check in
                    // `run`. It does block FUTURE candidacy — the pending-consumption scheme
                    // in the compiler requires the candidate to be the walk-order-FIRST
                    // block-lexical declaration of its name.
                    self.ever_candidates.insert(n.clone());
                }
            }
        } else if self.fn_depth == 0 {
            // The function's top scope: params/vars/body lexicals block all same-name
            // candidacy (they'd be enclosing declarations).
            for n in &names {
                self.ever_candidates.insert(n.clone());
            }
        }
        self.scopes.push((names, self.fn_depth, serial));
    }

    /// Walk a whole function: params + hoisted vars + top-level lexicals in one scope, then body.
    fn fn_body(&mut self, func: &Function) -> Option<()> {
        let mut names = std::collections::HashSet::new();
        for p in &func.params {
            pat_idents(&p.pattern, &mut names);
        }
        if !func.is_arrow {
            names.insert("arguments".to_string());
        }
        if func.is_fn_expr {
            if let Some(n) = &func.name {
                names.insert(n.clone());
            }
        }
        let body = func.body();
        if !hoisted_vars(&body, true, func.is_strict, &mut names) {
            note_bail_reason(|| "capture-scan: sloppy block function (annex B)".to_string());
            return None;
        }
        self.declare_lexicals(&body, &mut names);
        self.push_scope(names);
        // Parameter defaults evaluate in the function scope.
        for p in &func.params {
            if let Some(d) = &p.default {
                self.expr(d)?;
            }
        }
        for s in body.iter() {
            self.stmt(s)?;
        }
        self.scopes.pop();
        Some(())
    }

    /// Add a statement list's block-scoped declarations (let/const/class, strict block functions).
    /// `lets` (when wanted) additionally collects the plain-`let` names — the only kind that
    /// qualifies for activation homing when captured (see `homable_inner_lets`).
    fn declare_lexicals(&self, stmts: &[Stmt], out: &mut std::collections::HashSet<String>) {
        self.declare_lexicals_lets(stmts, out, &mut Default::default());
    }

    fn declare_lexicals_lets(
        &self,
        stmts: &[Stmt],
        out: &mut std::collections::HashSet<String>,
        lets: &mut std::collections::HashMap<String, bool>,
    ) {
        for s in stmts {
            match s {
                Stmt::VarDecl {
                    kind: kind @ (DeclKind::Let | DeclKind::Const | DeclKind::Using | DeclKind::AwaitUsing),
                    decls,
                } => {
                    if matches!(kind, DeclKind::Let | DeclKind::Const) {
                        let mut ns = std::collections::HashSet::new();
                        for (p, _) in decls {
                            pat_idents(p, &mut ns);
                        }
                        for n in ns {
                            lets.insert(n, matches!(kind, DeclKind::Const));
                        }
                    }
                    for (p, _) in decls {
                        pat_idents(p, out);
                    }
                }
                Stmt::ClassDecl(c) => {
                    if let Some(n) = &c.name {
                        out.insert(n.clone());
                    }
                }
                Stmt::FuncDecl(f) => {
                    // Only reached for *block-level* declarations (top-level ones are in the
                    // hoisted set); strict mode makes them block lexicals. (Sloppy already bailed
                    // in hoisted_vars.)
                    if let Some(n) = &f.name {
                        out.insert(n.clone());
                    }
                }
                _ => {}
            }
        }
    }

    fn block(&mut self, stmts: &[Stmt]) -> Option<()> {
        let mut names = std::collections::HashSet::new();
        let mut lets = std::collections::HashMap::new();
        self.declare_lexicals_lets(stmts, &mut names, &mut lets);
        self.push_scope_lets(names, lets);
        for s in stmts {
            self.stmt(s)?;
        }
        self.scopes.pop();
        Some(())
    }

    fn reference(&mut self, name: &str) {
        if !self.self_named && self.self_name.as_deref() == Some(name) {
            self.self_named = true;
        }
        for (scope, depth, serial) in self.scopes.iter().rev() {
            if scope.contains(name) {
                if *depth == 0 && self.fn_depth > 0 {
                    self.captured.insert(name.to_string());
                    self.captured_serials
                        .entry(name.to_string())
                        .or_default()
                        .insert(*serial);
                }
                return;
            }
        }
        // Unresolved: a global/free name of the whole compilation — nothing to capture, but
        // it poisons activation homing for a like-named block lexical (whose env binding
        // would wrongly shadow the global for this reference).
        self.free_refs.insert(name.to_string());
    }

    /// Walk a pattern in *assignment* position (destructuring assignment): idents are references.
    fn pat_targets(&mut self, p: &Pattern) -> Option<()> {
        match p {
            Pattern::Ident(n) => {
                self.reference(n);
                Some(())
            }
            Pattern::Array(elems) => {
                for e in elems {
                    match e {
                        ArrayPatElem::Hole => {}
                        ArrayPatElem::Elem { pattern, default } => {
                            self.pat_targets(pattern)?;
                            if let Some(d) = default {
                                self.expr(d)?;
                            }
                        }
                        ArrayPatElem::Rest(p) => self.pat_targets(p)?,
                    }
                }
                Some(())
            }
            Pattern::Object(o) => {
                for pr in &o.props {
                    if let PropKey::Computed(k) = &pr.key {
                        self.expr(k)?;
                    }
                    self.pat_targets(&pr.value)?;
                    if let Some(d) = &pr.default {
                        self.expr(d)?;
                    }
                }
                if let Some(r) = &o.rest {
                    self.reference(r);
                }
                Some(())
            }
            Pattern::Member(e) => self.expr(e),
        }
    }

    /// Walk the expressions inside a *declaration* pattern (defaults, computed keys); the idents
    /// themselves were declared by the enclosing scope construction.
    fn pat_decl_exprs(&mut self, p: &Pattern) -> Option<()> {
        match p {
            Pattern::Ident(_) => Some(()),
            Pattern::Array(elems) => {
                for e in elems {
                    match e {
                        ArrayPatElem::Hole => {}
                        ArrayPatElem::Elem { pattern, default } => {
                            self.pat_decl_exprs(pattern)?;
                            if let Some(d) = default {
                                self.expr(d)?;
                            }
                        }
                        ArrayPatElem::Rest(p) => self.pat_decl_exprs(p)?,
                    }
                }
                Some(())
            }
            Pattern::Object(o) => {
                for pr in &o.props {
                    if let PropKey::Computed(k) = &pr.key {
                        self.expr(k)?;
                    }
                    self.pat_decl_exprs(&pr.value)?;
                    if let Some(d) = &pr.default {
                        self.expr(d)?;
                    }
                }
                Some(())
            }
            Pattern::Member(e) => self.expr(e),
        }
    }

    fn stmt(&mut self, s: &Stmt) -> Option<()> {
        if crate::stack::exhausted() {
            return None;
        }
        match s {
            Stmt::Expr(e) | Stmt::Throw(e) => self.expr(e),
            Stmt::VarDecl { decls, .. } => {
                for (p, init) in decls {
                    self.pat_decl_exprs(p)?;
                    if let Some(e) = init {
                        self.expr(e)?;
                    }
                }
                Some(())
            }
            Stmt::FuncDecl(f) => self.inner_fn(f),
            Stmt::Return(e) => {
                if let Some(e) = e {
                    self.expr(e)?;
                }
                Some(())
            }
            Stmt::If { test, cons, alt } => {
                self.expr(test)?;
                self.stmt(cons)?;
                if let Some(a) = alt {
                    self.stmt(a)?;
                }
                Some(())
            }
            Stmt::Block(b) => self.block(b),
            Stmt::While { test, body } => {
                self.expr(test)?;
                self.loop_depth += 1;
                let r = self.stmt(body);
                self.loop_depth -= 1;
                r
            }
            Stmt::DoWhile { body, test } => {
                self.loop_depth += 1;
                let r = self.stmt(body);
                self.loop_depth -= 1;
                r?;
                self.expr(test)
            }
            Stmt::For {
                init,
                test,
                update,
                body,
            } => {
                let mut names = std::collections::HashSet::new();
                if let Some(ForInit::VarDecl {
                    kind: DeclKind::Let | DeclKind::Const,
                    decls,
                }) = init.as_deref()
                {
                    for (p, _) in decls {
                        pat_idents(p, &mut names);
                    }
                }
                // The head scope itself counts as in-loop: its lexicals are per-iteration
                // fresh, never once-per-call.
                self.loop_depth += 1;
                self.push_scope(names);
                let r = (|| {
                    match init.as_deref() {
                        Some(ForInit::VarDecl { decls, .. }) => {
                            for (p, e) in decls {
                                self.pat_decl_exprs(p)?;
                                if let Some(e) = e {
                                    self.expr(e)?;
                                }
                            }
                        }
                        Some(ForInit::Expr(e)) => self.expr(e)?,
                        None => {}
                    }
                    if let Some(t) = test {
                        self.expr(t)?;
                    }
                    if let Some(u) = update {
                        self.expr(u)?;
                    }
                    self.stmt(body)
                })();
                self.scopes.pop();
                self.loop_depth -= 1;
                r
            }
            Stmt::ForInOf {
                decl,
                left,
                right,
                body,
                ..
            } => {
                self.expr(right)?;
                let mut names = std::collections::HashSet::new();
                match decl {
                    Some(
                        DeclKind::Let | DeclKind::Const | DeclKind::Using | DeclKind::AwaitUsing,
                    ) => {
                        pat_idents(left, &mut names);
                    }
                    Some(DeclKind::Var) => {} // already in the hoisted set
                    None => {}
                }
                self.loop_depth += 1;
                self.push_scope(names);
                let r = (|| {
                    if decl.is_none() {
                        self.pat_targets(left)?;
                    } else {
                        self.pat_decl_exprs(left)?;
                    }
                    self.stmt(body)
                })();
                self.scopes.pop();
                self.loop_depth -= 1;
                r
            }
            Stmt::Break(_) | Stmt::Continue(_) | Stmt::Empty | Stmt::Debugger => Some(()),
            Stmt::Try {
                block,
                handler,
                finalizer,
            } => {
                self.block(block)?;
                if let Some((param, body)) = handler.as_deref() {
                    let mut names = std::collections::HashSet::new();
                    if let Some(p) = param {
                        pat_idents(p, &mut names);
                    }
                    self.declare_lexicals(body, &mut names);
                    self.push_scope(names);
                    let r = (|| {
                        if let Some(p) = param {
                            self.pat_decl_exprs(p)?;
                        }
                        for s in body {
                            self.stmt(s)?;
                        }
                        Some(())
                    })();
                    self.scopes.pop();
                    r?;
                }
                if let Some(f) = finalizer {
                    self.block(f)?;
                }
                Some(())
            }
            Stmt::Switch { disc, cases } => {
                self.expr(disc)?;
                let mut names = std::collections::HashSet::new();
                let mut lets = std::collections::HashMap::new();
                for c in cases {
                    self.declare_lexicals_lets(&c.body, &mut names, &mut lets);
                }
                self.push_scope_lets(names, lets);
                let r = (|| {
                    for c in cases {
                        if let Some(t) = &c.test {
                            self.expr(t)?;
                        }
                        for s in &c.body {
                            self.stmt(s)?;
                        }
                    }
                    Some(())
                })();
                self.scopes.pop();
                r
            }
            Stmt::Labeled { body, .. } => self.stmt(body),
            Stmt::ClassDecl(c) => self.class(c),
            // `with`, modules, and anything else unrecognized: unanalyzable.
            other => {
                note_bail_reason(|| format!("capture-scan: stmt {}", node_kind(other)));
                None
            }
        }
    }

    /// Enter an inner function (declaration, expression, method, accessor…).
    fn inner_fn(&mut self, f: &Function) -> Option<()> {
        // Capture analysis needs the inner body; one that does not parse makes the outer
        // function uncompilable (the tree-walker reports the error when the inner is called).
        // A body parsed only for this scan is released again: compiling one enclosing function
        // must not materialise the AST of every function nested under it (V8's preparser scans
        // inner functions for free variables without keeping their trees either).
        let borrowed = f.parsed_body().is_none();
        f.ensure_body().ok()?;
        self.fn_depth += 1;
        self.arrow_path.push(f.is_arrow);
        let r = self.fn_body(f);
        self.arrow_path.pop();
        self.fn_depth -= 1;
        if borrowed {
            f.release_body();
        }
        r
    }

    fn class(&mut self, c: &Class) -> Option<()> {
        // The compiler delegates ClassDefinitionEvaluation to the oracle over the compiled
        // body's env, so the whole class (heritage, decorators and computed keys included) is
        // walked one function level down: every outer local it names homes in the activation,
        // a `this` read carries into it like an arrow's, and the class's own inner name scope
        // never counts as a body-level declaration.
        self.fn_depth += 1;
        self.arrow_path.push(true);
        let r = self.class_inner(c);
        self.arrow_path.pop();
        self.fn_depth -= 1;
        r
    }

    fn class_inner(&mut self, c: &Class) -> Option<()> {
        // Heritage, decorators, and computed keys evaluate at definition time (current depth);
        // method bodies / field initializers / static blocks run later (inner-function depth).
        for d in &c.decorators {
            self.expr(d)?;
        }
        if let Some(sc) = &c.superclass {
            self.expr(sc)?;
        }
        let mut names = std::collections::HashSet::new();
        if let Some(n) = &c.name {
            names.insert(n.clone());
        }
        self.push_scope(names);
        let r = (|| {
            for m in &c.members {
                for d in &m.decorators {
                    self.expr(d)?;
                }
                if let PropKey::Computed(k) = &m.key {
                    self.expr(k)?;
                }
                if let Some(f) = &m.func {
                    self.inner_fn(f)?;
                }
                if let Some(v) = &m.value {
                    // Field initializers run in an implicit method with its own `this` (the
                    // instance) — inner depth, and NOT part of any outer arrow chain.
                    self.fn_depth += 1;
                    self.arrow_path.push(false);
                    let r = self.expr(v);
                    self.arrow_path.pop();
                    self.fn_depth -= 1;
                    r?;
                }
            }
            Some(())
        })();
        self.scopes.pop();
        r
    }

    fn expr(&mut self, e: &Expr) -> Option<()> {
        if crate::stack::exhausted() {
            return None;
        }
        match e {
            Expr::Num(_)
            | Expr::BigInt(_)
            | Expr::Str(_)
            | Expr::Bool(_)
            | Expr::Null
            | Expr::Undefined
            | Expr::Regex { .. }
            | Expr::ImportMeta => Some(()),
            // Read from an inner arrow chain, `new.target` is the outer function's even after
            // it returned: the activation would have to carry it (not modeled).
            Expr::NewTarget => {
                if self.fn_depth > 0 && self.arrow_path[1..].iter().all(|a| *a) {
                    note_bail_reason(|| "capture-scan: new.target in an arrow".to_string());
                    return None;
                }
                Some(())
            }
            // `super.x` / `super.m()` read the `this` binding too (and `super()` binds it).
            Expr::This | Expr::Super => {
                // `this` read through an unbroken arrow chain from the outer function observes
                // the outer `this` — the activation must carry it.
                if self.fn_depth > 0 && self.arrow_path[1..].iter().all(|a| *a) {
                    self.env_this = true;
                }
                Some(())
            }
            Expr::Ident(n) => {
                self.reference(n);
                Some(())
            }
            Expr::Paren(i) | Expr::ToStr(i) | Expr::Await(i) | Expr::OptionalChain(i) => {
                self.expr(i)
            }
            Expr::Array(elems) => {
                for el in elems {
                    match el {
                        ArrayElem::Item(e) | ArrayElem::Spread(e) => self.expr(e)?,
                        ArrayElem::Hole => {}
                    }
                }
                Some(())
            }
            Expr::Object(props) => {
                for p in props {
                    match p {
                        PropDef::KeyValue { key, value } | PropDef::Cover { key, value } => {
                            if let PropKey::Computed(k) = key {
                                self.expr(k)?;
                            }
                            self.expr(value)?;
                        }
                        PropDef::Method { key, func }
                        | PropDef::Getter { key, func }
                        | PropDef::Setter { key, func } => {
                            if let PropKey::Computed(k) = key {
                                self.expr(k)?;
                            }
                            self.inner_fn(func)?;
                        }
                        PropDef::Spread(e) | PropDef::Proto(e) => self.expr(e)?,
                    }
                }
                Some(())
            }
            Expr::Func(f) => self.inner_fn(f),
            Expr::Class(c) => self.class(c),
            Expr::Yield { arg, .. } => {
                if let Some(a) = arg {
                    self.expr(a)?;
                }
                Some(())
            }
            Expr::Unary { arg, .. } | Expr::Update { arg, .. } => self.expr(arg),
            Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
                self.expr(left)?;
                self.expr(right)
            }
            Expr::Assign { target, value, .. } => {
                // A destructuring assignment target is a pattern of references.
                match &**target {
                    Expr::Array(_) | Expr::Object(_) => {
                        // Reinterpreting the literal as a pattern is the parser's job; walking it
                        // as an expression visits the same identifiers (Cover handles defaults).
                        self.expr(target)?;
                    }
                    t => self.expr(t)?,
                }
                self.expr(value)
            }
            Expr::Cond { test, cons, alt } => {
                self.expr(test)?;
                self.expr(cons)?;
                self.expr(alt)
            }
            Expr::Call { callee, args, .. } => {
                // Direct eval inside any nested function could name arbitrary outer locals.
                if matches!(&**callee, Expr::Ident(n) if n == "eval") {
                    note_bail_reason(|| "capture-scan: direct eval".to_string());
                    return None;
                }
                self.expr(callee)?;
                for a in args {
                    match a {
                        ArrayElem::Item(e) | ArrayElem::Spread(e) => self.expr(e)?,
                        ArrayElem::Hole => {}
                    }
                }
                Some(())
            }
            Expr::New { callee, args, .. } => {
                self.expr(callee)?;
                for a in args {
                    match a {
                        ArrayElem::Item(e) | ArrayElem::Spread(e) => self.expr(e)?,
                        ArrayElem::Hole => {}
                    }
                }
                Some(())
            }
            Expr::Member { obj, .. } => self.expr(obj),
            Expr::Index { obj, index, .. } => {
                self.expr(obj)?;
                self.expr(index)
            }
            Expr::Seq(es) => {
                for e in es {
                    self.expr(e)?;
                }
                Some(())
            }
            Expr::TaggedTemplate { tag, subs, .. } => {
                self.expr(tag)?;
                for s in subs {
                    self.expr(s)?;
                }
                Some(())
            }
            Expr::PrivateIn { obj, .. } => self.expr(obj),
            Expr::ImportCall { spec, options, .. } => {
                self.expr(spec)?;
                if let Some(o) = options {
                    self.expr(o)?;
                }
                Some(())
            }
        }
    }
}

/// Reuse the compiler's scope-aware AST walk rather than treating property keys
/// or shadowed lexical bindings as free variables.
#[cfg(feature = "parallel")]
pub(crate) fn function_free_identifiers(func: &Function) -> Option<std::collections::HashSet<String>> {
    CaptureScan::free_identifiers(func)
}

// ---------------------------------------------------------------------------------------------
// Compiler
// ---------------------------------------------------------------------------------------------

/// Compile `func` whole, or `None` if it uses anything outside the v0 subset.
#[cfg(feature = "compiler")]
pub fn compile(func: &Function) -> Option<Rc<Chunk>> {
    compile_with_derived(func, false)
}

/// Class metadata selects derived mode even when the body never calls `super()`.
/// This returns fresh compiler output; it does not read or mutate Function.code.
pub fn compile_derived(func: &Function) -> Option<Rc<Chunk>> {
    compile_with_derived(func, true)
}

fn compile_with_derived(func: &Function, force_derived: bool) -> Option<Rc<Chunk>> {
    if force_derived && (func.is_arrow || func.is_generator || func.is_async) { return None; }
    // An ahead-of-time blob's chunk for this function, if one is registered (see `serialize`).
    if let Some(chunk) = serialize::take_precompiled(func) {
        return Some(chunk);
    }
    serialize::note_compile();
    let _mem = crate::memstats::enter(crate::memstats::Cat::Compile);
    if !bail_log_enabled() {
        return compile_fresh(func, force_derived);
    }
    BAIL_REASON.with(|r| *r.borrow_mut() = None);
    let out = compile_fresh(func, force_derived);
    if out.is_none() {
        let why = BAIL_REASON.with(|r| r.borrow_mut().take());
        eprintln!("[tier] reason: {}", why.as_deref().unwrap_or("unknown"));
    }
    out
}

fn compile_fresh(func: &Function, force_derived: bool) -> Option<Rc<Chunk>> {
    let mut escaped = false;
    let out = compile_fresh_with(func, true, &mut escaped, force_derived);
    if escaped {
        // The virtual `arguments` / rest object is used some other way: a real one.
        return compile_fresh_with(func, false, &mut escaped, force_derived);
    }
    out
}

fn compile_fresh_with(func: &Function, virt_ok: bool, escaped: &mut bool, force_derived: bool) -> Option<Rc<Chunk>> {
    if func.ensure_body().is_err() {
        return None;
    }
    // Body facts the scanner already knows: `new.target` is an observation channel into the
    // activation that slots do not provide; `arguments` in an arrow is a free variable
    // we do not model. Parameterless synchronous ordinary functions can materialize an unmapped
    // arguments object into a dedicated slot (the common variadic-helper shape).
    let scan = func.scan_flags();
    // `new.target` is the engine's current value in a synchronous non-arrow body (see
    // `Op::LoadNewTarget`); an arrow's is lexical, and a resumed body runs under its resumer's.
    if scan & SCAN_NEW_TARGET != 0 && (func.is_arrow || func.is_async || func.is_generator) {
        log_bail("fn", "new.target");
        return None;
    }
    let mut uses_arguments = scan & SCAN_ARGUMENTS != 0;
    // Non-simple parameter lists (defaults, patterns, rest) and strict functions get an
    // unmapped arguments object, which a slot holds faithfully; a sloppy simple list maps
    // `arguments[k]` onto the parameter bindings, which slots cannot alias.
    let simple_params = func
        .params
        .iter()
        .all(|p| !p.rest && p.default.is_none() && matches!(p.pattern, Pattern::Ident(_)));
    if uses_arguments && (func.is_arrow || func.is_async) {
        log_bail("fn", "arguments with arrow/async");
        return None;
    }
    let mapped = !func.params.is_empty() && simple_params && !func.is_strict;
    // A parameter named `arguments` shadows the object (none is created).
    if uses_arguments {
        let mut pnames = std::collections::HashSet::new();
        for p in &func.params {
            pat_idents(&p.pattern, &mut pnames);
        }
        if pnames.contains("arguments") {
            uses_arguments = false;
        }
    }
    // Generators compile to a `VmCoro` body (`yield` suspends like `await`, see [`generator`]).
    // Async functions compile: `await` lowers to `Op::Await`, which suspends the `VmCoro` that
    // drives this body.
    // A named function expression's own name binds to the callee (see the prologue below): the
    // prologue's `LoadCallee` runs in an async body's first step, which `run_async` drives
    // synchronously inside the call's own frame (params cannot `await`).

    // Capture analysis: which locals inner functions can name (they live in a real activation
    // env), and whether an inner arrow chain reads `this`. `None` = unanalyzable — bail.
    let Some((captured, env_this, block_lets, _self_named, blk_names)) = CaptureScan::run(func)
    else {
        let head: String = func
            .source()
            .as_deref()
            .unwrap_or("<no source>")
            .chars()
            .take(90)
            .collect();
        log_bail(
            "capture-scan",
            &format!(
                "unanalyzable body (eval/with/annexB/pattern) in: {}",
                head.replace('\n', " ")
            ),
        );
        return None;
    };

    // A mapped `arguments` aliases the parameters, which slots cannot; a virtual one (see
    // `Chunk::virt_base`) is only read while no parameter is ever written, where aliasing is
    // unobservable.
    let virt_args = virt_ok
        && uses_arguments
        && !func.is_generator
        && !captured.contains("arguments")
        && func.params.iter().all(|p| !p.rest)
        && (!mapped
            || func.params.iter().all(|p| match &p.pattern {
                Pattern::Ident(n) => !captured.contains(n),
                _ => false,
            }));
    if uses_arguments && mapped && !virt_args {
        log_bail("fn", "mapped arguments");
        return None;
    }
    // A body calling `super(…)` is a derived class constructor: `this` becomes a TDZ binding in
    // the activation (read lexically everywhere), see [`derived`].
    let derived = force_derived || (!func.is_arrow
        && !func.is_generator
        && !func.is_async
        && crate::eval::stmts_have_super_call(&func.body()));
    let mut c = Compiler {
        // Arrows forward the enclosing binding through their scope chain. They must not
        // synthesize a new this binding for nested arrows, especially before super().
        env_this: (env_this && !func.is_arrow) || derived,
        lexical_this: func.is_arrow || derived,
        strict: func.is_strict,
        derived,
        generator: func.is_generator,
        async_gen: func.is_generator && func.is_async,
        blk_names,
        // Strict code's calls in tail position are proper tail calls (see [`self_tail`]); an
        // async or generator body completes through its coroutine after the call.
        tail_calls: func.is_strict && !func.is_async && !func.is_generator && crate::tail_calls_enabled(),
        ..Compiler::default()
    };
    // Captured once-per-call block `let`s home in the activation (TDZ from entry, initialized
    // by the declaring block's own StoreCapInit); CaptureScan proved no enclosing same-name
    // declaration and block-resolved references only, so the function-flat env map is
    // faithful (nested same-name declarations shadow it through their slots).
    for (name, is_const) in &block_lets {
        c.cap_inits
            .push(CapInit::Lexical(Rc::from(name.as_str()), *is_const));
        c.env_bind(name, *is_const);
        c.homed_lets.insert(name.clone());
        c.homed_pending.insert(name.clone());
    }
    // Parameters: one positional slot each (a sloppy duplicate name resolves to the later
    // parameter, matching the env behavior where the later insert wins). A captured identifier
    // parameter keeps its positional slot (dead) but homes in the activation env. A
    // destructuring parameter's positional slot is hidden; its leaves bind like `var`s (slots,
    // or activation bindings when captured) and are initialized in parameter order below. A
    // rest parameter's slot is seeded with the surplus arguments (`Chunk::rest_slot`).
    let hoist_fn_names: std::collections::HashSet<String> =
        crate::interpreter::collect_hoist_ops(&func.body(), func.is_strict, &[])
            .into_iter()
            .filter_map(|op| match op {
                HoistOp::Fn(n, _) | HoistOp::AnnexB(n, _) | HoistOp::VarForce(n) => Some(n),
                _ => None,
            })
            .collect();
    enum ParamInit<'a> {
        Default(u16, &'a Expr, Option<u32>),
        Pattern(u16, &'a Pattern, Option<&'a Expr>),
    }
    let mut inits: Vec<ParamInit> = Vec::new();
    // Every name bound by parameter k or later (a default may not observe them: the spec's
    // parameter TDZ would throw where slots read a seeded `undefined`).
    let later_names = |k: usize| -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        for q in &func.params[k..] {
            pat_idents(&q.pattern, &mut out);
        }
        out
    };
    let n_positional = func.params.len() - func.params.last().is_some_and(|p| p.rest) as usize;
    let mut pattern_params: Vec<(u16, &Pattern)> = Vec::new();
    // A captured rest parameter: its seeded slot is copied into the activation binding.
    let mut rest_cap: Option<(u16, u32)> = None;
    for (k, p) in func.params.iter().enumerate() {
        if p.rest {
            let Pattern::Ident(name) = &p.pattern else {
                log_bail("params", "destructuring rest parameter");
                return None;
            };
            if hoist_fn_names.contains(name) || p.default.is_some() {
                log_bail("params", "rest parameter shadowed by a function");
                return None;
            }
            let used = captured.contains(name) || !parameters::rest_unused(func, name);
            let virt = virt_ok
                && used
                && !captured.contains(name)
                && !uses_arguments
                && !func.is_generator
                && c.slot_names.len() == k;
            if virt {
                for _ in 0..VIRT_WINDOW {
                    c.fresh_slot("%arg");
                }
            }
            let slot = c.fresh_slot(name);
            // A rest array nothing can read is never built (see `parameters::rest_unused`).
            if used {
                c.rest_slot = Some(slot);
            }
            if virt {
                c.virt = Some(VirtC {
                    slot,
                    base: k as u16,
                    rest: true,
                    end: (k + VIRT_WINDOW) as u16,
                    escaped: false,
                });
            }
            if captured.contains(name) {
                c.cap_inits.push(CapInit::Var(Rc::from(name.as_str())));
                c.env_bind(name, false);
                rest_cap = Some((slot, c.name_idx(name)));
            } else {
                c.scope_bind(name, slot, false);
            }
            continue;
        }
        let Pattern::Ident(name) = &p.pattern else {
            let slot = c.fresh_slot("%param");
            let banned_owned = later_names(k);
            let banned: std::collections::HashSet<&str> =
                banned_owned.iter().map(|n| n.as_str()).collect();
            if let Some(d) = &p.default {
                if !parameters::default_expr_safe(d, &banned) {
                    log_bail_node("params-default", d, 100);
                    log_bail("params", "unsafe default expression");
                    return None;
                }
            }
            if !parameters::pattern_exprs_safe(&p.pattern, &banned) {
                log_bail("params", "unsafe destructuring parameter expression");
                return None;
            }
            pattern_params.push((slot, &p.pattern));
            inits.push(ParamInit::Pattern(slot, &p.pattern, p.default.as_deref()));
            continue;
        };
        if let Some(d) = &p.default {
            // Captured defaults need a bounded initialization proof; uncaptured defaults
            // retain the existing expression-safety check below.
            if captured.contains(name) && !parameters::captured_default_safe(func, name, d) {
                log_bail("params", "unsafe captured defaulted parameter");
                return None;
            }
            let banned_owned = later_names(k);
            let banned: std::collections::HashSet<&str> =
                banned_owned.iter().map(|n| n.as_str()).collect();
            if !parameters::default_expr_safe(d, &banned) {
                log_bail_node("params-default", d, 100);
                log_bail("params", "unsafe default expression");
                return None;
            }
            let cap = captured.contains(name).then(|| c.name_idx(name));
            inits.push(ParamInit::Default(k as u16, d, cap));
        }
        let slot = c.fresh_slot(name);
        if captured.contains(name) {
            c.cap_inits
                .push(CapInit::Param(k as u16, Rc::from(name.as_str())));
            c.env_bind(name, false);
        } else {
            c.scope_bind(name, slot, false);
        }
    }
    c.n_params = n_positional;
    if let Some(v) = &c.virt {
        c.n_params = v.end as usize;
    }
    // A virtual `arguments`' hidden parameter window (see `Chunk::virt_base`).
    let virt_args = virt_args && c.slot_names.len() == n_positional;
    if virt_args {
        while c.slot_names.len() < n_positional.max(VIRT_WINDOW) {
            c.fresh_slot("%arg");
        }
        c.n_params = c.slot_names.len();
    }
    if let Some((slot, n)) = rest_cap {
        c.emit(Op::LoadLocal(slot));
        c.emit(Op::StoreCap(n));
    }
    // Destructuring-parameter leaves (after every positional slot).
    for (_, pat) in &pattern_params {
        let mut leaves = std::collections::HashSet::new();
        pat_idents(pat, &mut leaves);
        let mut leaves: Vec<String> = leaves.into_iter().collect();
        leaves.sort();
        for name in leaves {
            if hoist_fn_names.contains(&name) {
                log_bail("params", "destructured parameter shadowed by a function/for-head var");
                return None;
            }
            if captured.contains(&name) {
                if !c.env_has(&name) {
                    c.cap_inits.push(CapInit::Var(Rc::from(name.as_str())));
                    c.env_bind(&name, false);
                }
            } else {
                let slot = c.fresh_slot(&name);
                c.scope_bind(&name, slot, false);
            }
        }
    }
    // The arguments object (after the parameters: positional slots come first).
    if uses_arguments {
        let slot = c.fresh_slot("arguments");
        c.arguments_slot = Some(slot);
        if virt_args {
            c.virt = Some(VirtC {
                slot,
                base: 0,
                rest: false,
                end: c.n_params as u16,
                escaped: false,
            });
        }
        if captured.contains("arguments") {
            // An inner arrow names it: home the object in the activation.
            if hoist_fn_names.contains("arguments") {
                return None;
            }
            c.cap_inits.push(CapInit::Var(Rc::from("arguments")));
            c.env_bind("arguments", false);
            let n = c.name_idx("arguments");
            c.emit(Op::LoadLocal(slot));
            c.emit(Op::StoreCap(n));
        } else {
            c.scope_bind("arguments", slot, false);
        }
    }
    // A named function expression's self-name: an immutable binding of the callee, in its own
    // scope outside the function's, so any parameter, var, function or top-level lexical of
    // the same name shadows it. Bound before the parameter initializers (a default may name
    // it). A captured one homes in the activation; its binding is strict-immutable there, so a
    // sloppy body (where assigning the self-name is a silent no-op) stays in the oracle.
    if func.is_fn_expr {
        if let Some(name) = &func.name {
            let mut shadow = std::collections::HashSet::new();
            for p in &func.params {
                pat_idents(&p.pattern, &mut shadow);
            }
            let body = func.body();
            for op in crate::interpreter::collect_hoist_ops(&body, func.is_strict, &[]) {
                match op {
                    HoistOp::Var(n)
                    | HoistOp::VarForce(n)
                    | HoistOp::Fn(n, _)
                    | HoistOp::AnnexB(n, _) => {
                        shadow.insert(n);
                    }
                }
            }
            for s in body.iter() {
                match s {
                    Stmt::VarDecl { kind, decls } if !matches!(kind, DeclKind::Var) => {
                        for (p, _) in decls {
                            pat_idents(p, &mut shadow);
                        }
                    }
                    Stmt::ClassDecl(k) => {
                        if let Some(n) = &k.name {
                            shadow.insert(n.clone());
                        }
                    }
                    _ => {}
                }
            }
            if name != "arguments" && !shadow.contains(name) {
                c.emit(Op::LoadCallee);
                if captured.contains(name) {
                    if !func.is_strict {
                        log_bail("fn", "captured self-name of a sloppy function expression");
                        return None;
                    }
                    c.cap_inits
                        .push(CapInit::Lexical(Rc::from(name.as_str()), true));
                    c.env_bind(name, true);
                    let n = c.name_idx(name);
                    c.emit(Op::StoreCapInit(n));
                } else {
                    let slot = c.fresh_slot(name);
                    c.scope_bind(name, slot, true);
                    c.emit(Op::StoreLocal(slot));
                }
            }
        }
    }
    // Parameter initializers run in parameter order before anything else (spec order:
    // parameter binding precedes var/function hoisting).
    for init in inits {
        match init {
            ParamInit::Default(slot, d, cap) => c.parameter_default(slot, d, cap).ok()?,
            ParamInit::Pattern(slot, pat, d) => {
                c.emit(Op::LoadLocal(slot));
                if let Some(d) = d {
                    c.emit(Op::Dup);
                    c.emit(Op::Undef);
                    c.emit(Op::StrictEq);
                    let skip = c.emit(Op::JumpIfFalse(0));
                    c.emit(Op::Pop);
                    c.expr(d).ok()?;
                    c.patch(skip);
                }
                if c.destructure_store(pat, DeclKind::Var).is_err() {
                    log_bail("params", "destructuring parameter pattern");
                    return None;
                }
            }
        }
    }
    // Function-scoped `var`s and hoisted function declarations from the shared hoist plan.
    let body = func.body();
    // Captured names only `var`s have bound so far (still undefined at entry).
    let mut var_only = std::collections::HashSet::new();
    for op in crate::interpreter::collect_hoist_ops(&body, func.is_strict, &[]) {
        match op {
            HoistOp::Var(name) => {
                if captured.contains(&name) {
                    if !c.env_has(&name) {
                        c.cap_inits.push(CapInit::Var(Rc::from(name.as_str())));
                        c.env_bind(&name, false);
                        var_only.insert(name);
                    }
                } else if c.lookup(&name).is_none() {
                    let slot = c.fresh_slot(&name);
                    c.scope_bind(&name, slot, false);
                }
            }
            HoistOp::VarForce(name) => {
                if captured.contains(&name) {
                    // Nothing bound yet (no parameter or earlier hoist of the name): the reset
                    // to undefined is a plain `var` binding.
                    if var_only.contains(&name) {
                        continue;
                    }
                    if c.env_has(&name) || c.lookup(&name).is_some() {
                        log_bail("fn", "for-head var resetting a captured parameter");
                        return None;
                    }
                    c.cap_inits.push(CapInit::Var(Rc::from(name.as_str())));
                    c.env_bind(&name, false);
                    var_only.insert(name);
                    continue;
                }
                let slot = match c.lookup(&name) {
                    Some((s, _)) => s,
                    None => {
                        let s = c.fresh_slot(&name);
                        c.scope_bind(&name, s, false);
                        s
                    }
                };
                if (slot as usize) < func.params.len() {
                    if func.params[slot as usize].default.is_some() {
                        return None; // reset would clobber the default (oracle order differs)
                    }
                    c.var_force_resets.push(slot);
                }
            }
            HoistOp::Fn(name, f) => {
                var_only.remove(&name);
                let fidx = c.funcs.len() as u16;
                c.funcs.push(f.clone());
                if captured.contains(&name) {
                    c.cap_inits.push(CapInit::Fn(fidx, Rc::from(name.as_str())));
                    c.env_bind(&name, false);
                } else {
                    let slot = match c.lookup(&name) {
                        Some((s, _)) => s,
                        None => {
                            let s = c.fresh_slot(&name);
                            c.scope_bind(&name, s, false);
                            s
                        }
                    };
                    // Created at entry, in hoist order, closing over the activation.
                    c.emit(Op::MakeClosure(fidx as u32, u32::MAX));
                    c.emit(Op::StoreLocal(slot));
                }
            }
            // Annex B promotions have declaration-time sync the VM doesn't model — bail.
            HoistOp::AnnexB(..) => return None,
        }
    }
    // Body-level lexicals: captured ones home in the activation (inserted in TDZ by
    // make_run_env), the rest get TDZ slots.
    if c.declare_body_lexicals(&body, &captured).is_err() {
        log_bail("body-lexicals", "unsupported declaration form");
        return None;
    }
    // A generator's call ends here: parameters bound and declarations instantiated, it parks
    // before the first statement until the first `next()`.
    if c.generator {
        c.emit(Op::InitialYield);
    }
    for stmt in body.iter() {
        if c.stmt(stmt).is_err() {
            log_bail_node("stmt-in", stmt, 80);
            return None;
        }
    }
    // A for-head `var` reset of a parameter rewrites an `arguments` element too.
    if c.virt.as_ref().is_some_and(|v| {
        v.escaped || (!v.rest && c.var_force_resets.iter().any(|&s| s < v.end))
    }) {
        *escaped = true;
        return None;
    }
    if c.derived {
        c.emit(Op::Undef);
        c.emit(Op::DerivedReturn);
    } else {
        c.emit(Op::ReturnUndef);
    }
    peephole(&mut c.ops);
    let n_switch_tables = switch_table::switch_pass(&mut c.ops);
    static DUMP: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *DUMP.get_or_init(|| std::env::var_os("LUMEN_BC_DUMP").is_some()) {
        eprintln!("[bc] {:?} env_this={} caps={}", func.name, c.env_this, c.cap_inits.len());
        for (pc, op) in c.ops.iter().enumerate() {
            eprintln!("  {pc:4} {op:?}");
        }
    }
    let cap_cache_len = c.names.len();
    let activation_layout = activation::ActivationLayout::new(&c.cap_inits, c.env_this, &c.names);
    let positions = positions::encode(&c.ops, &c.sites);
    Some(jit::track_chunk(Rc::new(Chunk {
        debug_name: func.name.clone().unwrap_or_else(|| "<anonymous>".into()),
        ops: c.ops,
        consts: c.consts,
        names: c.names,
        n_slots: c.slot_names.len(),
        slot_names: c.slot_names,
        n_params: c.n_params,
        arguments_slot: c.arguments_slot,
        var_force_resets: c.var_force_resets,
        uses_this: c.uses_this,
        funcs: c.funcs,
        classes: c.classes,
        rest_slot: c.rest_slot,
        virt_base: c.virt.as_ref().map(|v| v.base),
        cap_inits: c.cap_inits,
        activation_layout,
        env_this: c.env_this,
        obj_maps: (0..c.obj_maps)
            .map(|_| std::cell::OnceCell::new())
            .collect(),
        caches: c.caches,
        jit: Default::default(),
        #[cfg(feature = "bench")]
        precompiled_origin: None,
        name_pins: std::cell::RefCell::new(vec![None; c.name_caches.len()]),
        name_paths: (0..c.name_caches.len())
            .map(|_| std::cell::RefCell::new(None))
            .collect(),
        name_caches: c.name_caches,
        cap_caches: vec![std::cell::Cell::new(NameIc::EMPTY); cap_cache_len],
        cap_pins: std::cell::RefCell::new(vec![None; cap_cache_len]),
        switch_tables: (0..n_switch_tables)
            .map(|_| std::cell::OnceCell::new())
            .collect(),
        derived: c.derived,
        reflect_args: !func.is_strict
            && !func.is_arrow
            && !func.is_method
            && !func.is_generator
            && !func.is_async,
        positions,
        inline_cbs: Default::default(),
        feedback_key: Default::default(),
    })))
}

#[derive(Default)]
struct Compiler {
    /// Root arrow bodies read this from the closure environment.
    lexical_this: bool,
    /// The compiled function's strictness (carried into ops whose runtime behavior forks on it).
    strict: bool,
    /// Captured once-per-call block `let`s homed in the activation (see CaptureScan's
    /// `candidates`). `homed_pending` holds the ones whose declaring block hasn't been reached
    /// yet: the FIRST block-level declaration of the name consumes it (skipping slot creation);
    /// any later same-name declaration is a nested shadow and binds a slot normally.
    homed_lets: std::collections::HashSet<String>,
    homed_pending: std::collections::HashSet<String>,
    ops: Vec<Op>,
    consts: Vec<Value>,
    names: Vec<Rc<str>>,
    /// Lexical scopes for slot resolution: (name, slot, is_const), innermost last.
    scopes: Vec<Vec<(String, u16, bool)>>,
    slot_names: Vec<Rc<str>>,
    n_params: usize,
    arguments_slot: Option<u16>,
    var_force_resets: Vec<u16>,
    loops: Vec<LoopCtx>,
    /// Labels collected from an enclosing `Stmt::Labeled` chain, waiting to be attached to the next
    /// loop's `LoopCtx` (drained when that loop pushes its context).
    pending_labels: Vec<String>,
    uses_this: bool,
    /// Count of object-literal template sites handed out (see `Chunk::obj_maps`).
    obj_maps: u32,
    caches: Vec<std::cell::Cell<IcState>>,
    name_caches: Vec<std::cell::Cell<NameIc>>,
    /// Number of `PushHandler` regions active at the current emission point. `break`/`continue`
    /// jumping out of a `try` block (or a for-of body, which wraps itself in a handler) must
    /// emit a `PopHandler` per region crossed, or the stale handler catches unrelated throws
    /// later in the frame.
    try_depth: u32,
    /// Slots that ever enter a temporal dead zone (an `Op::Tdz` was emitted for them). The fused
    /// element ops defer the base-slot read past key/value evaluation, which is only
    /// order-unobservable when the base can never TDZ-throw — params and `var`s qualify.
    tdz_slots: std::collections::HashSet<u16>,
    /// Slots whose `Op::Tdz` was emitted but whose declaration has not been compiled yet: an
    /// assignment compiled meanwhile could run in the TDZ, where `StoreLocal` would silently
    /// initialize instead of throwing — such assignments bail. (Uncaptured slots are only
    /// reachable in textual order, and re-entering a block re-runs its `Tdz`.)
    tdz_pending: std::collections::HashSet<u16>,
    /// The source position (+1; 0 = none) of the call/`new` expression being compiled, and the
    /// one recorded for every call-site op emitted, in order (see [`positions`]).
    site: u32,
    sites: Vec<u32>,
    /// Captured (env-homed) function-scope-wide names → is_const. Slot scopes shadow these.
    env_names: std::collections::HashMap<String, bool>,
    funcs: Vec<Rc<Function>>,
    classes: Vec<Rc<Class>>,
    rest_slot: Option<u16>,
    /// The virtual `arguments` / rest object being compiled (see [`Chunk::virt_base`]).
    virt: Option<VirtC>,
    /// Enclosing `try`/`finally` regions being compiled, innermost last (see `try_finally`).
    finallys: Vec<finally::FinallyCtx>,
    cap_inits: Vec<CapInit>,
    env_this: bool,
    /// Derived-constructor mode (see [`derived`]).
    derived: bool,
    /// A generator body: `yield` compiles (see [`generator`]).
    generator: bool,
    /// An async generator body: `yield` and `return <expr>` await their operand.
    async_gen: bool,
    /// Lowering a destructuring *assignment*: `destructure_store` leaves are PutValues (see
    /// [`destructure_assign`]).
    assign_mode: bool,
    /// Captured block-scoped names (CaptureScan): every block-level declaration of one of these
    /// homes in a per-entry block env (see [`block_env`]).
    blk_names: std::collections::HashSet<String>,
    /// Carrier slots of the block envs enclosing the emission point, innermost last.
    blk_envs: Vec<u16>,
    /// Emit proper tail calls (`Op::TailCall`) for `return f(…)` (see [`self_tail`]).
    tail_calls: bool,
    /// Captured block declarations collected while declaring a scope, waiting for
    /// [`Compiler::blk_flush`] to open their block env.
    pending_blk: Vec<(String, bool)>,
    /// Destructuring into a just-created block env nothing can have captured yet (a for-of
    /// head's per-iteration env): its leaves' initialization order is unobservable, so the
    /// batched array walk may store them.
    fresh_blk: bool,
    /// Compiling an inlined arrow's block body: where its `return`s go (see [`inline_callback`]).
    inline_ret: Option<inline_callback::InlineRet>,
}

/// Where a name resolves inside the compiled body.
enum Home {
    Slot(u16, bool),
    /// Captured: lives in the activation env; bool = is_const.
    Env(bool),
    /// A captured block-scoped binding in the block env whose carrier is in slot `.0` (see
    /// [`block_env`]); bool = is_const.
    Blk(u16, bool),
}

/// Scope-entry marker: the entry's "slot" is a block-env carrier slot (see [`Home::Blk`]).
const BLK_BIT: u16 = 0x8000;

#[derive(Default)]
struct LoopCtx {
    breaks: Vec<usize>,
    continues: Vec<usize>,
    /// `Compiler::try_depth` when this context was entered — the reference point for how many
    /// handler regions a `break`/`continue` targeting this context crosses.
    entry_try_depth: u32,
    /// A for-of loop's iterator slot: crossing `break`s close it (`IterCloseL`); its own
    /// `continue`s don't (the loop keeps iterating).
    foreach_iter: Option<u16>,
    /// A `for await` loop's state slot (see [`for_await`]): its closes await.
    foreach_async: Option<u16>,
    /// For a for-of context: `try_depth` just after its per-iteration body handler pushed —
    /// exits emitted inside the body pop down to here before touching the handler itself.
    body_try_depth: u32,
    /// Labels naming this loop (usually zero or one; `a: b: for(…)` stacks several). A labelled
    /// `break`/`continue` searches the loop stack for the ctx carrying its target label.
    labels: Vec<String>,
    /// A `switch` context: an unlabelled `break` targets it, but `continue` skips past it to the
    /// innermost enclosing loop.
    is_switch: bool,
    /// A labelled non-loop statement: only a `break` naming one of its labels targets it.
    label_only: bool,
}

/// Debug (`LUMEN_TIER_LOG=1`): report the AST construct a compile bail came from.
fn log_bail(what: &str, detail: &str) {
    if bail_log_enabled() {
        eprintln!("[tier] unsupported {what}: {detail}");
        note_bail_reason(|| format!("{what}: {detail}"));
    }
}

thread_local! {
    /// Debug (`LUMEN_TIER_LOG=1`): the innermost construct the current compile bailed on.
    static BAIL_REASON: std::cell::RefCell<Option<String>> = const { std::cell::RefCell::new(None) };
}

/// Record `why` as the current compile's bail reason unless an inner one was recorded first.
fn note_bail_reason(why: impl FnOnce() -> String) {
    if bail_log_enabled() {
        BAIL_REASON.with(|r| {
            let mut r = r.borrow_mut();
            if r.is_none() {
                *r = Some(why());
            }
        });
    }
}

/// The Debug variant name of an AST node (the text before its first delimiter).
fn node_kind(node: &dyn std::fmt::Debug) -> String {
    let s = format!("{node:?}");
    s.split(|c: char| c == '(' || c == ' ' || c == '{')
        .next()
        .unwrap_or("")
        .to_string()
}

fn bail_log_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("LUMEN_TIER_LOG").is_some())
}

/// Debug-formats an AST node for a bail log line, only when logging is on: a class expression's
/// Debug output walks every member, and doing that for every bail is measurable.
fn log_bail_node(what: &str, node: &dyn std::fmt::Debug, width: usize) {
    if bail_log_enabled() {
        if what != "expr" && what != "stmt" {
            note_bail_reason(|| format!("{what} {}", node_kind(node)));
        }
        eprintln!("[tier] unsupported {what}: {:.width$}", format!("{node:?}"));
    }
}

/// Compilation bail: the construct is outside the v0 subset.
struct Bail;
type CResult = Result<(), Bail>;

/// Whether `e` provably cannot reassign the local `name` (for fused element ops, which defer the
/// base-slot read past this expression's evaluation). Whitelist recursion: any variant not
/// explicitly handled answers `false` (don't fuse). Calls and nested functions are safe — a slot
/// local is unobservable outside its function (that is what makes slot storage sound), so only a
/// syntactic assignment/update in this very expression could touch it.
fn no_assign_to(e: &Expr, name: &str) -> bool {
    match e {
        Expr::Num(_)
        | Expr::BigInt(_)
        | Expr::Str(_)
        | Expr::Bool(_)
        | Expr::Null
        | Expr::Undefined
        | Expr::Ident(_)
        | Expr::This
        | Expr::Regex { .. }
        | Expr::Func(_) => true,
        Expr::Paren(x) | Expr::ToStr(x) | Expr::Unary { arg: x, .. } => no_assign_to(x, name),
        Expr::Update { arg, .. } => match &**arg {
            Expr::Ident(n) => n != name,
            Expr::Member { obj, .. } => no_assign_to(obj, name),
            Expr::Index { obj, index, .. } => no_assign_to(obj, name) && no_assign_to(index, name),
            _ => false,
        },
        Expr::Assign { target, value, .. } => {
            let target_ok = match &**target {
                Expr::Ident(n) => n != name,
                Expr::Member { obj, .. } => no_assign_to(obj, name),
                Expr::Index { obj, index, .. } => {
                    no_assign_to(obj, name) && no_assign_to(index, name)
                }
                _ => false, // destructuring pattern — could bind `name`
            };
            target_ok && no_assign_to(value, name)
        }
        Expr::Binary { left, right, .. } | Expr::Logical { left, right, .. } => {
            no_assign_to(left, name) && no_assign_to(right, name)
        }
        Expr::Cond { test, cons, alt } => {
            no_assign_to(test, name) && no_assign_to(cons, name) && no_assign_to(alt, name)
        }
        Expr::Member { obj, .. } => no_assign_to(obj, name),
        Expr::Index { obj, index, .. } => no_assign_to(obj, name) && no_assign_to(index, name),
        Expr::Call { callee, args, .. } | Expr::New { callee, args, .. } => {
            no_assign_to(callee, name)
                && args.iter().all(|a| match a {
                    ArrayElem::Item(e) | ArrayElem::Spread(e) => no_assign_to(e, name),
                    ArrayElem::Hole => true,
                })
        }
        Expr::Array(elems) => elems.iter().all(|a| match a {
            ArrayElem::Item(e) | ArrayElem::Spread(e) => no_assign_to(e, name),
            ArrayElem::Hole => true,
        }),
        _ => false,
    }
}

impl Compiler {
    fn emit(&mut self, op: Op) -> usize {
        if let Op::Tdz(s) = op {
            self.tdz_pending.insert(s);
        }
        if let Some(v) = &mut self.virt {
            v.escaped |= v.escapes(&op);
        }
        if positions::is_site(&op) {
            self.sites.push(self.site.wrapping_sub(1));
        }
        self.ops.push(op);
        self.ops.len() - 1
    }

    /// Store the initializing value of a lexical slot: code compiled after this point can
    /// only run once the binding is initialized (see `tdz_pending`).
    fn init_slot(&mut self, slot: u16) {
        self.emit(Op::StoreLocal(slot));
        self.tdz_pending.remove(&slot);
    }
    /// Reserve a fresh inline-cache slot (starts empty) for a property-access op.
    fn new_cache(&mut self) -> u32 {
        // PROP_IC_WAYS consecutive ways per site: consumers address way 1; probes reach the
        // others at `cache_ptr + k` (see `Interp::ic_way`). Keeps every existing call site
        // untouched.
        let idx = self.caches.len() as u32;
        for _ in 0..PROP_IC_WAYS {
            self.caches.push(std::cell::Cell::new(IcState::EMPTY));
        }
        idx
    }
    /// Reserve one cache cell for an op whose generated template has a single stable shape.
    /// Unlike property sites, `instanceof` does not need four polymorphic ways; keeping this
    /// separate avoids paying 96 bytes per source occurrence.
    fn new_single_cache(&mut self) -> u32 {
        let idx = self.caches.len() as u32;
        self.caches.push(std::cell::Cell::new(IcState::EMPTY));
        idx
    }
    /// Reserve a fresh name-cache slot for a free-name op.
    fn new_name_cache(&mut self) -> u32 {
        self.name_caches.push(std::cell::Cell::new(NameIc::EMPTY));
        (self.name_caches.len() - 1) as u32
    }
    fn emit_store_name(&mut self, name: u32) {
        let cache = self.new_name_cache();
        self.emit(Op::StoreNameCached(name, cache));
    }
    /// Declare every binding a lexical declaration pattern introduces, in source order (slot +
    /// TDZ each, like the plain-identifier path). Only the destructuring subset the compiler
    /// can lower is accepted (see `destructure_store`); anything else bails to the tree-walker.
    /// A body-level lexical destructuring declaration: like `declare_lexical_pattern`, with
    /// captured leaves homed in the activation env (in TDZ from entry via `CapInit::Lexical`).
    fn declare_body_pattern(
        &mut self,
        pat: &Pattern,
        is_const: bool,
        captured: &std::collections::HashSet<String>,
    ) -> CResult {
        match pat {
            Pattern::Ident(name) if captured.contains(name) => {
                self.cap_inits
                    .push(CapInit::Lexical(Rc::from(name.as_str()), is_const));
                self.env_bind(name, is_const);
                Ok(())
            }
            Pattern::Ident(_) => self.declare_lexical_pattern(pat, is_const),
            Pattern::Object(o) => {
                for prop in &o.props {
                    self.declare_body_pattern(&prop.value, is_const, captured)?;
                }
                if let Some(r) = &o.rest {
                    self.declare_body_pattern(&Pattern::Ident(r.clone()), is_const, captured)?;
                }
                Ok(())
            }
            Pattern::Array(elems) => {
                for e in elems {
                    match e {
                        ArrayPatElem::Hole => {}
                        // (Defaults / rest elements: the sequential walk, `destructure_seq`.)
                        ArrayPatElem::Elem { pattern, .. } | ArrayPatElem::Rest(pattern) => {
                            self.declare_body_pattern(pattern, is_const, captured)?
                        }
                    }
                }
                Ok(())
            }
            _ => Err(Bail),
        }
    }

    fn declare_lexical_pattern(&mut self, pat: &Pattern, is_const: bool) -> CResult {
        match pat {
            Pattern::Ident(name) => {
                if self.homed_pending.remove(name) {
                    // The homed block `let`'s own declaration (see `Compiler::homed_lets`):
                    // in TDZ since entry, no slot, no per-entry Tdz (the block runs at most
                    // once per call by construction). Consumed so any LATER same-name
                    // declaration (a nested for-of head, a sibling block) slot-shadows.
                    return Ok(());
                }
                if self.blk_names.contains(name) {
                    // Captured: homes in the scope's block env (opened by `blk_flush`).
                    self.pending_blk.push((name.clone(), is_const));
                    return Ok(());
                }
                let slot = self.fresh_slot(name);
                self.scope_bind(name, slot, is_const);
                self.tdz_slots.insert(slot);
                self.emit(Op::Tdz(slot));
                Ok(())
            }
            Pattern::Object(o) => {
                for prop in &o.props {
                    self.declare_lexical_pattern(&prop.value, is_const)?;
                }
                if let Some(r) = &o.rest {
                    self.declare_lexical_pattern(&Pattern::Ident(r.clone()), is_const)?;
                }
                Ok(())
            }
            Pattern::Array(elems) => {
                for e in elems {
                    match e {
                        ArrayPatElem::Hole => {}
                        // (Defaults / rest elements: the sequential walk, `destructure_seq`.)
                        ArrayPatElem::Elem { pattern, .. } | ArrayPatElem::Rest(pattern) => {
                            self.declare_lexical_pattern(pattern, is_const)?
                        }
                    }
                }
                Ok(())
            }
            _ => Err(Bail),
        }
    }

    /// A destructuring default: the value on top of the stack is replaced by `d`'s value when
    /// it is `undefined` (anonymous functions named after an identifier target, as the oracle
    /// does after evaluation).
    fn pattern_default(&mut self, d: &Expr, target: &Pattern) -> CResult {
        self.emit(Op::Dup);
        self.emit(Op::Undef);
        self.emit(Op::StrictEq);
        let skip = self.emit(Op::JumpIfFalse(0));
        self.emit(Op::Pop);
        match (target, d) {
            (Pattern::Ident(n), Expr::Func(f)) => self.emit_closure(f, Some(n)),
            (Pattern::Ident(_), Expr::Class(c)) if c.name.is_none() => return Err(Bail),
            _ => self.expr(d)?,
        }
        self.patch(skip);
        Ok(())
    }

    /// Lower a declaration destructuring against the value on the stack (consumed): the
    /// KeyedBindingInitialization subset with plain (non-computed) keys, no defaults, no rest —
    /// per property: Dup + GetProp (the oracle's GetV), recursing into nested object patterns.
    /// The nullish guard throws the oracle's exact TypeError before any read.
    fn destructure_store(&mut self, pat: &Pattern, kind: DeclKind) -> CResult {
        match pat {
            Pattern::Ident(name) if self.assign_mode => self.assign_leaf(name),
            Pattern::Ident(name) => {
                let home = self.home(name).ok_or(Bail)?;
                match home {
                    Home::Slot(slot, _) => {
                        if matches!(kind, DeclKind::Var) && self.tdz_pending.contains(&slot) {
                            return Err(Bail);
                        }
                        self.init_slot(slot);
                    }
                    Home::Env(_) => {
                        let n = self.name_idx(name);
                        if matches!(kind, DeclKind::Var) {
                            self.emit(Op::StoreCap(n));
                        } else {
                            self.emit(Op::StoreCapInit(n));
                        }
                    }
                    Home::Blk(c, _) => {
                        let n = self.name_idx(name);
                        if matches!(kind, DeclKind::Var) {
                            self.emit(Op::BlkStore(c, n));
                        } else {
                            self.emit(Op::BlkInit(c, n));
                        }
                    }
                }
                Ok(())
            }
            Pattern::Object(o) => {
                // The rest copy excludes the keys read before it: static keys only.
                let mut rest_keys: Vec<String> = Vec::new();
                if o.rest.is_some() {
                    for prop in &o.props {
                        match &prop.key {
                            PropKey::Ident(k) => rest_keys.push(k.clone()),
                            PropKey::Str(k) => rest_keys.push(k.to_string()),
                            _ => return Err(Bail),
                        }
                    }
                }
                self.emit(Op::DestructureGuard);
                for prop in &o.props {
                    // Per property, the oracle's order: key (computed: evaluate + ToPropertyKey),
                    // GetV, the default when undefined, then the binding.
                    self.emit(Op::Dup);
                    match &prop.key {
                        PropKey::Ident(k) => {
                            let ki = self.name_idx(k);
                            let c = self.new_cache();
                            self.emit(Op::GetProp(ki, c));
                        }
                        PropKey::Str(k) => {
                            let ki = self.name_idx(k);
                            let c = self.new_cache();
                            self.emit(Op::GetProp(ki, c));
                        }
                        PropKey::Num(x) => {
                            let ci = self.const_idx(Value::Num(*x));
                            self.emit(Op::Const(ci));
                            self.emit(Op::GetElem);
                        }
                        PropKey::Computed(e) => {
                            self.expr(e)?;
                            self.emit(Op::ToPropKey);
                            self.emit(Op::GetElem);
                        }
                    }
                    if let Some(d) = &prop.default {
                        self.pattern_default(d, &prop.value)?;
                    }
                    self.destructure_store(&prop.value, kind)?;
                }
                if let Some(r) = &o.rest {
                    let start = self.names.len() as u32;
                    let count = u16::try_from(rest_keys.len()).map_err(|_| Bail)?;
                    self.names
                        .extend(rest_keys.iter().map(|k| Rc::from(k.as_str())));
                    self.emit(Op::ObjRest(start, count));
                    self.destructure_store(&Pattern::Ident(r.clone()), kind)?;
                }
                self.emit(Op::Pop);
                Ok(())
            }
            Pattern::Array(elems) => {
                // Batched iterator walk (Op::DestructureArr), then stores in reverse. Batching
                // is only order-unobservable when every leaf is an UNCAPTURED slot (an env-homed
                // leaf's initialization is visible to a later iterator step's next() per spec)
                // and elements are flat idents/holes (a nested pattern's own reads would
                // interleave with the steps), with no defaults (their evaluation interleaves).
                // The reversed stores also need distinct names (`var [a, a] = [1, 2]` leaves 2).
                let mut seen = std::collections::HashSet::new();
                for e in elems.iter() {
                    match e {
                        ArrayPatElem::Hole => {}
                        ArrayPatElem::Elem {
                            pattern: Pattern::Ident(n),
                            default: None,
                        } if (matches!(self.home(n), Some(Home::Slot(..)))
                            || self.fresh_blk && matches!(self.home(n), Some(Home::Blk(..))))
                            && seen.insert(n.as_str()) => {}
                        // Anything else steps the iterator one element at a time.
                        _ => return self.destructure_array_seq(elems, kind),
                    }
                }
                self.emit(Op::DestructureArr(elems.len() as u16));
                for e in elems.iter().rev() {
                    match e {
                        ArrayPatElem::Hole => {
                            self.emit(Op::Pop);
                        }
                        ArrayPatElem::Elem { pattern, .. } => {
                            self.destructure_store(pattern, kind)?;
                        }
                        ArrayPatElem::Rest(_) => unreachable!("filtered above"),
                    }
                }
                Ok(())
            }
            _ => Err(Bail),
        }
    }

    /// Emit a call's arguments left to right. `Ok(true)`: the last argument was a spread —
    /// the caller must emit a `CallSpread` op (evaluate-then-expand only matches the spec's
    /// interleaved order when nothing evaluates after the spread part, so any other spread
    /// position bails).
    /// After `GetMethod(apply)`: `f.apply(t, arguments)` forwarding the virtual `arguments`
    /// becomes [`Op::ApplyArgs`] (the object is never built). `false` (nothing emitted) for any
    /// other call.
    fn apply_args(&mut self, prop: &str, args: &[ArrayElem]) -> Result<bool, Bail> {
        let (true, [ArrayElem::Item(t), ArrayElem::Item(Expr::Ident(a))]) = (prop == "apply", args)
        else {
            return Ok(false);
        };
        let Some(Home::Slot(slot, _)) = self.home(a) else {
            return Ok(false);
        };
        let Some(tag) = self
            .virt
            .as_ref()
            .filter(|v| v.slot == slot && !v.rest)
            .map(VirtC::tag)
        else {
            return Ok(false);
        };
        self.expr(t)?;
        self.emit(Op::ApplyArgs(slot, tag));
        Ok(true)
    }

    fn call_args(&mut self, args: &[ArrayElem]) -> Result<bool, Bail> {
        let spread_at = args.iter().position(|a| !matches!(a, ArrayElem::Item(_)));
        if let Some(k) = spread_at {
            if k != args.len() - 1 || !matches!(args[k], ArrayElem::Spread(_)) {
                log_bail("expr", "spread argument (non-final)");
                return Err(Bail);
            }
        }
        for a in args {
            match a {
                ArrayElem::Item(e) | ArrayElem::Spread(e) => self.expr(e)?,
                ArrayElem::Hole => return Err(Bail),
            }
        }
        Ok(spread_at.is_some())
    }

    /// `delete obj.p` / `delete obj[k]` on plain (non-optional, non-super, public) references;
    /// a non-reference operand evaluates for its effects and deletes to `true`. Identifier
    /// deletes (env bindings) and optional chains stay in the oracle.
    fn delete_expr(&mut self, arg: &Expr) -> CResult {
        match arg {
            Expr::Paren(inner) => self.delete_expr(inner),
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.expr(obj)?;
                let n = self.name_idx(prop);
                self.emit(Op::DeleteProp(n, self.strict));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                self.expr(obj)?;
                self.expr(index)?;
                self.emit(Op::DeleteElem(self.strict));
                Ok(())
            }
            Expr::Ident(_) | Expr::OptionalChain(_) => Err(Bail),
            // `delete super.x` / `delete super[k]` throw a ReferenceError (the oracle's order).
            Expr::Member { obj, .. } | Expr::Index { obj, .. } if matches!(**obj, Expr::Super) => {
                Err(Bail)
            }
            other => {
                self.expr(other)?;
                self.emit(Op::Pop);
                let k = self.const_idx(Value::Bool(true));
                self.emit(Op::Const(k));
                Ok(())
            }
        }
    }

    /// Compile an optional chain (`a?.b.c`, `r?.m(args)`): each optional link peeks its base —
    /// nullish pops what the link would have consumed and jumps to a shared pad that pushes the
    /// chain's `undefined` result (skipping every later link, key expression, and argument, per
    /// spec). Non-optional links compile as usual. Supported spine: Member/Index/private reads,
    /// method calls on Member/Index/private callees, calls of a free identifier (`f?.()`, with
    /// its reference this value) or of any other chain value, spread in the last argument. Optional
    /// `delete`, `super` links and `eval?.()` bail to the tree-walker.
    fn opt_chain(&mut self, e: &Expr, shorts: &mut Vec<usize>) -> CResult {
        match e {
            Expr::Member {
                obj,
                prop,
                optional,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.opt_chain(obj, shorts)?;
                if *optional {
                    self.opt_link(1, shorts);
                }
                let i = self.name_idx(prop);
                let c = self.new_cache();
                self.emit(Op::GetProp(i, c));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional,
            } if !matches!(**obj, Expr::Super) => {
                self.opt_chain(obj, shorts)?;
                if *optional {
                    self.opt_link(1, shorts);
                }
                self.expr(index)?;
                self.emit(Op::GetElem);
                Ok(())
            }
            Expr::Call {
                callee,
                args,
                optional: call_opt,
                pos,
            } => {
                if matches!(&**callee, Expr::Ident(n) if n == "eval") {
                    return Err(Bail); // `eval?.(x)` is an indirect eval, but keep it simple
                }
                let saved_site = std::mem::replace(&mut self.site, pos.wrapping_add(1));
                match &**callee {
                    Expr::Member {
                        obj,
                        prop,
                        optional,
                    } if !matches!(**obj, Expr::Super) => {
                        self.opt_chain(obj, shorts)?;
                        if *optional {
                            self.opt_link(1, shorts);
                        }
                        let i = self.name_idx(prop);
                        if prop.starts_with('#') {
                            self.emit(Op::GetPrivateMethod(i));
                        } else {
                            let c = self.new_cache();
                            self.emit(Op::GetMethod(i, c));
                        }
                        if *call_opt {
                            // `a.b?.(args)`: the method value is peeked; nullish drops
                            // [obj, method].
                            self.opt_link(2, shorts);
                        }
                        if !*call_opt && self.apply_args(prop, args)? {
                            self.site = saved_site;
                            return Ok(());
                        }
                        if self.call_args(args)? {
                            self.emit(Op::CallSpreadThis(args.len() as u16));
                        } else {
                            self.emit(Op::CallWithThis(args.len() as u16));
                        }
                    }
                    Expr::Index {
                        obj,
                        index,
                        optional,
                    } if !matches!(**obj, Expr::Super) => {
                        self.opt_chain(obj, shorts)?;
                        if *optional {
                            self.opt_link(1, shorts);
                        }
                        self.expr(index)?;
                        self.emit(Op::GetMethodElem);
                        if *call_opt {
                            self.opt_link(2, shorts);
                        }
                        if self.call_args(args)? {
                            self.emit(Op::CallSpreadThis(args.len() as u16));
                        } else {
                            self.emit(Op::CallWithThis(args.len() as u16));
                        }
                    }
                    Expr::Member { .. } | Expr::Index { .. } | Expr::Super => return Err(Bail),
                    // `f?.(args)`, `g(x)?.(y)`: a plain callee (this = undefined). A free name
                    // resolves through the scope chain like `LoadName` — compiled bodies never
                    // sit under a `with`, so no base object can supply a receiver.
                    Expr::Ident(name) if self.home(name).is_none() => {
                        let i = self.name_idx(name);
                        let c = self.new_name_cache();
                        self.emit(Op::LoadNameForCall(i, c));
                        if *call_opt {
                            self.opt_link(2, shorts);
                        }
                        if self.call_args(args)? {
                            self.emit(Op::CallSpreadThis(args.len() as u16));
                        } else {
                            self.emit(Op::CallWithThis(args.len() as u16));
                        }
                    }
                    other => {
                        self.opt_chain(other, shorts)?;
                        if *call_opt {
                            self.opt_link(1, shorts);
                        }
                        if self.call_args(args)? {
                            self.emit(Op::CallSpread(args.len() as u16));
                        } else {
                            self.emit(Op::Call(args.len() as u16));
                        }
                    }
                }
                self.site = saved_site;
                Ok(())
            }
            Expr::Member {
                obj,
                prop,
                optional,
            } if !matches!(**obj, Expr::Super) => {
                // Private name link (`a?.#x`, `a?.b.#x`).
                self.opt_chain(obj, shorts)?;
                if *optional {
                    self.opt_link(1, shorts);
                }
                let n = self.name_idx(prop);
                self.emit(Op::GetPrivate(n));
                Ok(())
            }
            // The chain's base (before any `?.` link): an ordinary expression.
            other => self.expr(other),
        }
    }

    /// One optional link: fall through when the top of stack isn't nullish; otherwise pop the
    /// `depth` values the rest of the link would consume and jump to the chain's undefined pad.
    fn opt_link(&mut self, depth: u16, shorts: &mut Vec<usize>) {
        let cont = self.emit(Op::JumpIfNotNullishPeek(0));
        for _ in 0..depth {
            self.emit(Op::Pop);
        }
        shorts.push(self.emit(Op::Jump(0)));
        self.patch(cont);
    }

    /// The local slot for a fused element access (`x[k]` → `GetElemLocal`), or `None` to use
    /// the generic ops. Fusing defers the base-local read past the key/value evaluation, so it
    /// requires: the base is an Ident homed in a slot that can never be in TDZ (a param or a
    /// `var` — no early throw to reorder; see `tdz_slots`), and no `deps` expression can
    /// reassign that local (calls can't — slot locals are unobservable outside the function;
    /// only an explicit assignment/update in the key/value expressions themselves could, and
    /// `no_assign_to` rejects those).
    fn fused_elem_slot(&self, obj: &Expr, deps: &[&Expr]) -> Option<u16> {
        let Expr::Ident(name) = obj else { return None };
        let Some(Home::Slot(slot, _)) = self.home(name) else {
            return None;
        };
        if self.tdz_slots.contains(&slot) {
            return None;
        }
        if deps.iter().all(|d| no_assign_to(d, name)) {
            Some(slot)
        } else {
            None
        }
    }
    fn fresh_slot(&mut self, name: &str) -> u16 {
        let slot = self.slot_names.len() as u16;
        self.slot_names.push(Rc::from(name));
        slot
    }
    fn scope_bind(&mut self, name: &str, slot: u16, is_const: bool) {
        if self.scopes.is_empty() {
            self.scopes.push(Vec::new());
        }
        let top = self.scopes.last_mut().unwrap();
        if let Some(e) = top.iter_mut().find(|(n, ..)| n == name) {
            *e = (name.to_string(), slot, is_const);
        } else {
            top.push((name.to_string(), slot, is_const));
        }
    }
    fn lookup(&self, name: &str) -> Option<(u16, bool)> {
        for scope in self.scopes.iter().rev() {
            if let Some((_, slot, k)) = scope.iter().rev().find(|(n, ..)| n == name) {
                return Some((*slot, *k));
            }
        }
        None
    }
    fn env_bind(&mut self, name: &str, is_const: bool) {
        self.env_names.insert(name.to_string(), is_const);
    }
    fn env_has(&self, name: &str) -> bool {
        self.env_names.contains_key(name)
    }
    /// Resolve a local: innermost slot scope first (block lexicals shadow captured names — a
    /// captured block lexical bails compile, so every env name is function-scope-wide).
    fn home(&self, name: &str) -> Option<Home> {
        if let Some((slot, k)) = self.lookup(name) {
            if slot & BLK_BIT != 0 {
                return Some(Home::Blk(slot & !BLK_BIT, k));
            }
            return Some(Home::Slot(slot, k));
        }
        self.env_names.get(name).map(|k| Home::Env(*k))
    }
    fn const_idx(&mut self, v: Value) -> u32 {
        self.consts.push(v);
        (self.consts.len() - 1) as u32
    }
    fn name_idx(&mut self, name: &str) -> u32 {
        if let Some(i) = self.names.iter().position(|n| &**n == name) {
            return i as u32;
        }
        self.names.push(intern_name(name));
        (self.names.len() - 1) as u32
    }
    /// Compile `test` and a jump taken when it is falsy; returns the jump to patch. A comparison
    /// fuses into the jump ([`Op::JumpIfNotCmp`]): nothing inside `test` can jump between the
    /// compare and the branch, since a comparison's own operands end at the compare.
    fn jump_if_false(&mut self, test: &Expr) -> Result<usize, Bail> {
        if let Expr::Binary { op, left, right } = test {
            if let Some(kind) = CmpKind::of(op) {
                if typeof_test(op, left, right).is_none() {
                    let at = self.ops.len();
                    self.expr(left)?;
                    self.expr(right)?;
                    // Both operands single loads: nothing can target the second, so the three ops
                    // collapse into one at the first's index (where a loop head may point).
                    if self.ops.len() == at + 2 {
                        let fused = match (self.ops[at], self.ops[at + 1]) {
                            (Op::LoadLocal(a), Op::LoadLocal(b)) => {
                                Some(Op::JumpIfNotCmpLL(kind, a, b, 0))
                            }
                            (Op::LoadLocal(a), Op::Const(k)) => {
                                Some(Op::JumpIfNotCmpLK(kind, a, k, 0))
                            }
                            _ => None,
                        };
                        if let Some(op) = fused {
                            self.ops.truncate(at);
                            return Ok(self.emit(op));
                        }
                    }
                    return Ok(self.emit(Op::JumpIfNotCmp(kind, 0)));
                }
            }
        }
        self.expr(test)?;
        Ok(self.emit(Op::JumpIfFalse(0)))
    }

    fn patch(&mut self, at: usize) {
        let target = self.ops.len() as u32;
        match &mut self.ops[at] {
            Op::Jump(t)
            | Op::JumpIfFalse(t)
            | Op::JumpIfNotCmp(_, t)
            | Op::JumpIfNotCmpLL(.., t)
            | Op::JumpIfNotCmpLK(.., t)
            | Op::JumpIfFalsePeek(t)
            | Op::JumpIfTruePeek(t)
            | Op::JumpIfNotNullishPeek(t) => *t = target,
            _ => unreachable!("patching a non-jump"),
        }
    }

    /// Declare the function body's top-level `let`/`const`: captured ones home in the activation
    /// env (inserted in TDZ by `make_run_env`), the rest get TDZ slots. Function declarations
    /// were already handled by the hoist plan; classes and `using` bail.
    fn declare_body_lexicals(
        &mut self,
        stmts: &[Stmt],
        captured: &std::collections::HashSet<String>,
    ) -> CResult {
        for s in stmts {
            match s {
                Stmt::VarDecl {
                    kind: kind @ (DeclKind::Let | DeclKind::Const),
                    decls,
                } => {
                    let is_const = matches!(kind, DeclKind::Const);
                    for (pat, _) in decls {
                        // Patterns declare every bound ident; a captured one homes in the
                        // activation env like the plain-ident path.
                        let mut names = std::collections::HashSet::new();
                        pat_idents(pat, &mut names);
                        if !matches!(pat, Pattern::Ident(_)) {
                            // Captured leaves home in the activation (initialized by
                            // `destructure_store`'s StoreCapInit), the rest get TDZ slots.
                            self.declare_body_pattern(pat, is_const, captured)?;
                            continue;
                        }
                        let Pattern::Ident(name) = pat else {
                            unreachable!()
                        };
                        if captured.contains(name) {
                            self.cap_inits
                                .push(CapInit::Lexical(Rc::from(name.as_str()), is_const));
                            self.env_bind(name, is_const);
                        } else {
                            let slot = self.fresh_slot(name);
                            self.scope_bind(name, slot, is_const);
                            self.tdz_slots.insert(slot);
                            self.emit(Op::Tdz(slot));
                        }
                    }
                }
                Stmt::ClassDecl(c) => {
                    let Some(name) = &c.name else {
                        return Err(Bail);
                    };
                    if captured.contains(name) {
                        self.cap_inits
                            .push(CapInit::Lexical(Rc::from(name.as_str()), false));
                        self.env_bind(name, false);
                    } else {
                        let slot = self.fresh_slot(name);
                        self.scope_bind(name, slot, false);
                        self.tdz_slots.insert(slot);
                        self.emit(Op::Tdz(slot));
                    }
                }
                Stmt::VarDecl {
                    kind: DeclKind::Using | DeclKind::AwaitUsing,
                    ..
                } => return Err(Bail),
                Stmt::FuncDecl(_) => {} // hoisted — created at entry
                _ => {}
            }
        }
        Ok(())
    }

    /// Declare a statement list's `let`/`const` as TDZ slots (block entry).
    fn declare_block_lexicals(&mut self, stmts: &[Stmt]) -> CResult {
        for s in stmts {
            match s {
                Stmt::VarDecl {
                    kind: DeclKind::Let | DeclKind::Const,
                    decls,
                } => {
                    let is_const = matches!(
                        s,
                        Stmt::VarDecl {
                            kind: DeclKind::Const,
                            ..
                        }
                    );
                    for (pat, _) in decls {
                        self.declare_lexical_pattern(pat, is_const)?;
                    }
                }
                Stmt::ClassDecl(c) => {
                    let Some(name) = &c.name else {
                        return Err(Bail);
                    };
                    if self.blk_names.contains(name) {
                        self.pending_blk.push((name.clone(), false));
                        continue;
                    }
                    let slot = self.fresh_slot(name);
                    self.scope_bind(name, slot, false);
                    self.tdz_slots.insert(slot);
                    self.emit(Op::Tdz(slot));
                }
                // A block-level function (strict code — sloppy Annex B bodies never compile): a
                // mutable binding initialized at block entry (see `block_body_inner`).
                Stmt::FuncDecl(f) => {
                    let Some(name) = &f.name else {
                        return Err(Bail);
                    };
                    if self.homed_pending.remove(name) {
                        continue; // homed in the activation (in TDZ until block entry)
                    }
                    if self.blk_names.contains(name) {
                        self.pending_blk.push((name.clone(), false));
                        continue;
                    }
                    let slot = self.fresh_slot(name);
                    self.scope_bind(name, slot, false);
                }
                Stmt::VarDecl {
                    kind: DeclKind::Using | DeclKind::AwaitUsing,
                    ..
                } => return Err(Bail),
                _ => {}
            }
        }
        Ok(())
    }

    /// Instantiate a block's function declarations (block entry, after its env exists).
    fn init_block_functions(&mut self, stmts: &[Stmt]) -> CResult {
        for s in stmts {
            let Stmt::FuncDecl(f) = s else { continue };
            let Some(name) = &f.name else {
                return Err(Bail);
            };
            self.emit_closure(f, None);
            match self.home(name) {
                Some(Home::Slot(slot, _)) => self.init_slot(slot),
                Some(Home::Env(_)) => {
                    let n = self.name_idx(name);
                    self.emit(Op::StoreCapInit(n));
                }
                Some(Home::Blk(c, _)) => {
                    let n = self.name_idx(name);
                    self.emit(Op::BlkInit(c, n));
                }
                None => return Err(Bail),
            }
        }
        Ok(())
    }

    fn stmt(&mut self, s: &Stmt) -> CResult {
        if crate::stack::exhausted() {
            return Err(Bail);
        }
        let r = self.stmt_inner(s);
        if r.is_err() {
            note_bail_reason(|| format!("stmt {}", node_kind(s)));
        }
        r
    }

    fn stmt_inner(&mut self, s: &Stmt) -> CResult {
        match s {
            Stmt::Expr(e) => self.expr_stmt(e),
            Stmt::Empty | Stmt::Debugger => Ok(()),
            // Top-level function declarations were hoisted (created at entry); block-level ones
            // never reach here (declare_block_lexicals bails first).
            Stmt::FuncDecl(_) => Ok(()),
            Stmt::ClassDecl(c) => {
                let Some(name) = &c.name else {
                    return Err(Bail);
                };
                let home = self.home(name).ok_or(Bail)?;
                self.class_value(c, None)?;
                match home {
                    Home::Slot(slot, _) => {
                        self.init_slot(slot);
                    }
                    Home::Env(_) => {
                        let n = self.name_idx(name);
                        self.emit(Op::StoreCapInit(n));
                    }
                    Home::Blk(c, _) => {
                        let n = self.name_idx(name);
                        self.emit(Op::BlkInit(c, n));
                    }
                }
                Ok(())
            }
            Stmt::VarDecl { kind, decls } => {
                if matches!(kind, DeclKind::Using | DeclKind::AwaitUsing) {
                    return Err(Bail);
                }
                for (pat, init) in decls {
                    let Pattern::Ident(name) = pat else {
                        // Destructuring declaration: evaluate the initializer, then lower the
                        // pattern against it (a pattern without an initializer is a parse error).
                        let Some(e) = init else { return Err(Bail) };
                        self.expr(e)?;
                        self.destructure_store(pat, *kind)?;
                        continue;
                    };
                    let home = self.home(name).ok_or(Bail)?;
                    match init {
                        Some(e) => self.named_expr(e, name)?,
                        // `var x;` leaves an existing binding alone; `let x;` initializes.
                        None => {
                            if matches!(kind, DeclKind::Var) {
                                continue;
                            }
                            self.emit(Op::Undef);
                        }
                    }
                    match home {
                        Home::Slot(slot, _) => {
                            if matches!(kind, DeclKind::Var) && self.tdz_pending.contains(&slot) {
                                return Err(Bail);
                            }
                            self.init_slot(slot);
                        }
                        Home::Env(_) => {
                            let n = self.name_idx(name);
                            // A lexical declaration initializes (clearing TDZ); a `var` writes an
                            // already-initialized binding.
                            if matches!(kind, DeclKind::Var) {
                                self.emit(Op::StoreCap(n));
                            } else {
                                self.emit(Op::StoreCapInit(n));
                            }
                        }
                        Home::Blk(c, _) => {
                            if matches!(kind, DeclKind::Var) {
                                return Err(Bail);
                            }
                            let n = self.name_idx(name);
                            self.emit(Op::BlkInit(c, n));
                        }
                    }
                }
                Ok(())
            }
            Stmt::Return(arg) if self.inline_ret.is_some() => self.inline_return(arg.as_ref()),
            Stmt::Return(Some(e)) if self.tail_position() => self.tail_return(e),
            Stmt::Return(arg) => {
                // For-of closes and crossed finally regions: see `emit_return_tail`.
                match arg {
                    Some(e) => {
                        self.expr(e)?;
                        // An async generator's `return <expr>` awaits the operand.
                        if self.async_gen {
                            self.emit(Op::Await);
                        }
                    }
                    None => {
                        self.emit(Op::Undef);
                    }
                }
                self.emit_return_tail()
            }
            Stmt::Throw(e) => {
                self.expr(e)?;
                self.emit(Op::Throw);
                Ok(())
            }
            Stmt::If { test, cons, alt } => {
                let jf = self.jump_if_false(test)?;
                self.stmt(cons)?;
                match alt {
                    Some(a) => {
                        let jend = self.emit(Op::Jump(0));
                        self.patch(jf);
                        self.stmt(a)?;
                        self.patch(jend);
                    }
                    None => self.patch(jf),
                }
                Ok(())
            }
            Stmt::Block(body) => {
                self.scopes.push(Vec::new());
                let r = self.block_body(body);
                self.scopes.pop();
                r
            }
            Stmt::While { test, body } => {
                let labels = std::mem::take(&mut self.pending_labels);
                let start = self.ops.len();
                let jf = self.jump_if_false(test)?;
                self.loops.push(LoopCtx {
                    labels,
                    entry_try_depth: self.try_depth,
                    ..LoopCtx::default()
                });
                let r = self.stmt(body);
                let ctx = self.loops.pop().unwrap();
                r?;
                for c in ctx.continues {
                    match &mut self.ops[c] {
                        Op::Jump(t) => *t = start as u32,
                        _ => unreachable!(),
                    }
                }
                self.emit(Op::Jump(start as u32));
                self.patch(jf);
                for b in ctx.breaks {
                    self.patch(b);
                }
                Ok(())
            }
            Stmt::DoWhile { body, test } => {
                let labels = std::mem::take(&mut self.pending_labels);
                let start = self.ops.len();
                self.loops.push(LoopCtx {
                    labels,
                    entry_try_depth: self.try_depth,
                    ..LoopCtx::default()
                });
                let r = self.stmt(body);
                let ctx = self.loops.pop().unwrap();
                r?;
                let cont = self.ops.len();
                for c in ctx.continues {
                    match &mut self.ops[c] {
                        Op::Jump(t) => *t = cont as u32,
                        _ => unreachable!(),
                    }
                }
                let jf = self.jump_if_false(test)?;
                self.emit(Op::Jump(start as u32));
                self.patch(jf);
                for b in ctx.breaks {
                    self.patch(b);
                }
                Ok(())
            }
            Stmt::For {
                init,
                test,
                update,
                body,
            } => {
                self.scopes.push(Vec::new());
                let depth = self.blk_envs.len();
                let r = self.for_loop(init.as_deref(), test.as_deref(), update.as_deref(), body);
                self.blk_envs.truncate(depth);
                self.scopes.pop();
                r
            }
            Stmt::Break(None) => {
                let idx = self.loops.iter().rposition(|c| !c.label_only).ok_or(Bail)?;
                self.emit_jump_exit(idx, false)
            }
            Stmt::Continue(None) => {
                // `continue` skips switch contexts: it targets the innermost enclosing *loop*.
                let idx = self
                    .loops
                    .iter()
                    .rposition(|c| !c.is_switch && !c.label_only)
                    .ok_or(Bail)?;
                self.emit_jump_exit(idx, true)
            }
            // Labelled break/continue: jump to the loop on the stack that carries the target label.
            // A `break` to a labelled *block* (not a loop) isn't modeled here — no ctx matches, so
            // it bails to the interpreter.
            Stmt::Break(Some(name)) => {
                let idx = self
                    .loops
                    .iter()
                    .rposition(|c| c.labels.iter().any(|l| l == name))
                    .ok_or(Bail)?;
                self.emit_jump_exit(idx, false)
            }
            Stmt::Continue(Some(name)) => {
                // A labelled continue must target a loop — a label on a switch is only a break
                // target (the parser rejects `continue` to it; not-found bails to the oracle).
                let idx = self
                    .loops
                    .iter()
                    .rposition(|c| {
                        !c.is_switch && !c.label_only && c.labels.iter().any(|l| l == name)
                    })
                    .ok_or(Bail)?;
                self.emit_jump_exit(idx, true)
            }
            // A label naming a loop or switch attaches to that context; stacked labels
            // (`a: b: for`) accumulate through the recursion. A label on any other statement bails.
            Stmt::Labeled { label, body } => match &**body {
                Stmt::While { .. }
                | Stmt::DoWhile { .. }
                | Stmt::For { .. }
                | Stmt::Switch { .. }
                | Stmt::ForInOf { .. }
                | Stmt::Labeled { .. } => {
                    self.pending_labels.push(label.clone());
                    self.stmt(body)
                }
                // Any other statement: a break-only target (`l: { … break l; … }`).
                _ => {
                    let mut labels = std::mem::take(&mut self.pending_labels);
                    labels.push(label.clone());
                    self.loops.push(LoopCtx {
                        labels,
                        entry_try_depth: self.try_depth,
                        label_only: true,
                        ..LoopCtx::default()
                    });
                    let r = self.stmt(body);
                    let ctx = self.loops.pop().expect("just pushed");
                    r?;
                    for b in ctx.breaks {
                        self.patch(b);
                    }
                    Ok(())
                }
            },
            // `switch`: the discriminant lands in a hidden slot; case tests run in source order
            // (exactly the oracle's two-phase evaluation), then bodies are laid out contiguously
            // so fall-through is just falling through. Any lexical/class/function declaration
            // directly in a case body bails — the oracle gives all cases one shared block scope
            // whose TDZ interleavings slots don't model.
            Stmt::Switch { disc, cases } => self.switch_statement(disc, cases),
            // `try { ... } catch (e?) { ... }` (catch param an ident or none): on a throw in the
            // try region the VM unwinds to the catch pad with the exception pushed. With a
            // `finally`, see `bytecode/finally.rs`.
            Stmt::Try {
                block,
                handler,
                finalizer: Some(fin),
            } => self.try_finally(block, handler.as_deref(), fin),
            Stmt::Try { block, handler, .. } => self.try_catch(block, handler.as_deref()),
            Stmt::ForInOf {
                decl: Some(kind @ (DeclKind::Let | DeclKind::Const)),
                left: Pattern::Ident(name),
                right,
                of: false,
                is_await: false,
                body,
            } => self.for_in_statement(*kind, name, right, body),
            Stmt::ForInOf {
                decl: None | Some(DeclKind::Var),
                left: Pattern::Ident(name),
                right,
                of: false,
                is_await: false,
                body,
            } => self.for_in_assign(name, right, body),
            // `for (x of it)`: the iterator and its `next` live in hidden slots; each step is
            // IterStepL + JumpIfFalse (existing branch machinery in both tiers); the body runs
            // under a per-iteration handler whose pad closes the iterator in throw mode and
            // rethrows. Exhaustion closes nothing (spec); break/return close via
            // `emit_exit_cleanup` / the Return arm. Other for-in heads, `for await`, and
            // captured loop variables stay on the tree-walker.
            Stmt::ForInOf {
                decl,
                left,
                right,
                of: true,
                is_await,
                body,
            } => {
                let is_await = *is_await;
                let labels = std::mem::take(&mut self.pending_labels);
                // The loop variable: a declaration binds a fresh (uncaptured — else the
                // per-iteration env freshness matters and we bail) slot scoped to the loop; a
                // bare identifier assigns an existing binding or a free name. Spec order for a
                // lexical declaration: the fresh binding exists — in TDZ — while the iterable
                // expression evaluates (`for (const x of [x])` throws a ReferenceError), so the
                // scope and Tdz emit BEFORE `right`.
                self.scopes.push(Vec::new());
                let blk_depth = self.blk_envs.len();
                enum Bind {
                    Slot(u16),
                    Cap(u32),
                    Name(u32),
                    /// A captured lexical head: a fresh block env per iteration (carrier slot,
                    /// its parent operand, the declaration).
                    Blk(u16, u16, Vec<(String, bool)>),
                    /// A `var`/assignment head naming a captured block binding.
                    BlkStore(u16, u32),
                    /// Destructuring lexical head: bound by `destructure_store` INSIDE the
                    /// body's handler region (a binding throw must IteratorClose in throw mode,
                    /// which is exactly what the body's abort pad does). Captured leaves live
                    /// in a per-iteration block env (carrier, parent, declarations).
                    Pattern(DeclKind, Option<(u16, u16, Vec<(String, bool)>)>),
                }
                let bind = match (left, decl) {
                    (_, Some(DeclKind::Using | DeclKind::AwaitUsing)) => {
                        self.scopes.pop();
                        return Err(Bail);
                    }
                    (Pattern::Ident(name), Some(kind @ (DeclKind::Let | DeclKind::Const)))
                        if self.blk_names.contains(name) =>
                    {
                        // Captured: the TDZ env while `right` evaluates, then a fresh one per
                        // iteration.
                        let names = vec![(name.clone(), matches!(kind, DeclKind::Const))];
                        match self.blk_open(&names) {
                            Ok((slot, parent)) => Bind::Blk(slot, parent, names),
                            Err(b) => {
                                self.scopes.pop();
                                return Err(b);
                            }
                        }
                    }
                    (Pattern::Ident(name), Some(kind @ (DeclKind::Let | DeclKind::Const))) => {
                        // An env-homed name blocks a head slot — except a homed block `let`
                        // (`Compiler::homed_lets`), which a fresh slot shadows correctly.
                        if self.env_names.contains_key(name) && !self.homed_lets.contains(name) {
                            self.scopes.pop();
                            return Err(Bail);
                        }
                        let slot = self.fresh_slot(name);
                        self.scope_bind(name, slot, matches!(kind, DeclKind::Const));
                        if matches!(kind, DeclKind::Let | DeclKind::Const) {
                            self.tdz_slots.insert(slot);
                            self.emit(Op::Tdz(slot));
                        }
                        Bind::Slot(slot)
                    }
                    (pat, Some(kind @ (DeclKind::Let | DeclKind::Const))) => {
                        // A destructuring lexical head: fresh uncaptured slots for every leaf,
                        // declared (in TDZ) before `right` like the ident path. `var` patterns
                        // would have to write hoisted function-scope bindings — those stay in
                        // the oracle.
                        let mut leaf_names = std::collections::HashSet::new();
                        pat_idents(pat, &mut leaf_names);
                        if leaf_names.iter().any(|n| {
                            self.env_names.contains_key(n)
                                && !self.homed_lets.contains(n)
                                && !self.blk_names.contains(n)
                        }) || self
                            .declare_lexical_pattern(pat, matches!(kind, DeclKind::Const))
                            .is_err()
                        {
                            self.pending_blk.clear();
                            self.scopes.pop();
                            return Err(Bail);
                        }
                        match self.blk_flush() {
                            Ok(blk) => Bind::Pattern(*kind, blk),
                            Err(b) => {
                                self.scopes.pop();
                                return Err(b);
                            }
                        }
                    }
                    (Pattern::Array(_) | Pattern::Object(_) | Pattern::Member(_), Some(DeclKind::Var)) => {
                        self.scopes.pop();
                        return Err(Bail);
                    }
                    // A `var` head writes the hoisted function-scope binding, exactly like an
                    // assignment head.
                    (Pattern::Ident(name), None | Some(DeclKind::Var)) => match self.home(name) {
                        Some(Home::Blk(c, false)) => Bind::BlkStore(c, self.name_idx(name)),
                        Some(Home::Blk(_, true)) => {
                            self.scopes.pop();
                            return Err(Bail);
                        }
                        Some(Home::Slot(slot, is_const)) => {
                            if is_const || self.tdz_pending.contains(&slot) {
                                self.scopes.pop();
                                return Err(Bail);
                            }
                            Bind::Slot(slot)
                        }
                        Some(Home::Env(is_const)) => {
                            if is_const {
                                self.scopes.pop();
                                return Err(Bail);
                            }
                            Bind::Cap(self.name_idx(name))
                        }
                        None => Bind::Name(self.name_idx(name)),
                    },
                    // Destructuring *assignment* head (`for ([a, b] of xs)`) — oracle.
                    (_, None) => {
                        self.scopes.pop();
                        return Err(Bail);
                    }
                };
                let er = self.expr(right);
                if er.is_err() {
                    self.blk_envs.truncate(blk_depth);
                    self.scopes.pop();
                    return er;
                }
                let iter_s = self.fresh_slot("%iter%");
                let next_s = self.fresh_slot("%next%");
                let async_st = if is_await {
                    let st = self.fresh_slot("%ast%");
                    self.emit(Op::GetAsyncIter);
                    self.emit(Op::StoreLocal(st));
                    Some(st)
                } else {
                    self.emit(Op::GetIter);
                    None
                };
                self.emit(Op::StoreLocal(next_s));
                self.emit(Op::StoreLocal(iter_s));
                self.loops.push(LoopCtx {
                    labels,
                    entry_try_depth: self.try_depth,
                    foreach_iter: Some(iter_s),
                    foreach_async: async_st,
                    ..Default::default()
                });
                let loop_head = self.ops.len();
                match async_st {
                    Some(st) => {
                        self.emit(Op::AsyncIterNext(iter_s, next_s, st));
                        self.emit(Op::Await);
                        self.emit(Op::AsyncIterResult(st));
                    }
                    None => {
                        self.emit(Op::IterStepL(iter_s, next_s));
                    }
                }
                let jexit = self.emit(Op::JumpIfFalse(0));
                match bind {
                    Bind::Slot(slot) => {
                        self.init_slot(slot);
                    }
                    Bind::Cap(n) => {
                        self.emit(Op::StoreCap(n));
                    }
                    Bind::Name(n) => {
                        self.emit_store_name(n);
                    }
                    Bind::Blk(slot, parent, ref names) => {
                        self.blk_renew(slot, parent, names);
                        let n = self.name_idx(&names[0].0);
                        self.emit(Op::BlkInit(slot, n));
                    }
                    Bind::BlkStore(c, n) => {
                        self.emit(Op::BlkStore(c, n));
                    }
                    Bind::Pattern(..) => {} // bound below, inside the handler region
                }
                let push = self.emit(Op::PushHandler(0));
                self.try_depth += 1;
                self.loops.last_mut().expect("just pushed").body_try_depth = self.try_depth;
                let r = match bind {
                    Bind::Pattern(kind, ref blk) => {
                        if let Some((slot, parent, names)) = blk {
                            self.blk_renew(*slot, *parent, names);
                        }
                        self.fresh_blk = true;
                        let r = self.destructure_store(left, kind);
                        self.fresh_blk = false;
                        r.and_then(|()| self.stmt(body))
                    }
                    _ => self.stmt(body),
                };
                let ctx = self.loops.pop().expect("just pushed");
                self.blk_envs.truncate(blk_depth);
                self.scopes.pop();
                r?;
                self.emit(Op::PopHandler);
                self.try_depth -= 1;
                // continues re-enter at the step (the loop head re-pushes the body handler —
                // their cleanup already popped it).
                for j in ctx.continues {
                    match &mut self.ops[j] {
                        Op::Jump(t) => *t = loop_head as u32,
                        _ => unreachable!("continue is a jump"),
                    }
                }
                self.emit(Op::Jump(0));
                let jback = self.ops.len() - 1;
                match &mut self.ops[jback] {
                    Op::Jump(t) => *t = loop_head as u32,
                    _ => unreachable!(),
                }
                // The body's catch pad: swallow-close + rethrow (always abrupt).
                let abort_pc = self.ops.len() as u32;
                match &mut self.ops[push] {
                    Op::PushHandler(t) => *t = abort_pc,
                    _ => unreachable!(),
                }
                match async_st {
                    Some(st) => self.emit_async_abort(iter_s, st),
                    None => {
                        self.emit(Op::IterAbortL(iter_s));
                    }
                }
                // Exhaustion lands here (the step's bool was false): drop the undefined
                // placeholder the step pushed; no close on a completed iterator.
                self.patch(jexit);
                self.emit(Op::Pop);
                // Breaks jump here too — their cleanup (pop handler + close) ran at the site.
                let after = self.ops.len() as u32;
                for j in ctx.breaks {
                    match &mut self.ops[j] {
                        Op::Jump(t) => *t = after,
                        _ => unreachable!("break is a jump"),
                    }
                }
                Ok(())
            }
            other => {
                log_bail_node("stmt", other, 60);
                Err(Bail)
            }
        }
    }

    /// Emit the bookkeeping a `break`/`continue` targeting `self.loops[target]` must run before
    /// its jump: pop every `try`/for-of-body handler region opened since the target's entry (a
    /// stale handler would catch unrelated throws later in the frame), and IteratorClose each
    /// for-of iterator being abandoned, innermost first — the target's own iterator too for a
    /// `break`, but not for a `continue` (the loop keeps iterating). A close error propagates
    /// to the handlers still pushed: an enclosing abandoned loop's body pad closes that loop in
    /// throw mode (the spec's cascade), or a `try` between the loops catches it.
    fn emit_exit_cleanup(&mut self, target: usize, is_continue: bool) -> CResult {
        // For-of levels whose iterator is abandoned by this jump, innermost first.
        let closes: Vec<(u16, u32)> = self
            .loops
            .iter()
            .skip(if is_continue { target + 1 } else { target })
            .rev()
            .filter_map(|c| c.foreach_iter.map(|it| (it, c.body_try_depth)))
            .collect();
        let mut depth_now = self.try_depth;
        for (iter_s, body_depth) in closes {
            // Pop the regions inside the for-of body, then its own body handler, then close.
            for _ in body_depth..depth_now {
                self.emit(Op::PopHandler);
            }
            self.emit(Op::PopHandler);
            self.emit_iter_close(iter_s);
            depth_now = body_depth - 1;
        }
        // Remaining regions down to the target's entry (plain `try`s between the loops — and for
        // a continue to a for-of, its own body handler, which the loop head re-pushes).
        let floor = self.loops[target].entry_try_depth;
        for _ in floor..depth_now {
            self.emit(Op::PopHandler);
        }
        Ok(())
    }

    fn block_body(&mut self, body: &[Stmt]) -> CResult {
        let depth = self.blk_envs.len();
        let r = self.block_body_inner(body);
        self.blk_envs.truncate(depth);
        r
    }

    fn block_body_inner(&mut self, body: &[Stmt]) -> CResult {
        self.declare_block_lexicals(body)?;
        self.blk_flush()?;
        self.init_block_functions(body)?;
        for s in body {
            self.stmt(s)?;
        }
        Ok(())
    }

    fn for_loop(
        &mut self,
        init: Option<&ForInit>,
        test: Option<&Expr>,
        update: Option<&Expr>,
        body: &Stmt,
    ) -> CResult {
        // Claim any labels from an enclosing `Stmt::Labeled` before the head runs, so they land on
        // this loop's context (the head itself introduces no labelled break/continue targets).
        let labels = std::mem::take(&mut self.pending_labels);
        // A captured `let` head: its block env is copied per iteration (CreatePerIterationEnvironment).
        let mut per_iter: Option<u16> = None;
        match init {
            Some(ForInit::VarDecl { kind, decls }) => {
                if matches!(kind, DeclKind::Using | DeclKind::AwaitUsing) {
                    return Err(Bail);
                }
                let lexical = matches!(kind, DeclKind::Let | DeclKind::Const);
                if lexical {
                    let is_const = matches!(kind, DeclKind::Const);
                    for (pat, _) in decls {
                        match pat {
                            Pattern::Ident(name) if self.blk_names.contains(name) => {
                                self.pending_blk.push((name.clone(), is_const));
                            }
                            Pattern::Ident(name) => {
                                let slot = self.fresh_slot(name);
                                self.scope_bind(name, slot, is_const);
                                self.tdz_slots.insert(slot);
                                self.emit(Op::Tdz(slot));
                            }
                            // Leaves: TDZ slots, or the head's block env when captured.
                            _ => self.declare_lexical_pattern(pat, is_const)?,
                        }
                    }
                    if let Some((slot, _, _)) = self.blk_flush()? {
                        if !is_const {
                            per_iter = Some(slot);
                        }
                    }
                }
                for (pat, initv) in decls {
                    let Pattern::Ident(name) = pat else {
                        let Some(e) = initv else {
                            return Err(Bail);
                        };
                        self.expr(e)?;
                        self.destructure_store(pat, *kind)?;
                        continue;
                    };
                    match initv {
                        Some(e) => self.named_expr(e, name)?,
                        None if lexical => {
                            self.emit(Op::Undef);
                        }
                        None => continue,
                    }
                    match self.home(name) {
                        // Initialized from here on: later assignments (`label = label.next`
                        // in the update) need no TDZ guard.
                        Some(Home::Slot(slot, _)) if lexical => self.init_slot(slot),
                        Some(Home::Slot(slot, _)) => {
                            if self.tdz_pending.contains(&slot) {
                                return Err(Bail);
                            }
                            self.emit(Op::StoreLocal(slot));
                        }
                        Some(Home::Env(_)) => {
                            let n = self.name_idx(name);
                            self.emit(if lexical { Op::StoreCapInit(n) } else { Op::StoreCap(n) });
                        }
                        Some(Home::Blk(c, _)) => {
                            let n = self.name_idx(name);
                            self.emit(if lexical { Op::BlkInit(c, n) } else { Op::BlkStore(c, n) });
                        }
                        None => return Err(Bail),
                    }
                }
            }
            Some(ForInit::Expr(e)) => {
                self.expr_stmt(e)?;
            }
            None => {}
        }
        if let Some(s) = per_iter {
            self.emit(Op::BlkCopy(s));
        }
        let start = self.ops.len();
        let jf = match test {
            Some(t) => Some(self.jump_if_false(t)?),
            None => None,
        };
        self.loops.push(LoopCtx {
            labels,
            entry_try_depth: self.try_depth,
            ..LoopCtx::default()
        });
        let r = self.stmt(body);
        let ctx = self.loops.pop().unwrap();
        r?;
        let cont = self.ops.len();
        for c in ctx.continues {
            match &mut self.ops[c] {
                Op::Jump(t) => *t = cont as u32,
                _ => unreachable!(),
            }
        }
        if let Some(s) = per_iter {
            self.emit(Op::BlkCopy(s));
        }
        if let Some(u) = update {
            self.expr_stmt(u)?;
        }
        self.emit(Op::Jump(start as u32));
        if let Some(jf) = jf {
            self.patch(jf);
        }
        for b in ctx.breaks {
            self.patch(b);
        }
        Ok(())
    }

    /// Compile an expression whose value is discarded (an expression statement, or a `for`
    /// header's init / update). Assignments and `++`/`--` to a local drop their producing `Dup`
    /// (and the trailing `Pop`); everything else falls back to `expr` + `Pop`. Semantically
    /// identical to `self.expr(e)?; self.emit(Op::Pop)` — the only difference is the unobservable
    /// result value.
    fn expr_stmt(&mut self, e: &Expr) -> CResult {
        match e {
            Expr::Paren(inner) => return self.expr_stmt(inner),
            // A comma expression as a statement: every operand is evaluated for effect only.
            Expr::Seq(exprs) => {
                for ex in exprs {
                    self.expr_stmt(ex)?;
                }
                return Ok(());
            }
            Expr::Update { op, arg, .. } => {
                let kind = match *op {
                    "++" => UpdKind::IncDiscard,
                    "--" => UpdKind::DecDiscard,
                    _ => return Err(Bail),
                };
                return self.update_target(arg, kind);
            }
            Expr::Assign { op, target, value } => {
                return self.assign_discard(op, target, value);
            }
            _ => {}
        }
        self.expr(e)?;
        self.emit(Op::Pop);
        Ok(())
    }

    /// Compile a discarded assignment: the fast `Dup`-free lowering when the target is a plain
    /// local / free name / `obj.x` / `obj[k]`, otherwise the generic value-producing `assign`
    /// followed by `Pop` (identical to `self.expr(assign)?; Pop`).
    fn assign_discard(&mut self, op: &str, target: &Expr, value: &Expr) -> CResult {
        if self.try_assign_discard(op, target, value)? {
            return Ok(());
        }
        self.assign(op, target, value)?;
        self.emit(Op::Pop);
        Ok(())
    }

    /// Fast lowering for a discarded assignment (no `Dup`, no trailing `Pop`). Returns `Ok(true)`
    /// when it emitted the assignment, `Ok(false)` to defer to the generic `assign` + `Pop` path
    /// (which handles — or itself bails on — the forms not covered here). Any `Bail` from a
    /// compiled sub-expression propagates: the generic path would bail identically.
    fn try_assign_discard(&mut self, op: &str, target: &Expr, value: &Expr) -> Result<bool, Bail> {
        // Logical-assignment short-circuits; leave it to the generic path (which bails).
        if matches!(op, "&&=" | "||=" | "??=") {
            return Ok(false);
        }
        if op == "=" && matches!(target, Expr::Array(_) | Expr::Object(_)) {
            self.destructure_assign(target, value, false)?;
            return Ok(true);
        }
        match target {
            Expr::Ident(name) => match self.home(name) {
                Some(Home::Slot(slot, is_const)) => {
                    if is_const || (op == "=" && self.tdz_pending.contains(&slot)) {
                        return Ok(false);
                    }
                    if op == "=" {
                        self.named_expr(value, name)?;
                    } else {
                        self.emit(Op::LoadLocal(slot));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::StoreLocal(slot));
                    Ok(true)
                }
                Some(Home::Env(is_const)) => {
                    if is_const {
                        return Ok(false); // runtime TypeError — the oracle's business
                    }
                    let n = self.name_idx(name);
                    if op == "=" {
                        self.named_expr(value, name)?;
                    } else {
                        self.emit(Op::LoadCap(n));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::StoreCap(n));
                    Ok(true)
                }
                Some(Home::Blk(c, _)) => {
                    let n = self.name_idx(name);
                    if op == "=" {
                        self.named_expr(value, name)?;
                    } else {
                        self.emit(Op::BlkLoad(c, n));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::BlkStore(c, n));
                    Ok(true)
                }
                None => {
                    let i = self.name_idx(name);
                    if op == "=" {
                        // StoreName already consumes the value without re-pushing it.
                        self.named_expr(value, name)?;
                    } else {
                        // Resolve/read before the RHS, as compound assignment requires. Compiled
                        // closures under `with` are rejected at entry and direct eval in this
                        // body prevents compilation, so no nearer binding can appear between
                        // this read and StoreName; re-resolution names the same Reference.
                        let c = self.new_name_cache();
                        self.emit(Op::LoadName(i, c));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit_store_name(i);
                    Ok(true)
                }
            },
            // Receiver-direct statement stores: `this.x = v` (always safe — `this` can't be
            // reassigned) and `slotlocal.x = v` when the RHS provably can't reassign the local
            // (the receiver is read at set time, after the RHS — evaluation order must agree).
            Expr::Member {
                obj: mobj,
                prop,
                optional: false,
            } if op == "="
                && !prop.starts_with('#')
                && match &**mobj {
                    Expr::This => self.direct_this_allowed(),
                    Expr::Ident(name) => {
                        matches!(self.home(name), Some(Home::Slot(..))) && no_assign_to(value, name)
                    }
                    _ => false,
                } =>
            {
                self.expr(value)?;
                let i = self.name_idx(prop);
                let c = self.new_cache();
                match &**mobj {
                    Expr::This => {
                        self.uses_this = true;
                        self.emit(Op::SetPropThisDrop(i, c));
                    }
                    Expr::Ident(name) => {
                        let Some(Home::Slot(slot, _)) = self.home(name) else {
                            unreachable!()
                        };
                        self.emit(Op::SetPropLocalDrop(slot, i, c));
                    }
                    _ => unreachable!(),
                }
                Ok(true)
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.expr(obj)?;
                let i = self.name_idx(prop);
                if op == "+=" {
                    // Fused append: same evaluation order (read before RHS), and the op itself
                    // falls back to the generic Add + store when anything isn't plain strings.
                    self.emit(Op::Dup);
                    let cg = self.new_cache();
                    self.emit(Op::GetProp(i, cg));
                    self.expr(value)?;
                    let c = self.new_cache();
                    self.emit(Op::AppendProp(i, c));
                    return Ok(true);
                }
                if op == "=" {
                    self.expr(value)?;
                } else {
                    self.emit(Op::Dup);
                    let cg = self.new_cache();
                    self.emit(Op::GetProp(i, cg));
                    self.expr(value)?;
                    self.emit_compound(op)?;
                }
                let c = self.new_cache();
                self.emit(Op::SetPropDrop(i, c));
                Ok(true)
            }

            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                if let Some(slot) = self.fused_elem_slot(obj, &[index.as_ref(), value]) {
                    self.expr(index)?;
                    if op == "=" {
                        self.expr(value)?;
                    } else {
                        // Compound: coerce a side-effecting key once (Num keys pass raw), then
                        // read-modify-write against the slot base — one Dup, no receiver churn.
                        self.emit(Op::ToPropKeyLocal(slot));
                        self.emit(Op::Dup);
                        self.emit(Op::GetElemLocal(slot));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::SetElemLocalDrop(slot));
                    return Ok(true);
                }
                self.expr(obj)?;
                self.expr(index)?;
                if op == "=" {
                    self.expr(value)?;
                } else {
                    self.emit(Op::ToPropKey);
                    self.emit(Op::Dup2);
                    self.emit(Op::GetElem);
                    self.expr(value)?;
                    self.emit_compound(op)?;
                }
                self.emit(Op::SetElemDrop);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// Emit a closure over the current environment; `name` applies NamedEvaluation to an
    /// anonymous function expression (`var f = function(){}` → `f.name === "f"`).
    fn emit_closure(&mut self, f: &Rc<Function>, name: Option<&str>) {
        let fidx = self.funcs.len() as u32;
        self.funcs.push(f.clone());
        let name_idx = match name {
            Some(n) if f.name.is_none() && !f.is_method => self.name_idx(n),
            _ => u32::MAX,
        };
        self.env_prefix();
        self.emit(Op::MakeClosure(fidx, name_idx));
    }

    /// Compile a value expression in a naming position (declaration/assignment to `name`).
    fn named_expr(&mut self, e: &Expr, name: &str) -> CResult {
        if let Expr::Func(f) = e {
            self.emit_closure(f, Some(name));
            return Ok(());
        }
        if let Expr::Class(c) = e {
            return self.class_value(c, Some(name));
        }
        self.expr(e)
    }

    fn expr(&mut self, e: &Expr) -> CResult {
        // Out of native stack: bail to the tree-walker, which throws a RangeError.
        if crate::stack::exhausted() {
            return Err(Bail);
        }
        let saved_site = match e {
            Expr::Call { pos, .. } | Expr::New { pos, .. } => {
                Some(std::mem::replace(&mut self.site, pos.wrapping_add(1)))
            }
            _ => None,
        };
        let r = self.expr_inner(e);
        if let Some(s) = saved_site {
            self.site = s;
        }
        if r.is_err() {
            note_bail_reason(|| {
                let sub = match e {
                    Expr::Member { obj, prop, .. } if prop.starts_with('#') => {
                        if matches!(**obj, Expr::This) { " #priv(this)" } else { " #priv" }
                    }
                    Expr::Member { obj, .. } | Expr::Index { obj, .. }
                        if matches!(**obj, Expr::Super) => " super",
                    Expr::Call { callee, .. } => match &**callee {
                        Expr::Super => " super()",
                        Expr::Member { obj, .. } if matches!(**obj, Expr::Super) => " super.m()",
                        Expr::Member { prop, .. } if prop.starts_with('#') => " #m()",
                        _ => "",
                    },
                    _ => "",
                };
                format!("expr {}{sub}", node_kind(e))
            });
        }
        r
    }

    fn expr_inner(&mut self, e: &Expr) -> CResult {
        match e {
            Expr::Func(f) => {
                self.emit_closure(f, None);
                Ok(())
            }
            Expr::Num(n) => {
                let i = self.const_idx(Value::Num(*n));
                self.emit(Op::Const(i));
                Ok(())
            }
            Expr::Str(s) => {
                let i = self.const_idx(Value::Str(s.clone().into()));
                self.emit(Op::Const(i));
                Ok(())
            }
            Expr::Bool(b) => {
                let i = self.const_idx(Value::Bool(*b));
                self.emit(Op::Const(i));
                Ok(())
            }
            Expr::Null => {
                let i = self.const_idx(Value::Null);
                self.emit(Op::Const(i));
                Ok(())
            }
            Expr::Undefined => {
                self.emit(Op::Undef);
                Ok(())
            }
            Expr::BigInt(n) => {
                let i = self.const_idx(Value::BigInt(n.clone()));
                self.emit(Op::Const(i));
                Ok(())
            }
            Expr::Ident(name) => {
                match self.home(name) {
                    Some(Home::Slot(slot, _)) => {
                        self.emit(Op::LoadLocal(slot));
                    }
                    Some(Home::Env(_)) => {
                        let i = self.name_idx(name);
                        self.emit(Op::LoadCap(i));
                    }
                    Some(Home::Blk(c, _)) => {
                        let i = self.name_idx(name);
                        self.emit(Op::BlkLoad(c, i));
                    }
                    None => {
                        let i = self.name_idx(name);
                        let c = self.new_name_cache();
                        self.emit(Op::LoadName(i, c));
                    }
                };
                Ok(())
            }
            Expr::This => {
                self.emit_this();
                Ok(())
            }
            Expr::NewTarget => {
                self.emit(Op::LoadNewTarget);
                Ok(())
            }
            Expr::Paren(inner) => self.expr(inner),
            Expr::Seq(exprs) => {
                for (k, ex) in exprs.iter().enumerate() {
                    self.expr(ex)?;
                    if k + 1 < exprs.len() {
                        self.emit(Op::Pop);
                    }
                }
                Ok(())
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                // Receiver-direct forms: `this.x` and `slotlocal.x` skip the operand-stack
                // round trip (push + refcount bump + drop) entirely.
                match &**obj {
                    Expr::This if self.direct_this_allowed() => {
                        self.uses_this = true;
                        let i = self.name_idx(prop);
                        let c = self.new_cache();
                        self.emit(Op::GetPropThis(i, c));
                        return Ok(());
                    }
                    Expr::Ident(name) => {
                        if let Some(Home::Slot(slot, _)) = self.home(name) {
                            if let Some(v) = self.virt.as_ref().filter(|v| v.slot == slot) {
                                if prop == "length" {
                                    let tag = v.tag();
                                    self.emit(Op::ArgsLen(slot, tag));
                                    return Ok(());
                                }
                            }
                            let i = self.name_idx(prop);
                            let c = self.new_cache();
                            self.emit(Op::GetPropLocal(slot, i, c));
                            return Ok(());
                        }
                    }
                    _ => {}
                }
                self.expr(obj)?;
                let i = self.name_idx(prop);
                let c = self.new_cache();
                self.emit(Op::GetProp(i, c));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                if let Expr::Ident(name) = &**obj {
                    if let Some(Home::Slot(slot, _)) = self.home(name) {
                        if let Some(tag) = self.virt.as_ref().filter(|v| v.slot == slot).map(VirtC::tag) {
                            self.expr(index)?;
                            self.emit(Op::ArgsGet(slot, tag));
                            return Ok(());
                        }
                    }
                }
                if let Some(slot) = self.fused_elem_slot(obj, &[index.as_ref()]) {
                    self.expr(index)?;
                    self.emit(Op::GetElemLocal(slot));
                } else {
                    self.expr(obj)?;
                    self.expr(index)?;
                    self.emit(Op::GetElem);
                }
                Ok(())
            }
            Expr::Binary { op: "+", left, right } if template_chain(left, right).is_some() => {
                let parts = template_chain(left, right).expect("checked");
                let mut n = 0u16;
                for p in parts {
                    if matches!(p, Expr::Str(s) if s.is_empty()) {
                        continue;
                    }
                    self.expr(p)?;
                    n += 1;
                }
                match n {
                    0 => {
                        let i = self.const_idx(Value::Str(crate::lstr::LStr::from("")));
                        self.emit(Op::Const(i));
                    }
                    1 => {}
                    _ => {
                        self.emit(Op::Concat(n));
                    }
                }
                Ok(())
            }
            Expr::Binary { op, left, right } => {
                if let Some((arg, kind, negated)) = typeof_test(op, left, right) {
                    // `typeof freeName` must not throw for an unresolvable name; that path keeps
                    // TypeofName and the ordinary string comparison.
                    let free = matches!(arg, Expr::Ident(n) if self.home(n).is_none());
                    if !free {
                        self.expr(arg)?;
                        self.emit(Op::TypeofIs(kind, negated));
                        return Ok(());
                    }
                }
                self.expr(left)?;
                self.expr(right)?;
                let bop = match *op {
                    "+" => Op::Add,
                    "-" => Op::Sub,
                    "*" => Op::Mul,
                    "/" => Op::Div,
                    "%" => Op::Mod,
                    "&" => Op::BitAnd,
                    "|" => Op::BitOr,
                    "^" => Op::BitXor,
                    "<<" => Op::Shl,
                    ">>" => Op::Shr,
                    ">>>" => Op::UShr,
                    "<" => Op::Lt,
                    ">" => Op::Gt,
                    "<=" => Op::Le,
                    ">=" => Op::Ge,
                    "==" => Op::EqEq,
                    "!=" => Op::NotEq,
                    "===" => Op::StrictEq,
                    "!==" => Op::StrictNotEq,
                    "instanceof" => Op::InstanceOf(self.new_single_cache()),
                    other => {
                        let i = self.name_idx(other);
                        Op::GenBin(i)
                    }
                };
                self.emit(bop);
                Ok(())
            }
            Expr::Logical { op, left, right } => {
                self.expr(left)?;
                let j = match *op {
                    "&&" => self.emit(Op::JumpIfFalsePeek(0)),
                    "||" => self.emit(Op::JumpIfTruePeek(0)),
                    "??" => self.emit(Op::JumpIfNotNullishPeek(0)),
                    _ => return Err(Bail),
                };
                self.emit(Op::Pop);
                self.expr(right)?;
                self.patch(j);
                Ok(())
            }
            Expr::Cond { test, cons, alt } => {
                let jf = self.jump_if_false(test)?;
                self.expr(cons)?;
                let jend = self.emit(Op::Jump(0));
                self.patch(jf);
                self.expr(alt)?;
                self.patch(jend);
                Ok(())
            }
            Expr::Unary { op, arg } => {
                match *op {
                    "-" => {
                        self.expr(arg)?;
                        self.emit(Op::Neg);
                    }
                    "+" => {
                        self.expr(arg)?;
                        self.emit(Op::Plus);
                    }
                    "!" => {
                        self.expr(arg)?;
                        self.emit(Op::Not);
                    }
                    "~" => {
                        self.expr(arg)?;
                        self.emit(Op::BitNot);
                    }
                    "void" => {
                        self.expr(arg)?;
                        self.emit(Op::Void);
                    }
                    "typeof" => {
                        if let Expr::Ident(n) = &**arg {
                            if self.home(n).is_none() {
                                let name = self.name_idx(n);
                                self.emit(Op::TypeofName(name));
                                return Ok(());
                            }
                        }
                        self.expr(arg)?;
                        self.emit(Op::Typeof);
                    }
                    "delete" => return self.delete_expr(arg),
                    _ => return Err(Bail),
                }
                Ok(())
            }
            Expr::Await(arg) => {
                self.expr(arg)?;
                self.emit(Op::Await);
                Ok(())
            }
            Expr::Yield { delegate, arg } => self.yield_expr(*delegate, arg.as_deref()),
            Expr::Update { op, prefix, arg } => {
                let kind = match (*op, *prefix) {
                    ("++", true) => UpdKind::PreInc,
                    ("--", true) => UpdKind::PreDec,
                    ("++", false) => UpdKind::PostInc,
                    ("--", false) => UpdKind::PostDec,
                    _ => return Err(Bail),
                };
                self.update_target(arg, kind)
            }
            Expr::Assign { op, target, value } => self.assign(op, target, value),
            Expr::ToStr(inner) => {
                self.expr(inner)?;
                self.emit(Op::ToStr);
                Ok(())
            }
            Expr::OptionalChain(inner) => {
                let mut shorts = Vec::new();
                self.opt_chain(inner, &mut shorts)?;
                if shorts.is_empty() {
                    return Ok(()); // no optional link actually taken a short path
                }
                let done = self.emit(Op::Jump(0));
                for j in shorts {
                    self.patch(j);
                }
                self.emit(Op::Undef);
                self.patch(done);
                Ok(())
            }
            Expr::Call {
                callee,
                args,
                optional: false,
                ..
            } => {
                // Direct eval can see the activation — bail the function.
                if matches!(&**callee, Expr::Ident(n) if n == "eval") {
                    return Err(Bail);
                }
                if matches!(&**callee, Expr::Ident(n) if n == class_fields::DEFINE_FIELD) {
                    return self.define_field_intrinsic(args);
                }
                match &**callee {
                    Expr::Member {
                        obj,
                        prop,
                        optional: false,
                    } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                        if self.inline_array_callback(obj, prop, args) {
                            return Ok(());
                        }
                        self.expr(obj)?;
                        let i = self.name_idx(prop);
                        let c = self.new_cache();
                        self.emit(Op::GetMethod(i, c));
                        if self.apply_args(prop, args)? {
                            // (Emitted the call.)
                        } else if self.call_args(args)? {
                            self.emit(Op::CallSpreadThis(args.len() as u16));
                        } else {
                            self.emit(Op::CallWithThis(args.len() as u16));
                        }
                    }
                    Expr::Index {
                        obj,
                        index,
                        optional: false,
                    } if !matches!(**obj, Expr::Super) => {
                        self.expr(obj)?;
                        self.expr(index)?;
                        self.emit(Op::GetMethodElem);
                        if self.call_args(args)? {
                            self.emit(Op::CallSpreadThis(args.len() as u16));
                        } else {
                            self.emit(Op::CallWithThis(args.len() as u16));
                        }
                    }
                    Expr::Super => self.super_call(args)?,
                    Expr::Member {
                        obj,
                        prop,
                        optional: false,
                    } if prop.starts_with('#') || matches!(**obj, Expr::Super) => {
                        let n = self.name_idx(prop);
                        if matches!(**obj, Expr::Super) {
                            let lexical = !self.direct_this_allowed();
                            if !lexical {
                                self.uses_this = true;
                            }
                            self.emit(Op::SuperBase);
                            self.emit(Op::SuperMethod(n, lexical));
                        } else {
                            self.expr(obj)?;
                            self.emit(Op::GetPrivateMethod(n));
                        }
                        if self.call_args(args)? {
                            self.emit(Op::CallSpreadThis(args.len() as u16));
                        } else {
                            self.emit(Op::CallWithThis(args.len() as u16));
                        }
                    }
                    Expr::Index {
                        obj,
                        index,
                        optional: false,
                    } if matches!(**obj, Expr::Super) => {
                        let lexical = !self.direct_this_allowed();
                        if !lexical {
                            self.uses_this = true;
                        }
                        self.emit(Op::SuperBase);
                        self.expr(index)?;
                        self.emit(Op::SuperMethodElem(lexical));
                        if self.call_args(args)? {
                            self.emit(Op::CallSpreadThis(args.len() as u16));
                        } else {
                            self.emit(Op::CallWithThis(args.len() as u16));
                        }
                    }
                    Expr::Ident(name) if self.home(name).is_none() => {
                        // Free-name callee: resolved before the arguments (spec order), and a
                        // `with (obj) f()` hit supplies obj as `this`.
                        let i = self.name_idx(name);
                        let c = self.new_name_cache();
                        self.emit(Op::LoadNameForCall(i, c));
                        if self.call_args(args)? {
                            self.emit(Op::CallSpreadThis(args.len() as u16));
                        } else {
                            self.emit(Op::CallWithThis(args.len() as u16));
                        }
                    }
                    other => {
                        self.expr(other)?;
                        if self.call_args(args)? {
                            self.emit(Op::CallSpread(args.len() as u16));
                        } else {
                            self.emit(Op::Call(args.len() as u16));
                        }
                    }
                }
                Ok(())
            }
            Expr::New { callee, args, .. } => {
                self.expr(callee)?;
                if self.call_args(args)? {
                    self.emit(Op::NewSpread(args.len() as u16));
                } else {
                    self.emit(Op::New(args.len() as u16));
                }
                Ok(())
            }
            Expr::Regex { body, flags } => {
                let body = self.name_idx(body);
                let flags = self.name_idx(flags);
                self.emit(Op::MakeRegExp(body, flags));
                Ok(())
            }
            Expr::Array(elems) if elems.iter().all(|e| matches!(e, ArrayElem::Item(_))) => {
                for el in elems {
                    if let ArrayElem::Item(e) = el {
                        self.expr(e)?;
                    }
                }
                self.emit(Op::MakeArray(elems.len() as u16));
                Ok(())
            }
            // Spreads/holes: elements append in order, each spread exhausted before the next
            // element evaluates (ArrayAccumulation, as the oracle's `eval_array`).
            Expr::Array(elems) => {
                self.emit(Op::NewArrayLit);
                for el in elems {
                    match el {
                        ArrayElem::Item(e) => {
                            self.expr(e)?;
                            self.emit(Op::ArrayAppend);
                        }
                        ArrayElem::Spread(e) => {
                            self.expr(e)?;
                            self.emit(Op::ArrayAppendSpread);
                        }
                        ArrayElem::Hole => {
                            self.emit(Op::ArrayHole);
                        }
                    }
                }
                Ok(())
            }
            Expr::Object(props) => self.object_literal(props),
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if prop.starts_with('#') && !matches!(**obj, Expr::Super) => {
                self.expr(obj)?;
                let n = self.name_idx(prop);
                self.emit(Op::GetPrivate(n));
                Ok(())
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if matches!(**obj, Expr::Super) => {
                self.emit_this();
                let n = self.name_idx(prop);
                self.emit(Op::SuperGet(n));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if matches!(**obj, Expr::Super) => {
                self.emit_this();
                self.expr(index)?;
                self.emit(Op::SuperGetElem);
                Ok(())
            }
            Expr::PrivateIn { name, obj } => {
                self.expr(obj)?;
                let n = self.name_idx(name);
                self.emit(Op::PrivateIn(n));
                Ok(())
            }
            Expr::Class(c) => self.class_value(c, None),
            Expr::ImportCall {
                spec,
                phase,
                options,
            } => {
                self.expr(spec)?;
                if let Some(o) = options {
                    self.expr(o)?;
                }
                let phase = match phase {
                    ImportPhase::Evaluation => 0,
                    ImportPhase::Source => 1,
                    ImportPhase::Defer => 2,
                };
                self.emit(Op::ImportCall(phase, options.is_some()));
                Ok(())
            }
            other => {
                log_bail_node("expr", other, 60);
                Err(Bail)
            }
        }
    }

    /// `++`/`--` on a local slot, `obj.name`, or `obj[k]`; `kind` carries pre/post/discard.
    fn update_target(&mut self, arg: &Expr, kind: UpdKind) -> CResult {
        match arg {
            Expr::Paren(inner) => self.update_target(inner, kind),
            Expr::Ident(name) => match self.home(name) {
                Some(Home::Slot(slot, false)) => {
                    self.emit(Op::UpdateLocal(slot, kind));
                    Ok(())
                }
                Some(Home::Env(false)) => {
                    let n = self.name_idx(name);
                    self.emit(Op::UpdateCap(n, kind));
                    Ok(())
                }
                Some(Home::Slot(_, true)) | Some(Home::Env(true)) => Err(Bail),
                Some(Home::Blk(c, _)) => {
                    let n = self.name_idx(name);
                    self.emit(Op::BlkUpdate(c, n, kind));
                    Ok(())
                }
                None => {
                    let n = self.name_idx(name);
                    let c = self.new_name_cache();
                    self.emit(Op::UpdateNameCached(n, c, kind));
                    Ok(())
                }
            },
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.expr(obj)?;
                let i = self.name_idx(prop);
                let c = self.new_cache();
                self.emit(Op::UpdateProp(i, c, kind));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                self.expr(obj)?;
                self.expr(index)?;
                self.emit(Op::UpdateElem(kind));
                Ok(())
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if prop.starts_with('#') && !matches!(**obj, Expr::Super) => {
                self.expr(obj)?;
                let n = self.name_idx(prop);
                self.emit(Op::UpdatePrivate(n, kind));
                Ok(())
            }
            other => {
                log_bail_node("expr", other, 60);
                Err(Bail)
            }
        }
    }

    fn assign(&mut self, op: &str, target: &Expr, value: &Expr) -> CResult {
        if matches!(op, "&&=" | "||=" | "??=") {
            return self.logical_assign(op, target, value);
        }
        if op == "=" && matches!(target, Expr::Array(_) | Expr::Object(_)) {
            return self.destructure_assign(target, value, true);
        }
        match target {
            Expr::Ident(name) => match self.home(name) {
                Some(Home::Slot(slot, is_const)) => {
                    if is_const || (op == "=" && self.tdz_pending.contains(&slot)) {
                        return Err(Bail);
                    }
                    if op == "=" {
                        self.named_expr(value, name)?;
                    } else {
                        self.emit(Op::LoadLocal(slot));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::Dup);
                    self.emit(Op::StoreLocal(slot));
                    Ok(())
                }
                Some(Home::Env(is_const)) => {
                    if is_const {
                        return Err(Bail);
                    }
                    let n = self.name_idx(name);
                    if op == "=" {
                        self.named_expr(value, name)?;
                    } else {
                        self.emit(Op::LoadCap(n));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::Dup);
                    self.emit(Op::StoreCap(n));
                    Ok(())
                }
                Some(Home::Blk(c, _)) => {
                    let n = self.name_idx(name);
                    if op == "=" {
                        self.named_expr(value, name)?;
                    } else {
                        self.emit(Op::BlkLoad(c, n));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::Dup);
                    self.emit(Op::BlkStore(c, n));
                    Ok(())
                }
                None => {
                    let i = self.name_idx(name);
                    if op == "=" {
                        self.named_expr(value, name)?;
                    } else {
                        // See the discarded-assignment path above for the stable-Reference proof.
                        let c = self.new_name_cache();
                        self.emit(Op::LoadName(i, c));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::Dup);
                    self.emit_store_name(i);
                    Ok(())
                }
            },
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if !matches!(**obj, Expr::Super) && !prop.starts_with('#') => {
                self.expr(obj)?;
                let i = self.name_idx(prop);
                if op == "=" {
                    self.expr(value)?;
                } else {
                    // Compound: base evaluated once (Dup), get before the RHS — Reference order.
                    self.emit(Op::Dup);
                    let cg = self.new_cache();
                    self.emit(Op::GetProp(i, cg));
                    self.expr(value)?;
                    self.emit_compound(op)?;
                }
                let c = self.new_cache();
                self.emit(Op::SetProp(i, c));
                Ok(())
            }
            Expr::Index {
                obj,
                index,
                optional: false,
            } if !matches!(**obj, Expr::Super) => {
                if let Some(slot) = self.fused_elem_slot(obj, &[index.as_ref(), value]) {
                    self.expr(index)?;
                    if op == "=" {
                        self.expr(value)?;
                    } else {
                        self.emit(Op::ToPropKeyLocal(slot));
                        self.emit(Op::Dup);
                        self.emit(Op::GetElemLocal(slot));
                        self.expr(value)?;
                        self.emit_compound(op)?;
                    }
                    self.emit(Op::SetElemLocal(slot));
                    return Ok(());
                }
                self.expr(obj)?;
                self.expr(index)?;
                if op == "=" {
                    self.expr(value)?;
                } else {
                    // Compound: coerce a side-effecting key once, then read-modify-write.
                    self.emit(Op::ToPropKey);
                    self.emit(Op::Dup2);
                    self.emit(Op::GetElem);
                    self.expr(value)?;
                    self.emit_compound(op)?;
                }
                self.emit(Op::SetElem);
                Ok(())
            }
            Expr::Member {
                obj,
                prop,
                optional: false,
            } if prop.starts_with('#') && !matches!(**obj, Expr::Super) => {
                self.expr(obj)?;
                let n = self.name_idx(prop);
                if op == "=" {
                    self.expr(value)?;
                } else {
                    self.emit(Op::Dup);
                    self.emit(Op::GetPrivate(n));
                    self.expr(value)?;
                    self.emit_compound(op)?;
                }
                self.emit(Op::SetPrivate(n));
                Ok(())
            }
            _ => Err(Bail),
        }
    }

    fn emit_compound(&mut self, op: &str) -> CResult {
        let bop = match op {
            "+=" => Op::Add,
            "-=" => Op::Sub,
            "*=" => Op::Mul,
            "/=" => Op::Div,
            "%=" => Op::Mod,
            "&=" => Op::BitAnd,
            "|=" => Op::BitOr,
            "^=" => Op::BitXor,
            "<<=" => Op::Shl,
            ">>=" => Op::Shr,
            ">>>=" => Op::UShr,
            "**=" => {
                let i = self.name_idx("**");
                Op::GenBin(i)
            }
            _ => return Err(Bail),
        };
        self.emit(bop);
        Ok(())
    }
}

