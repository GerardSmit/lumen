//! Per-site identifier resolution cache for the tree-walker.
//!
//! Resolving a name walks the scope chain with a string/hash lookup per scope. Within one scope
//! instance (a loop body, a call's activation) the same identifier resolves to the same binding
//! every time, as long as the chain's structure is unchanged — which the scope epoch
//! ([`crate::value::scope_epoch`]) certifies: a resolution marks every binding map it walks
//! (`VarMap::observe`), and any later structural mutation of a marked map bumps the epoch, so
//! while it holds no walked scope can have gained (shadowed) or lost the name and no binding can
//! have moved. A site keyed by its AST node address therefore reuses the binding address it found
//! last time when the epoch, the environment and the environment's serial all match. Scope
//! creation does not bump the epoch, so a loop that calls functions keeps its entries.
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
    /// `env`'s serial: a later scope reusing the address is a different environment.
    serial: u64,
    epoch: u64,
    /// Null for a name no scope on the chain declares.
    scope: *const ScopeCell,
    binding: *mut Binding,
    /// The binding's key, kept only for a name longer than `name` holds.
    key: *const Rc<str>,
    /// The name's bytes when it fits (`name_len > 0`), so a reused node address cannot alias
    /// the entry. Longer names compare against `key` instead; unscoped ones are not cached.
    name: [u8; NAME_BYTES],
    name_len: u8,
}

const NAME_BYTES: usize = 16;

const EMPTY_SITE: NameSite = NameSite {
    node: 0,
    env: std::ptr::null(),
    serial: 0,
    epoch: 0,
    scope: std::ptr::null(),
    key: std::ptr::null(),
    binding: std::ptr::null_mut(),
    name: [0; NAME_BYTES],
    name_len: 0,
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
        // SAFETY: `env` is live; the serial is written once, at creation.
        let serial = unsafe { (*(*env_ptr).as_ptr()).serial };
        if site.node == node && site.env == env_ptr && site.epoch == epoch && site.serial == serial
        {
            let same = if site.name_len > 0 {
                same_name(&site.name[..site.name_len as usize], name)
            } else {
                // SAFETY: an unchanged epoch means the map holding `key` has not been mutated
                // or freed since the fill (its scope is an ancestor of the live `env`).
                same_name(unsafe { &**site.key }, name)
            };
            if !same {
                return NameRes::Unknown;
            }
            if site.scope.is_null() {
                return NameRes::Unscoped;
            }
            return NameRes::Binding(site.scope, site.binding);
        }
        self.fill_name_site(node, name, env, serial, epoch, idx)
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

    /// Resolve by walking the chain, marking each map walked. The entry is only filled the
    /// second time the site runs in the same environment: a site that runs once per activation
    /// (the common case in call-heavy code) would never hit, so it records just its environment.
    #[inline(never)]
    fn fill_name_site(
        &mut self,
        node: usize,
        name: &str,
        env: &Env,
        serial: u64,
        epoch: u64,
        idx: usize,
    ) -> NameRes {
        let short = name.len() <= NAME_BYTES;
        let mut key = std::ptr::null();
        let mut res = NameRes::Unscoped;
        // Every scope on the walk is kept alive by `env` through the `parent` links.
        let mut cur: *const ScopeCell = Rc::as_ptr(env);
        loop {
            let Ok(mut b) = (unsafe { (*cur).try_borrow_mut() }) else {
                return NameRes::Unknown;
            };
            if b.with_obj.is_some() {
                return NameRes::Unknown;
            }
            // From now on a structural change to this map bumps the epoch, which is what keeps
            // the result valid for a caller that runs code before using it.
            b.vars.observe();
            let found = if short {
                b.vars.binding_ptr(name).map(|binding| (std::ptr::null(), binding))
            } else {
                b.vars.entry_ptrs(name)
            };
            if let Some((k, binding)) = found {
                key = k;
                res = NameRes::Binding(cur, binding);
                break;
            }
            match b.parent.as_ref() {
                Some(p) => cur = Rc::as_ptr(p),
                None => break,
            }
        }
        let env_ptr = Rc::as_ptr(env);
        let site = &mut self.name_sites.0[idx];
        let revisit = site.node == node && site.env == env_ptr && site.serial == serial;
        site.node = node;
        site.env = env_ptr;
        site.serial = serial;
        site.epoch = PENDING;
        if !revisit {
            return res;
        }
        let (scope, binding) = match res {
            NameRes::Binding(scope, binding) => (scope, binding),
            _ if short => (std::ptr::null(), std::ptr::null_mut()),
            _ => return res,
        };
        site.scope = scope;
        site.binding = binding;
        site.key = key;
        site.name_len = 0;
        if short {
            site.name[..name.len()].copy_from_slice(name.as_bytes());
            site.name_len = name.len() as u8;
        }
        site.epoch = epoch;
        res
    }
}

/// An epoch value the scope epoch never reaches: the entry holds only its environment.
const PENDING: u64 = u64::MAX;

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
