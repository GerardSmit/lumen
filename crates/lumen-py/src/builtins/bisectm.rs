//! `_bisect`: binary search and insertion into sorted sequences.

/// Bisection algorithms.
///
/// This module provides support for maintaining a list in sorted order without
/// having to sort the list after each insertion. For long lists of items with
/// expensive comparison operations, this can be an improvement over the more
/// common approach.
#[lumen_bind::module(name = "_bisect")]
pub mod _bisect {
    use crate::ast::CmpOp;
    use crate::object::*;
    use crate::vm::Interp;

    fn item(it: &mut Interp, a: &Value, i: i64) -> R<Value> {
        if let Value::Obj(o) = a {
            if o.cls.is_none() {
                if let Some(l) = list_of(a) {
                    if let Some(v) = l.borrow().get(i as usize) {
                        return Ok(v.clone());
                    }
                }
            }
        }
        it.getitem(a, &Value::Int(i))
    }

    fn less(it: &mut Interp, x: &Value, y: &Value) -> R<bool> {
        let r = it.rich_compare(CmpOp::Lt, x, y)?;
        it.truthy(&r)
    }

    /// The insertion point for `x`; `right` puts it after any equal entries.
    fn search(
        it: &mut Interp,
        a: &Value,
        x: &Value,
        lo: i64,
        hi: Option<&Value>,
        key: Option<&Value>,
        right: bool,
    ) -> R<i64> {
        if lo < 0 {
            return Err(it.value_error("lo must be non-negative"));
        }
        let mut lo = lo;
        let mut hi = match hi {
            Some(h) if !h.is_none() => it.index_of(h)?,
            _ => -1,
        };
        if hi == -1 {
            hi = it.len_of(a)? as i64;
        }
        let key = key.filter(|k| !k.is_none());
        while lo < hi {
            let mid = ((lo as u64 + hi as u64) / 2) as i64;
            let mut probe = item(it, a, mid)?;
            if let Some(k) = key {
                probe = it.call(k, vec![probe], Vec::new())?;
            }
            let go_right = if right {
                !less(it, x, &probe)?
            } else {
                less(it, &probe, x)?
            };
            if go_right {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }

    fn insort(
        it: &mut Interp,
        a: &Value,
        x: &Value,
        lo: i64,
        hi: Option<&Value>,
        key: Option<&Value>,
        right: bool,
    ) -> R<()> {
        let probe = match key.filter(|k| !k.is_none()) {
            Some(k) => it.call(k, vec![x.clone()], Vec::new())?,
            None => x.clone(),
        };
        let index = search(it, a, &probe, lo, hi, key, right)?;
        if let (Value::Obj(o), Some(l)) = (a, list_of(a)) {
            if o.cls.is_none() {
                let mut l = l.borrow_mut();
                let at = (index.max(0) as usize).min(l.len());
                l.insert(at, x.clone());
                return Ok(());
            }
        }
        it.call_method(a, "insert", vec![Value::Int(index), x.clone()])?;
        Ok(())
    }

    /// Return the index where to insert item x in list a, assuming a is sorted.
    ///
    /// The return value i is such that all e in a[:i] have e <= x, and all e in
    /// a[i:] have e > x.  So if x already appears in the list, a.insert(i, x) will
    /// insert just after the rightmost x already there.
    ///
    /// Optional args lo (default 0) and hi (default len(a)) bound the
    /// slice of a to be searched.
    ///
    /// A custom key function can be supplied to customize the sort order.
    #[op]
    fn bisect_right(
        it: &mut Interp,
        #[kw] a: &Value,
        #[kw] x: &Value,
        #[kw]
        #[default(0)]
        lo: i64,
        #[kw] hi: Option<&Value>,
        #[kwonly] key: Option<&Value>,
    ) -> R<i64> {
        search(it, a, x, lo, hi, key, true)
    }

    /// Return the index where to insert item x in list a, assuming a is sorted.
    ///
    /// The return value i is such that all e in a[:i] have e < x, and all e in
    /// a[i:] have e >= x.  So if x already appears in the list, a.insert(i, x) will
    /// insert just before the leftmost x already there.
    ///
    /// Optional args lo (default 0) and hi (default len(a)) bound the
    /// slice of a to be searched.
    ///
    /// A custom key function can be supplied to customize the sort order.
    #[op]
    fn bisect_left(
        it: &mut Interp,
        #[kw] a: &Value,
        #[kw] x: &Value,
        #[kw]
        #[default(0)]
        lo: i64,
        #[kw] hi: Option<&Value>,
        #[kwonly] key: Option<&Value>,
    ) -> R<i64> {
        search(it, a, x, lo, hi, key, false)
    }

    /// Insert item x in list a, and keep it sorted assuming a is sorted.
    ///
    /// If x is already in a, insert it to the right of the rightmost x.
    ///
    /// Optional args lo (default 0) and hi (default len(a)) bound the
    /// slice of a to be searched.
    ///
    /// A custom key function can be supplied to customize the sort order.
    #[op]
    fn insort_right(
        it: &mut Interp,
        #[kw] a: &Value,
        #[kw] x: &Value,
        #[kw]
        #[default(0)]
        lo: i64,
        #[kw] hi: Option<&Value>,
        #[kwonly] key: Option<&Value>,
    ) -> R<()> {
        insort(it, a, x, lo, hi, key, true)
    }

    /// Insert item x in list a, and keep it sorted assuming a is sorted.
    ///
    /// If x is already in a, insert it to the left of the leftmost x.
    ///
    /// Optional args lo (default 0) and hi (default len(a)) bound the
    /// slice of a to be searched.
    ///
    /// A custom key function can be supplied to customize the sort order.
    #[op]
    fn insort_left(
        it: &mut Interp,
        #[kw] a: &Value,
        #[kw] x: &Value,
        #[kw]
        #[default(0)]
        lo: i64,
        #[kw] hi: Option<&Value>,
        #[kwonly] key: Option<&Value>,
    ) -> R<()> {
        insort(it, a, x, lo, hi, key, false)
    }
}
