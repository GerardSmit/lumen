//! `_heapq`: binary min-heaps (and the max-heap helpers `heapq.merge` uses) on plain lists, with
//! CPython's comparison order and its checks for lists resized by a comparison.

/// Heap queue algorithm (a.k.a. priority queue).
///
/// Heaps are arrays for which a[k] <= a[2*k+1] and a[k] <= a[2*k+2] for
/// all k, counting elements from 0.  For the sake of comparison,
/// non-existing elements are considered to be infinite.  The interesting
/// property of a heap is that a[0] is always its smallest element.
#[lumen_bind::module(name = "_heapq")]
pub mod _heapq {
    use crate::ast::CmpOp;
    use crate::object::*;
    use crate::vm::{dict_set_str, Interp};
    use std::cell::RefCell;

    #[init]
    fn init(it: &mut Interp, m: &Value) -> R<()> {
        if let Value::Obj(m) = m {
            dict_set_str(&it.module_dict(m), "__about__", Value::str("Heap queues"));
        }
        Ok(())
    }

    type List = RefCell<Vec<Value>>;

    fn list<'a>(it: &mut Interp, heap: &'a Value, func: &str) -> R<&'a List> {
        match list_of(heap) {
            Some(l) => Ok(l),
            None => {
                let t = it.type_name_of(heap);
                Err(it.type_error(&format!("{func}() argument 1 must be list, not {t}")))
            }
        }
    }

    /// `a < b`, or `b < a` for the max-heap variants.
    fn less(it: &mut Interp, a: &Value, b: &Value, max: bool) -> R<bool> {
        let (x, y) = if max { (b, a) } else { (a, b) };
        let r = it.rich_compare(CmpOp::Lt, x, y)?;
        it.truthy(&r)
    }

    fn changed(it: &mut Interp) -> Obj {
        it.runtime_error("list changed size during iteration")
    }

    fn get(l: &List, i: usize) -> Value {
        l.borrow()[i].clone()
    }

    fn sift_down(it: &mut Interp, l: &List, start: usize, mut pos: usize, max: bool) -> R<()> {
        let size = l.borrow().len();
        if pos >= size {
            return Err(it.new_exc_str("IndexError", "list index out of range"));
        }
        let newitem = get(l, pos);
        while pos > start {
            let parentpos = (pos - 1) >> 1;
            let parent = get(l, parentpos);
            let lt = less(it, &newitem, &parent, max)?;
            if size != l.borrow().len() {
                return Err(changed(it));
            }
            if !lt {
                break;
            }
            let mut v = l.borrow_mut();
            v.swap(parentpos, pos);
            pos = parentpos;
        }
        Ok(())
    }

    fn sift_up(it: &mut Interp, l: &List, mut pos: usize, max: bool) -> R<()> {
        let end = l.borrow().len();
        let start = pos;
        if pos >= end {
            return Err(it.new_exc_str("IndexError", "list index out of range"));
        }
        let limit = end >> 1;
        while pos < limit {
            let mut child = 2 * pos + 1;
            if child + 1 < end {
                let (a, b) = (get(l, child), get(l, child + 1));
                if !less(it, &a, &b, max)? {
                    child += 1;
                }
                if end != l.borrow().len() {
                    return Err(changed(it));
                }
            }
            l.borrow_mut().swap(child, pos);
            pos = child;
        }
        sift_down(it, l, start, pos, max)
    }

    fn pop(it: &mut Interp, heap: &Value, func: &str, max: bool) -> R<Value> {
        let l = list(it, heap, func)?;
        let last = match l.borrow_mut().pop() {
            Some(v) => v,
            None => return Err(it.new_exc_str("IndexError", "index out of range")),
        };
        if l.borrow().is_empty() {
            return Ok(last);
        }
        let top = std::mem::replace(&mut l.borrow_mut()[0], last);
        sift_up(it, l, 0, max)?;
        Ok(top)
    }

    fn replace(it: &mut Interp, heap: &Value, item: &Value, func: &str, max: bool) -> R<Value> {
        let l = list(it, heap, func)?;
        if l.borrow().is_empty() {
            return Err(it.new_exc_str("IndexError", "index out of range"));
        }
        let top = std::mem::replace(&mut l.borrow_mut()[0], item.clone());
        sift_up(it, l, 0, max)?;
        Ok(top)
    }

    fn heapify_with(it: &mut Interp, heap: &Value, func: &str, max: bool) -> R<()> {
        let l = list(it, heap, func)?;
        let n = l.borrow().len();
        for i in (0..n / 2).rev() {
            sift_up(it, l, i, max)?;
        }
        Ok(())
    }

    /// Push item onto heap, maintaining the heap invariant.
    #[op]
    fn heappush(it: &mut Interp, heap: &Value, item: &Value) -> R<()> {
        let l = list(it, heap, "heappush")?;
        l.borrow_mut().push(item.clone());
        let n = l.borrow().len();
        sift_down(it, l, 0, n - 1, false)
    }

    /// Pop the smallest item off the heap, maintaining the heap invariant.
    #[op]
    fn heappop(it: &mut Interp, heap: &Value) -> R<Value> {
        pop(it, heap, "heappop", false)
    }

    /// Pop and return the current smallest value, and add the new item.
    ///
    /// This is more efficient than heappop() followed by heappush(), and can be
    /// more appropriate when using a fixed-size heap.  Note that the value
    /// returned may be larger than item!  That constrains reasonable uses of
    /// this routine unless written as part of a conditional replacement:
    ///
    ///     if item > heap[0]:
    ///         item = heapreplace(heap, item)
    #[op]
    fn heapreplace(it: &mut Interp, heap: &Value, item: &Value) -> R<Value> {
        replace(it, heap, item, "heapreplace", false)
    }

    /// Push item on the heap, then pop and return the smallest item from the heap.
    ///
    /// The combined action runs more efficiently than heappush() followed by
    /// a separate call to heappop().
    #[op]
    fn heappushpop(it: &mut Interp, heap: &Value, item: &Value) -> R<Value> {
        let l = list(it, heap, "heappushpop")?;
        let top = match l.borrow().first() {
            Some(v) => v.clone(),
            None => return Ok(item.clone()),
        };
        if !less(it, &top, item, false)? {
            return Ok(item.clone());
        }
        if l.borrow().is_empty() {
            return Err(it.new_exc_str("IndexError", "index out of range"));
        }
        let top = std::mem::replace(&mut l.borrow_mut()[0], item.clone());
        sift_up(it, l, 0, false)?;
        Ok(top)
    }

    /// Push item onto max heap, maintaining the heap invariant.
    #[op]
    fn heappush_max(it: &mut Interp, heap: &Value, item: &Value) -> R<()> {
        let l = list(it, heap, "heappush_max")?;
        l.borrow_mut().push(item.clone());
        let n = l.borrow().len();
        sift_down(it, l, 0, n - 1, true)
    }

    /// Maxheap variant of heappushpop.
    #[op]
    fn heappushpop_max(it: &mut Interp, heap: &Value, item: &Value) -> R<Value> {
        let l = list(it, heap, "heappushpop_max")?;
        let top = match l.borrow().first() {
            Some(v) => v.clone(),
            None => return Ok(item.clone()),
        };
        if !less(it, &top, item, true)? {
            return Ok(item.clone());
        }
        if l.borrow().is_empty() {
            return Err(it.new_exc_str("IndexError", "index out of range"));
        }
        let top = std::mem::replace(&mut l.borrow_mut()[0], item.clone());
        sift_up(it, l, 0, true)?;
        Ok(top)
    }

    /// Transform list into a heap, in-place, in O(len(heap)) time.
    #[op]
    fn heapify(it: &mut Interp, heap: &Value) -> R<()> {
        heapify_with(it, heap, "heapify", false)
    }

    /// Maxheap variant of heappop.
    #[op]
    fn heappop_max(it: &mut Interp, heap: &Value) -> R<Value> {
        pop(it, heap, "heappop_max", true)
    }

    /// Maxheap variant of heapreplace.
    #[op]
    fn heapreplace_max(it: &mut Interp, heap: &Value, item: &Value) -> R<Value> {
        replace(it, heap, item, "heapreplace_max", true)
    }

    /// Maxheap variant of heapify.
    #[op]
    fn heapify_max(it: &mut Interp, heap: &Value) -> R<()> {
        heapify_with(it, heap, "heapify_max", true)
    }
}
