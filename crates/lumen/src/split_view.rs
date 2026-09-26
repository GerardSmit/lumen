//! Lazy `String.prototype.split` results.
//!
//! `text.split('\n')` over a multi-megabyte list used to hold one heap string per line plus
//! its element slot (~95 bytes a line). Most consumers only read `.length` and `lines[i]`, so a
//! large split on a plain string separator instead returns a *split view*: an object that is an
//! Array from the outside (Array.prototype, own `length`, `Array.isArray`, every reflection
//! answer the eager array would give) but stores just the source string and one `u32` per piece.
//! Each element read slices a fresh string out of the source.
//!
//! Representation: `exotic == Exotic::SplitView`, `call == Callable::SplitView(state)`,
//! `ic_plain == false`, and `props` is an array map holding exactly the own `length` data
//! property (writable, non-enumerable, non-configurable) — so plain `length` reads, and
//! `Interp::array_length`, work unchanged. Every `Exotic::Array` fast path misses the view.
//!
//! Lazy (view-aware) operations: [[Get]] of an index / `length` (interpreter, bytecode, JIT slow
//! paths), [[HasProperty]], [[GetOwnProperty]] of an index, IsArray, the Array iterator, and the
//! read-only `Array.prototype` walks (which go through [[Get]]). Everything else — any write,
//! define, delete, `length` store, integrity change, prototype change, key enumeration or
//! direct `props` consumer — first calls [`materialize`], which rebuilds the ordinary packed
//! element storage in place (exactly the elements the eager split would have produced), turns
//! the object into a plain `Exotic::Array` and drops the view state. Materializing is never
//! observable: the object's identity, prototype, extensibility and property values are the same.

use crate::lstr::LStr;
use crate::value::{Callable, Exotic, Gc, Object, Property, Value};
use std::rc::Rc;
use std::sync::OnceLock;

/// Piece boundaries as byte offsets into the source string.
enum Offsets {
    /// Consecutive pieces of one split: piece `k` is `starts[k] .. starts[k + 1] - sep`
    /// (`starts` has one entry past the last piece). 4 bytes per piece.
    Contig { starts: Box<[u32]>, sep: u32 },
    /// Arbitrary pieces (a filtered view): piece `k` is `pairs[2k] .. pairs[2k + 1]`.
    Pairs(Box<[u32]>),
}

/// A split view's state (see the module docs).
pub(crate) struct SplitView {
    src: LStr,
    offs: Offsets,
    /// Element reads so far: past twice the piece count the array is being re-read, and
    /// [`fast_get`] materializes it (one copy per piece instead of one per read).
    reads: std::cell::Cell<u32>,
}

impl SplitView {
    /// The number of pieces.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        match &self.offs {
            Offsets::Contig { starts, .. } => starts.len() - 1,
            Offsets::Pairs(p) => p.len() / 2,
        }
    }

    /// Byte range of piece `k` (`k < len`).
    #[inline]
    pub(crate) fn range(&self, k: usize) -> (usize, usize) {
        match &self.offs {
            Offsets::Contig { starts, sep } => {
                (starts[k] as usize, (starts[k + 1] - sep) as usize)
            }
            Offsets::Pairs(p) => (p[2 * k] as usize, p[2 * k + 1] as usize),
        }
    }

    /// Piece `k` as a fresh string (`k < len`), for a read: a plain copy. Most reads are
    /// transient, and copying a line is cheaper than registering a string view of the root.
    #[inline]
    pub(crate) fn piece(&self, k: usize) -> LStr {
        let (a, b) = self.range(k);
        LStr::from(&self.src.as_str()[a..b])
    }

    /// Piece `k` as it is stored when the view materializes: like the eager split, a string
    /// view of the source's root for long pieces (see [`LStr::sub`]), a copy for short ones.
    #[inline]
    pub(crate) fn piece_kept(&self, k: usize) -> LStr {
        let (a, b) = self.range(k);
        self.src.sub(&self.src.as_str()[a..b])
    }

    /// Piece `k` as a value, or `None` past the end.
    #[inline]
    pub(crate) fn get(&self, k: usize) -> Option<Value> {
        (k < self.len()).then(|| Value::Str(self.piece(k)))
    }

    /// Heap bytes owned by the view state (the offsets; the source string is shared).
    pub(crate) fn offsets_bytes(&self) -> usize {
        match &self.offs {
            Offsets::Contig { starts, .. } => starts.len() * 4,
            Offsets::Pairs(p) => p.len() * 4,
        }
    }

    /// The source string (for memory accounting).
    pub(crate) fn source(&self) -> &LStr {
        &self.src
    }

    /// The view of pieces `from..to` of this one (`from <= to <= len`).
    pub(crate) fn slice(&self, from: usize, to: usize) -> SplitView {
        let offs = match &self.offs {
            Offsets::Contig { starts, sep } => Offsets::Contig {
                starts: starts[from..=to].into(),
                sep: *sep,
            },
            Offsets::Pairs(p) => Offsets::Pairs(p[2 * from..2 * to].into()),
        };
        SplitView { src: self.src.clone(), offs, reads: Default::default() }
    }

    /// The view of the pieces `keep` selects, in order.
    pub(crate) fn subset(&self, keep: &[u32]) -> SplitView {
        let mut pairs = Vec::with_capacity(keep.len() * 2);
        for &k in keep {
            let (a, b) = self.range(k as usize);
            pairs.push(a as u32);
            pairs.push(b as u32);
        }
        SplitView {
            src: self.src.clone(),
            offs: Offsets::Pairs(pairs.into_boxed_slice()),
            reads: Default::default(),
        }
    }
}

/// The minimum piece count for a split to come back as a view (`LUMEN_SPLIT_VIEW_MIN`, default
/// 64; `0` makes every eligible split a view, for testing). Smaller results stay eager.
pub(crate) fn min_pieces() -> usize {
    #[cfg(test)]
    if let Some(n) = TEST_MIN.with(|m| m.get()) {
        return n;
    }
    static MIN: OnceLock<usize> = OnceLock::new();
    *MIN.get_or_init(|| {
        std::env::var("LUMEN_SPLIT_VIEW_MIN")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(64)
    })
}

#[cfg(test)]
thread_local! {
    /// A per-thread [`min_pieces`] override for unit tests.
    pub(crate) static TEST_MIN: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// Split `src` on the non-empty `sep` into at most `limit` pieces, byte-wise (the same pieces
/// as the eager `str::split` path). `None` when the result should stay eager: too few pieces,
/// or offsets that would not fit in `u32`.
pub(crate) fn split_offsets(src: &LStr, sep: &str, limit: usize) -> Option<SplitView> {
    debug_assert!(!sep.is_empty());
    let s = src.as_str();
    if s.len() as u64 + sep.len() as u64 >= u32::MAX as u64 || limit == 0 {
        return None;
    }
    let min = min_pieces();
    // Cheap rejection before allocating: small results stay ordinary arrays.
    if min > 1 && limit < min {
        return None;
    }
    let mut starts: Vec<u32> = Vec::new();
    starts.push(0);
    for (at, _) in s.match_indices(sep) {
        if starts.len() >= limit {
            break;
        }
        starts.push((at + sep.len()) as u32);
    }
    let pieces = starts.len();
    if pieces < min {
        return None;
    }
    // The end sentinel: the last kept piece ends at the next separator (a `limit` cut) or at
    // the end of the source.
    let from = *starts.last().unwrap() as usize;
    let last_end = match s[from..].find(sep) {
        Some(at) => from + at,
        None => s.len(),
    };
    starts.push((last_end + sep.len()) as u32);
    Some(SplitView {
        src: src.clone(),
        offs: Offsets::Contig { starts: starts.into_boxed_slice(), sep: sep.len() as u32 },
        reads: Default::default(),
    })
}

/// A new split view object over `view` on `array_proto`.
pub(crate) fn make_view(array_proto: &Gc, view: SplitView) -> Value {
    let n = view.len();
    let obj = Object::new(Some(array_proto.clone()));
    {
        let mut b = obj.borrow_mut();
        b.props.mark_array();
        b.props.insert(
            "length",
            Property::data(Value::Num(n as f64), true, false, false),
        );
        b.exotic = Exotic::SplitView;
        b.call = Callable::SplitView(Rc::new(view));
        b.ic_plain.set(false);
    }
    Value::Obj(obj)
}

/// `o`'s view state while it is a split view.
#[inline]
pub(crate) fn view_of(o: &Gc) -> Option<Rc<SplitView>> {
    let b = o.try_borrow().ok()?;
    match &b.call {
        Callable::SplitView(v) if b.exotic == Exotic::SplitView => Some(v.clone()),
        _ => None,
    }
}

/// Whether `o` is still the split view whose state is `snap`.
#[inline]
pub(crate) fn is_same_view(o: &Gc, snap: &Rc<SplitView>) -> bool {
    match o.try_borrow() {
        Ok(b) => matches!(&b.call, Callable::SplitView(v) if Rc::ptr_eq(v, snap)),
        Err(_) => false,
    }
}

/// Whether `o` is (still) a split view.
#[inline]
pub(crate) fn is_view(o: &Gc) -> bool {
    matches!(o.try_borrow(), Ok(b) if b.exotic == Exotic::SplitView)
}

/// Turn a split view into the ordinary Array it stands for, in place (see the module docs).
/// No-op for anything else. The object must not be borrowed.
#[inline]
pub(crate) fn unview(o: &Gc) {
    if is_view(o) {
        materialize(o);
    }
}

/// [`unview`] for a value.
#[inline]
pub(crate) fn unview_val(v: &Value) {
    if let Value::Obj(o) = v {
        unview(o);
    }
}

#[cold]
#[inline(never)]
fn materialize(o: &Gc) {
    let view = {
        let mut b = o.borrow_mut();
        if b.exotic != Exotic::SplitView {
            return;
        }
        match std::mem::replace(&mut b.call, Callable::None) {
            Callable::SplitView(v) => v,
            other => {
                b.call = other;
                return;
            }
        }
    };
    let n = view.len();
    let mut elems: Vec<Property> = Vec::with_capacity(n);
    for k in 0..n {
        elems.push(Property::plain(Value::Str(view.piece_kept(k))));
    }
    drop(view);
    let mut b = o.borrow_mut();
    b.props.adopt_packed_elements(elems);
    b.exotic = Exotic::Array;
    b.ic_plain.set(true);
}

/// `o.length` when `o` is a split view (its stored own `length` data property).
#[inline]
pub(crate) fn view_length(o: &Gc) -> Option<Value> {
    let b = o.try_borrow().ok()?;
    if b.exotic != Exotic::SplitView {
        return None;
    }
    b.props.length_property().map(|p| p.value())
}

/// Own element `k` of `o` when `o` is a split view and `k` is in range; `None` otherwise (not a
/// view, or past its end — the caller takes the generic path).
#[inline]
pub(crate) fn fast_get(o: &Gc, k: usize) -> Option<Value> {
    let (v, reread) = {
        let b = o.try_borrow().ok()?;
        if b.exotic != Exotic::SplitView {
            return None;
        }
        let Callable::SplitView(view) = &b.call else { return None };
        let r = view.reads.get().saturating_add(1);
        view.reads.set(r);
        (view.get(k)?, r as usize > 2 * view.len())
    };
    // Only when nothing up the stack borrows the object (a read inside a walk over it).
    if reread && o.try_borrow_mut().is_ok() {
        materialize(o);
    }
    Some(v)
}

/// Own element `key` of `o` when `o` is a split view and `key` one of its indices.
#[inline]
pub(crate) fn own_elem(o: &Gc, key: &str) -> Option<Value> {
    let b = o.try_borrow().ok()?;
    own_elem_obj(&b, key)
}

/// [`own_elem`] on a borrowed object.
#[inline]
pub(crate) fn own_elem_obj(b: &Object, key: &str) -> Option<Value> {
    if b.exotic != Exotic::SplitView {
        return None;
    }
    match &b.call {
        Callable::SplitView(v) => index_of_key(v, key).map(|k| Value::Str(v.piece(k))),
        _ => None,
    }
}

/// Whether `b` is a split view with an own element `key` (no string built).
#[inline]
pub(crate) fn has_own_elem_obj(b: &Object, key: &str) -> bool {
    if b.exotic != Exotic::SplitView {
        return false;
    }
    match &b.call {
        Callable::SplitView(v) => index_of_key(v, key).is_some(),
        _ => false,
    }
}

/// `key` as an element index of the view: `Some(k)` for a canonical array index below the
/// view's length.
#[inline]
pub(crate) fn index_of_key(view: &SplitView, key: &str) -> Option<usize> {
    let k = crate::value::canonical_index(key)? as usize;
    (k < view.len()).then_some(k)
}

#[cfg(test)]
mod tests {
    use crate::{Completion, Engine};

    fn eval_with(min: usize, src: &str) -> String {
        super::TEST_MIN.with(|m| m.set(Some(min)));
        let out = match Engine::new().eval(src, false).expect("parse") {
            Completion::Value(v) => v,
            Completion::Throw { name, message } => format!("THREW {name}: {message}"),
        };
        super::TEST_MIN.with(|m| m.set(None));
        out
    }

    /// `src` gives the same completion with every split a view as with none.
    fn same(src: &str) -> String {
        let lazy = eval_with(0, src);
        let eager = eval_with(usize::MAX, src);
        assert_eq!(lazy, eager, "split view diverges from eager split for:\n{src}");
        assert!(!lazy.starts_with("THREW"), "{lazy}");
        lazy
    }

    fn views_alive() -> usize {
        crate::value::memwalk::walk().split_views
    }

    fn val(e: &mut Engine, src: &str) -> String {
        match e.eval(src, false).expect("parse") {
            Completion::Value(v) => v,
            Completion::Throw { name, message } => panic!("threw {name}: {message}"),
        }
    }

    #[test]
    fn view_is_created_and_materialized() {
        super::TEST_MIN.with(|m| m.set(Some(0)));
        let mut e = Engine::new();
        let before = views_alive();
        val(&mut e, "var v = 'a,b,c'.split(','); v.length");
        assert_eq!(views_alive(), before + 1);
        // Reads stay lazy.
        let r = val(
            &mut e,
            "v[0] + v[2] + v.length + v.indexOf('b') + v.join('-') + JSON.stringify(v) + v.includes('c')",
        );
        assert_eq!(r, "ac31a-b-c[\"a\",\"b\",\"c\"]true");
        assert_eq!(views_alive(), before + 1);
        // A write materializes in place.
        val(&mut e, "v.push('d')");
        assert_eq!(views_alive(), before);
        assert_eq!(val(&mut e, "v.join('')"), "abcd");
        // Slices and filters of a view are views.
        let before = views_alive();
        val(&mut e, "var big = 'a,b,c,d,e,f'.split(','); var s = big.slice(1, 4); var f = big.filter(x => x > 'c');");
        assert_eq!(views_alive(), before + 3);
        assert_eq!(val(&mut e, "s.join() + '|' + f.join() + '|' + s.length + f.length"), "b,c,d|d,e,f|33");
        super::TEST_MIN.with(|m| m.set(Some(usize::MAX)));
        let before = views_alive();
        val(&mut e, "var w = 'a,b,c'.split(',')");
        assert_eq!(views_alive(), before);
        super::TEST_MIN.with(|m| m.set(None));
    }

    #[test]
    fn pieces_match_eager_split() {
        same(
            r#"
            const cases = [
              ['a,b,c', ','], [',a,,b,', ','], ['', ','], [',', ','], [',,', ','],
              ['abc', ','], ['a--b----c--', '--'], ['xxxx', 'xx'], ['xxx', 'xx'],
              ['héllo wörld \u{1F600} \u{1F600}x', ' '],
              ['\u{1F600}a\u{1F600}b\u{1F600}', '\u{1F600}'],
              ['a\uD800b\uD800c\uDC00d', 'b'], ['\uD800,\uDC00,􏿿', ','],
              ['x𐀀y', '\uDC00'], ['x𐀀y', '\uD800'],
              ['line1\nline2\r\nline3\n', '\n'], ['日本語,中文,한국어', ','],
            ];
            const out = [];
            for (const [s, sep] of cases) {
              for (const lim of [undefined, 0, 1, 2, 3, 100, -1, 2 ** 32 + 1]) {
                const p = s.split(sep, lim);
                const codes = p.map(x => [...x].map(c => c.charCodeAt(0)));
                out.push([p.length, p, codes, p[0], p[p.length - 1], p[p.length], Array.isArray(p)]);
              }
            }
            JSON.stringify(out)
            "#,
        );
    }

    #[test]
    fn reflection_matches_array() {
        same(
            r#"
            const v = 'a,b,c,d'.split(',');
            const r = [];
            r.push(Array.isArray(v), v instanceof Array, Object.getPrototypeOf(v) === Array.prototype);
            r.push(v.constructor === Array, Object.prototype.toString.call(v));
            r.push(JSON.stringify(Object.getOwnPropertyDescriptor(v, 'length')));
            r.push(JSON.stringify(Object.getOwnPropertyDescriptor(v, '1')));
            r.push(Object.getOwnPropertyDescriptor(v, '9') === undefined);
            r.push(v.hasOwnProperty(0), v.hasOwnProperty('3'), v.hasOwnProperty('4'), v.hasOwnProperty('01'));
            r.push(Object.hasOwn(v, 2), 2 in v, '2' in v, 7 in v, 'length' in v, 'push' in v);
            r.push(Reflect.has(v, 1), Reflect.get(v, 1), Reflect.get(v, 'length'));
            r.push(v['-0'], v['1.0'], v[1.5], v['0x1'], v[4294967295]);
            r.push(JSON.stringify(v), String(v), v + '', `${v}`);
            r.push(Object.isExtensible(v), Object.isFrozen(v), Object.isSealed(v));
            r.push(v.at(-1), v.includes('c'), v.indexOf('d'), v.lastIndexOf('a'), v.join('|'));
            r.push(v.map(x => x + x).join(), v.filter(x => x > 'b').join(), v.some(x => x === 'c'));
            r.push(v.every(x => x.length === 1), v.find(x => x > 'a'), v.findIndex(x => x === 'c'));
            r.push(v.reduce((a, x) => a + x, ''), v.reduceRight((a, x) => a + x, ''));
            r.push(v.slice(1, 3).join(), v.concat(['e'], 'f').join(), [].concat(v).length);
            r.push([...v].join(), Array.from(v).join(), Array.from(v, x => x.toUpperCase()).join());
            r.push(Array.prototype.slice.call(v, -2).join(), v.toString(), v.toLocaleString());
            r.push(v.flat().join(), v.flatMap(x => [x, x]).join(), v.entries().next().value.join());
            r.push([...v.keys()].join(), [...v.values()].join(), v.with(0, 'z').join());
            r.push(v.toReversed().join(), v.toSorted().join(), v.toSpliced(1, 1).join());
            let fo = ''; for (const x of v) fo += x; r.push(fo);
            let fi = ''; for (const k in v) fi += k; r.push(fi);
            let fe = ''; v.forEach((x, i, a) => { fe += i + x + (a === v); }); r.push(fe);
            const [p, q, ...rest] = v; r.push(p, q, rest.join());
            r.push(JSON.stringify({ ...v }), Object.keys(v).join(), Object.values(v).join());
            r.push(JSON.stringify(Object.entries(v)), Reflect.ownKeys(v).join());
            r.push(Object.getOwnPropertyNames(v).join(), JSON.stringify(Object.getOwnPropertyDescriptors(v)));
            r.push(JSON.stringify(Object.assign({}, v)), JSON.stringify(Object.assign([], v)));
            r.push(Math.max.apply(null, 'a,b'.split(',').map(x => x.length)), String.fromCharCode(...'65,66'.split(',')));
            r.push(JSON.stringify(v, null, 1), JSON.stringify({ v }), JSON.stringify(v, (k, x) => x));
            r.push(JSON.stringify(v, ['0', '1']));
            const o = Object.create(v); r.push(o[1], o.length, 1 in o, Array.isArray(o));
            JSON.stringify(r)
            "#,
        );
    }

    #[test]
    fn every_mutation_materializes() {
        same(
            r#"
            const S = 'a,b,c,d,e';
            const f = (op) => { const v = S.split(','); let res; try { res = op(v); } catch (e) { res = 'E:' + e.name; }
              let desc; try { desc = Object.getOwnPropertyDescriptors(v); } catch (e) { desc = e.name; }
              return JSON.stringify([res === v ? 'self' : res, v, v.length, Object.keys(v), Array.isArray(v),
                Object.isFrozen(v), Object.isExtensible(v), desc, Object.getPrototypeOf(v) === Array.prototype]); };
            const ops = [
              v => v.push('x'), v => v.pop(), v => v.shift(), v => v.unshift('x', 'y'),
              v => v.splice(1, 2, 'q'), v => v.splice(2), v => v.reverse(), v => v.sort().join(),
              v => v.sort((a, b) => a < b ? 1 : -1), v => v.fill('z', 1, 3), v => v.copyWithin(0, 3),
              v => { v[0] = 'Z'; }, v => { v[2] = 'Z'; }, v => { v[9] = 'Z'; }, v => { v.length = 2; },
              v => { v.length = 7; }, v => { v.length = 0; }, v => { v.foo = 1; }, v => { v['01'] = 1; },
              v => { v[-1] = 1; }, v => delete v[1], v => delete v[4], v => delete v.length,
              v => delete v[10], v => Object.freeze(v), v => Object.seal(v), v => Object.preventExtensions(v),
              v => Object.defineProperty(v, 1, { value: 'D' }), v => Object.defineProperty(v, 1, { get() { return 'G'; } }),
              v => Object.defineProperty(v, 'length', { value: 3 }), v => Object.defineProperty(v, 'length', { writable: false }),
              v => Object.setPrototypeOf(v, null), v => Object.setPrototypeOf(v, Object.prototype),
              v => { v.__proto__ = { x: 1 }; }, v => Reflect.set(v, 0, 'R'), v => Reflect.set(v, 'length', 1),
              v => Reflect.deleteProperty(v, 0), v => Reflect.defineProperty(v, 5, { value: 'F', enumerable: true, configurable: true, writable: true }),
              v => { v[0] += '!'; }, v => { v[1]++; }, v => { for (const k in v) v[k] = k; },
              v => { const p = new Proxy(v, {}); p[0] = 'P'; return p.length; },
              v => { with (v) { length = 1; } }, v => { const o = Object.create(v); o[0] = 'own'; return o[0]; },
              v => { Object.assign(v, { 0: 'A', 7: 'B' }); }, v => { v.forEach((x, i, a) => { a[i] = i; }); },
              v => { v.map((x, i, a) => a.pop()); }, v => { [v[0], v[1]] = [v[1], v[0]]; },
              v => { ({ a: v[3] } = { a: 'D' }); }, v => { v.length -= 1; }, v => Object.defineProperties(v, { 0: { value: 1 } }),
              v => { 'use strict'; Object.freeze(v); v[0] = 1; }, v => { Object.freeze(v); v.push(1); },
              v => { Object.seal(v); delete v[0]; return v[0]; }, v => v.push.apply(v, ['m', 'n']),
              v => Array.prototype.splice.call(v, 0, 1), v => { v[300] = 'big'; },
              v => { v[4294967295] = 'nonidx'; }, v => { v[Symbol.iterator] = null; try { [...v]; } catch (e) { return e.name; } },
              v => { v.filter((x, i, a) => { a[4] = 'mut'; return true; }).join(); },
              v => v.filter((x, i, a) => { if (i === 0) a.length = 2; return true; }).join(),
              v => v.filter((x, i, a) => { if (i === 0) a[3] = 'NEW'; return x !== 'b'; }).join(),
              v => { Array.prototype[7] = 'proto'; try { v.length = 8; return [v[7], v.slice(6).join()]; } finally { delete Array.prototype[7]; } },
            ];
            JSON.stringify(ops.map(f))
            "#,
        );
    }

    #[test]
    fn views_in_generic_paths() {
        same(
            r#"
            const v = 'x,y,z'.split(',');
            const r = [];
            class A extends Array {}
            r.push(A.from(v).join(), A.from(v) instanceof A, Array.of(...v).join());
            r.push(v.slice().constructor === Array, v.map(x => x).constructor === Array);
            const w = 'k,l,m'.split(','); w.constructor = { [Symbol.species]: A }; r.push(w.slice(1) instanceof A);
            const w2 = 'k,l,m'.split(','); w2.constructor = { [Symbol.species]: A }; r.push(w2.filter(x => 1) instanceof A);
            r.push(new Set(v).size, [...new Map(v.map((x, i) => [x, i]))].join(';'));
            r.push(Object.fromEntries('a=1,b=2'.split(',').map(s => s.split('='))).a);
            r.push(v.concat(v).length, [v, v].flat().join(), JSON.stringify([v]));
            r.push(String.raw({ raw: 'a,b,c'.split(',') }, 1, 2));
            r.push(Function.prototype.apply.call((...a) => a.join('+'), null, v));
            r.push(Reflect.apply((...a) => a.length, null, v), Reflect.construct(Array, v).join());
            r.push(new Array(...v).length, Array.prototype.concat.call(v, 1).length);
            r.push(v.indexOf('z', -1), v.includes('x', 1), v.lastIndexOf('x', -4));
            const idx = []; for (let i = 0; i < v.length; i++) idx.push(v[i], v[i + 1]); r.push(JSON.stringify(idx));
            const keys = []; for (const [i, x] of v.entries()) keys.push(i + x); r.push(keys.join());
            r.push(typeof v, v == 'x,y,z', v === v, Object.is(v, v));
            r.push(Array.prototype.join.call(Object.create('p,q'.split(',')), '/'));
            const big = Array.from({ length: 200 }, (_, i) => i).join('\n').split('\n');
            let sum = 0; for (let i = 0; i < big.length; i++) sum += +big[i]; r.push(sum);
            r.push(big.filter(x => x % 7 === 0).length, big.slice(10, 20).join(), big.slice(-3).length);
            r.push(big.filter(x => x > '5').slice(2, 5).join(), big.slice(5).filter(x => x.length === 1).join());
            r.push(big.slice(3, 1).length, big.slice(-500, 2).join(), big.slice(1.5, '4').join(), big.slice(NaN, Infinity).length);
            r.push(big.filter(() => false).length, Array.isArray(big.filter(() => false)), big.filter(() => true).length);
            const sl = big.slice(100, 150); sl.push('extra'); r.push(sl.length, sl[0], sl[50], big.length);
            const fl = big.filter(x => x.endsWith('9')); fl[0] = 'Q'; r.push(fl.join().slice(0, 30), big[9]);
            r.push(JSON.stringify(big.slice(0, 3).concat(big.slice(197))));
            const g = big.filter(function (x) { return this.k === 1 && x < '2'; }, { k: 1 }); r.push(g.length);
            JSON.stringify(r)
            "#,
        );
    }

    #[test]
    fn hot_loops_over_views() {
        // Hot loops over a view (tier-up paths read elements through the helpers).
        same(
            r#"
            const text = Array.from({ length: 3000 }, (_, i) => (i % 5 ? 'line ' + i : '! comment ' + i)).join('\n');
            function parse(t) {
              const lines = t.split('\n');
              let n = 0, bytes = 0, last = '';
              for (let i = 0; i < lines.length; i++) {
                const l = lines[i];
                if (l.length === 0 || l[0] === '!') continue;
                n++; bytes += l.length; last = l;
              }
              return [n, bytes, last, lines.length];
            }
            let out;
            for (let k = 0; k < 30; k++) out = parse(text);
            const lines = text.split('\n');
            let a = 0; for (let k = 0; k < 20; k++) for (const l of lines) a += l.length;
            let b = 0; for (let k = 0; k < 20; k++) lines.forEach(l => { b += l.length; });
            let c = 0; for (let k = 0; k < 20; k++) { const x = lines; for (let i = x.length - 1; i >= 0; i--) c += x[i].charCodeAt(0); }
            JSON.stringify([out, a, b, c, lines.map(l => l.length).reduce((x, y) => x + y)])
            "#,
        );
    }
}
