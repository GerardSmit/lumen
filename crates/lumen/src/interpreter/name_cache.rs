//! Per-site identifier resolution cache for the tree-walker.
//!
//! Resolving a name walks the scope chain with a string/hash lookup per scope. Within one scope
//! instance (a loop body, a call's activation) the same identifier resolves to the same binding
//! every time, as long as the chain's structure is unchanged — which the scope epoch
//! ([`crate::value::scope_epoch`]) certifies: every scope creation and every structural binding
//! map mutation bumps it, so while it holds no scope can have gained (shadowed) or lost the name
//! and no binding can have moved. A site keyed by its AST node address therefore reuses the
//! binding address it found last time when both the epoch and the environment match.
//!
//! Chains with a `with` object before the binding are never cached (the object's properties can
//! change without touching any binding map). A name no scope declares is cached as "unscoped", so
//! its reads go straight to the global object. The cached key is compared against the site's name on every hit,
//! so a node address reused by a different identifier (a freed eval body, a temporary node) cannot
//! alias a stale entry.
use super::{Binding, Env, Interp, Scope};
use std::cell::RefCell;
use std::rc::Rc;

pub(crate) type ScopeCell = RefCell<Scope>;

#[derive(Clone, Copy)]
struct NameSite {
    node: usize,
    env: *const ScopeCell,
    epoch: u64,
    /// Null for a name no scope on the chain declares (see `unscoped`).
    scope: *const ScopeCell,
    key: *const Rc<str>,
    binding: *mut Binding,
    /// For an unscoped entry: the name's bytes (names up to 16 bytes are cached this way), so a
    /// reused node address cannot alias it.
    unscoped: [u8; 16],
    unscoped_len: u8,
}

const EMPTY_SITE: NameSite = NameSite {
    node: 0,
    env: std::ptr::null(),
    epoch: 0,
    scope: std::ptr::null(),
    key: std::ptr::null(),
    binding: std::ptr::null_mut(),
    unscoped: [0; 16],
    unscoped_len: 0,
};

/// How a site's name resolves against the scope chain.
pub(crate) enum NameRes {
    /// A binding in `scope`, at a stable address while the scope epoch holds.
    Binding(*const ScopeCell, *mut Binding),
    /// No scope declares the name and no `with` object is on the chain: the global object's
    /// properties decide.
    Unscoped,
    /// Not cacheable (a `with` scope, a borrowed scope): take the full lookup.
    Unknown,
}

/// Direct-mapped: a collision only costs a re-resolution.
const NAME_SITES: usize = 1024;

pub(crate) struct NameSites(Box<[NameSite]>);

impl Default for NameSites {
    fn default() -> Self {
        NameSites(vec![EMPTY_SITE; NAME_SITES].into_boxed_slice())
    }
}

impl Interp {
    /// How `name` (the identifier at AST address `node`) resolves from `env`. A binding's
    /// pointers stay valid while the scope epoch is unchanged; a caller that runs arbitrary code
    /// in between must re-check it.
    #[inline]
    pub(crate) fn resolve_name(&mut self, node: usize, name: &str, env: &Env) -> NameRes {
        let epoch = crate::value::scope_epoch();
        let env_ptr = Rc::as_ptr(env);
        let idx = (node.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) & (NAME_SITES - 1);
        let site = &self.name_sites.0[idx];
        if site.node == node && site.env == env_ptr && site.epoch == epoch {
            if !site.scope.is_null() {
                // SAFETY: an unchanged epoch means the map holding `key` has not been mutated or
                // freed since the fill (its scope is an ancestor of the live `env`).
                if same_name(unsafe { &**site.key }, name) {
                    return NameRes::Binding(site.scope, site.binding);
                }
            } else if same_name(&site.unscoped[..site.unscoped_len as usize], name) {
                return NameRes::Unscoped;
            }
            return NameRes::Unknown;
        }
        self.fill_name_site(node, name, env, epoch, idx)
    }

    /// [`Interp::resolve_name`] narrowed to scope bindings.
    #[inline]
    pub(crate) fn cached_binding(
        &mut self,
        node: usize,
        name: &str,
        env: &Env,
    ) -> Option<(*const ScopeCell, *mut Binding)> {
        match self.resolve_name(node, name, env) {
            NameRes::Binding(scope, binding) => Some((scope, binding)),
            _ => None,
        }
    }

    #[inline(never)]
    fn fill_name_site(
        &mut self,
        node: usize,
        name: &str,
        env: &Env,
        epoch: u64,
        idx: usize,
    ) -> NameRes {
        // Every scope on the walk is kept alive by `env` through the `parent` links.
        let mut cur: *const ScopeCell = Rc::as_ptr(env);
        loop {
            let Ok(mut b) = (unsafe { (*cur).try_borrow_mut() }) else {
                return NameRes::Unknown;
            };
            if b.with_obj.is_some() {
                return NameRes::Unknown;
            }
            if let Some((key, binding)) = b.vars.entry_ptrs(name) {
                self.name_sites.0[idx] = NameSite {
                    node,
                    env: Rc::as_ptr(env),
                    epoch,
                    scope: cur,
                    key,
                    binding,
                    ..EMPTY_SITE
                };
                return NameRes::Binding(cur, binding);
            }
            match b.parent.as_ref() {
                Some(p) => cur = Rc::as_ptr(p),
                None => break,
            }
        }
        if name.len() <= 16 {
            let mut unscoped = [0; 16];
            unscoped[..name.len()].copy_from_slice(name.as_bytes());
            self.name_sites.0[idx] = NameSite {
                node,
                env: Rc::as_ptr(env),
                epoch,
                unscoped,
                unscoped_len: name.len() as u8,
                ..EMPTY_SITE
            };
        }
        NameRes::Unscoped
    }
}

/// Identifier equality without a `memcmp` call: names are short.
#[inline(always)]
fn same_name(a: impl AsRef<[u8]>, b: &str) -> bool {
    let (a, b) = (a.as_ref(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x == y)
}

/// An owned handle to a scope found by [`Interp::cached_binding`] (an ancestor of a live env).
pub(crate) fn scope_handle(scope: *const ScopeCell) -> Env {
    // SAFETY: `scope` is alive (see above); the extra strong count is owned by the result.
    unsafe {
        Rc::increment_strong_count(scope);
        Rc::from_raw(scope)
    }
}
