//! Scalar replacement of an effect-free, bounded Array binding iterator walk.
use crate::{
    interpreter::Interp,
    value::{Exotic, Gc, Value},
};

// extra_protos is already rooted, saved and restored with the active realm. These
// private entries retain original method identities even after JS replaces properties.
const VALUES: &str = "%DestructureArrayValuesIntrinsic%";
const NEXT: &str = "%DestructureArrayNextIntrinsic%";
const LIMIT: usize = 16;

pub(super) fn original_next(i: &Interp) -> Option<&Gc> {
    i.extra_protos.get(NEXT)
}

pub(crate) fn remember_next(i: &mut Interp, proto: &Gc) {
    if let Some(Value::Obj(next)) = proto.borrow().props.get("next").map(|p| p.value()) {
        i.extra_protos.insert(NEXT, next);
    }
}

/// Realm initialization only; disabled realms never enter the scalar replacement.
pub(crate) fn remember_values(i: &mut Interp, values: &Value) {
    if std::env::var_os("LUMEN_NO_ARRAY_DESTRUCTURE").is_none() {
        if let Value::Obj(values) = values {
            i.extra_protos.insert(VALUES, values.clone());
        }
    }
}

/// `false` is a pure miss (nothing was pushed): the caller still owns the original input, and
/// must run the complete existing DestructureArr opcode, including its IteratorClose. `true`:
/// exactly `count` values were handed to `push`, in order.
pub(super) fn try_dense(
    i: &Interp,
    input: &Value,
    count: u16,
    push: impl FnMut(Value),
) -> bool {
    try_dense_opt(i, input, count, push).is_some()
}

/// The protector half of [`try_dense`] for a walk that stops at or before the end (`count <=
/// length`, checked by the caller): iterating `input` is the intrinsic Array Iterator and
/// closing it is a no-op. The JIT then reads the elements natively.
pub(crate) fn dense_ok(i: &Interp, input: &Value) -> bool {
    let Value::Obj(array) = input else {
        return false;
    };
    super::iter_fast::array_ok(i, array) && i.array_iter_return_absent()
}

fn try_dense_opt(
    i: &Interp,
    input: &Value,
    count: u16,
    mut push: impl FnMut(Value),
) -> Option<()> {
    let count = usize::from(count);
    if count > LIMIT {
        return None;
    }
    // Present only when this scalar replacement is enabled for the realm (checked, memoized,
    // by `pristine_array_iteration`).
    let Value::Obj(array) = input else {
        return None;
    };
    // Shape-memoized proof that GetIterator/IteratorStep over `array` are the intrinsic ones.
    if !super::iter_fast::array_ok(i, array) {
        return None;
    }
    let b = array.try_borrow().ok()?;
    let len = array_length(&b)?;
    // Exactly count==len still closes: no subsequent exhausted step occurred.
    if count <= len && !i.array_iter_return_absent() {
        return None;
    }
    let yielded = count.min(len);
    // Guard every read before cloning outputs; holes/inherited/accessor elements
    // fall back, since any one can execute JS and invalidate earlier proofs.
    for index in 0..yielded {
        if b.props.get_index(index as u32)?.accessor() {
            return None;
        }
    }
    for index in 0..yielded {
        push(b.props.get_index(index as u32)?.value());
    }
    for _ in yielded..count {
        push(Value::Undefined);
    }
    #[cfg(test)]
    SUCCESSES.with(|n| n.set(n.get() + 1));
    Some(())
}

fn array_length(b: &crate::value::Object) -> Option<usize> {
    if !matches!(b.exotic, Exotic::Array) || !b.ic_plain.get() {
        return None;
    }
    let p = b.props.length_property()?;
    if p.accessor() {
        return None;
    }
    let Value::Num(n) = p.value() else {
        return None;
    };
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 || n > u32::MAX as f64 {
        return None;
    }
    Some(n as usize)
}

#[cfg(test)]
thread_local! { static SUCCESSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    fn check(source: &str, expected: usize) {
        for tier in [Tier::Interp, Tier::Bytecode] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            super::SUCCESSES.with(|n| n.set(0));
            let source = format!("function assert(v){{if(!v)throw new Error('dense destructure');}} {source}; 'passed'");
            match engine.eval(&source, false).unwrap() {
                Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
            if tier != Tier::Interp && std::env::var_os("LUMEN_NO_ARRAY_DESTRUCTURE").is_none() {
                assert_eq!(
                    super::SUCCESSES.with(|n| n.get()),
                    expected,
                    "{tier:?}: actual scalar replacements"
                );
            } else {
                assert_eq!(super::SUCCESSES.with(|n| n.get()), 0);
            }
        }
    }

    #[test]
    fn zero_short_exact_and_long_patterns_copy_owned_outputs() {
        check(
            r#"
            function empty(a){var []=a;return 1;}
            function one(a){var [x]=a;return x;}
            function pair(a){var [x,y]=a;return x===undefined?0:y===undefined?x:x+y;}
            function three(a){var [x,y,z]=a;return z;}
            assert(empty([])===1&&empty([1])===1);
            assert(one([])===undefined&&one([3])===3&&one([3,4])===3);
            assert(pair([])===0&&pair([2])===2&&pair([2,3])===5&&pair([2,3,4])===5);
            assert(three([1,2])===undefined&&three([1,2,3])===3);
            function owned(a){var [x,y]=a;$262.gc();return x===y&&x.value===9;}
            var object={value:9};assert(owned([object,object]));
        "#,
            12,
        );
    }

    #[test]
    fn custom_next_iterator_and_return_keep_protocol_observations() {
        check(
            r#"
            function pair(a){var [x,y]=a;return x+y;}
            function three(a){var [x,y,z]=a;return z;}
            assert(pair([2,3])===5);
            var proto=Object.getPrototypeOf([].values()),originalNext=proto.next,steps=0,closed=0;
            proto.next=function(){steps++;return {done:false,value:7};};
            proto.return=function(){closed++;return {};};
            assert(pair([2,3])===14&&steps===2&&closed===1);
            proto.next=originalNext;
            assert(pair([2,3])===5&&closed===2);
            assert(three([2,3])===undefined&&closed===2);
            delete proto.return;
            var originalValues=Array.prototype.values,originalIterator=Array.prototype[Symbol.iterator];
            function custom(){return {next:function(){return {done:false,value:4};},return:function(){closed++;return {};}};}
            Array.prototype.values=custom;Array.prototype[Symbol.iterator]=custom;
            assert(pair([2,3])===8&&closed===3);
            Array.prototype.values=originalValues;Array.prototype[Symbol.iterator]=originalIterator;
            var getters=0,a=[2,3];Object.defineProperty(a,'0',{get:function(){getters++;return 5;}});
            assert(pair(a)===8&&getters===1);
            var proxy=new Proxy([2,3],{get:function(t,k){return t[k];}});assert(pair(proxy)===5);
        "#,
            2,
        );
    }

    #[test]
    fn getters_holes_and_empty_close_are_never_suppressed() {
        check(
            r#"
            function empty(a){var []=a;return 1;}
            function pair(a){var [x,y]=a;return x+y;}
            function three(a){var [x,y,z]=a;return z;}
            assert(pair([2,3])===5);
            var proto=Object.getPrototypeOf([].values()),closed=0;
            Object.defineProperty(proto,'return',{get:function(){closed++;return undefined;},configurable:true});
            assert(empty([])===1&&closed===1);
            assert(pair([2,3])===5&&closed===2);
            assert(three([2,3])===undefined&&closed===2);
            delete proto.return;
            var oldNext=proto.next,reads=0;
            Object.defineProperty(proto,'next',{get:function(){reads++;return oldNext;},configurable:true});
            assert(empty([])===1&&reads===1);
            Object.defineProperty(proto,'next',{value:oldNext,writable:true,configurable:true});
            var a=[2,3],iterator=Array.prototype[Symbol.iterator];
            Object.defineProperty(a,Symbol.iterator,{get:function(){reads++;return iterator;}});
            assert(pair(a)===5&&reads===2);
            a=[,3];var parent=Object.create(Array.prototype);
            Object.defineProperty(parent,'0',{get:function(){reads++;return 5;}});Object.setPrototypeOf(a,parent);
            assert(pair(a)===8&&reads===3);
        "#,
            2,
        );
    }
    #[test]
    fn foreign_intrinsics_and_invalid_close_keep_original_identity() {
        check(
            r#"
            function pair(a){var [x,y]=a;return x+y;}
            assert(pair([2,3])===5);
            var realm=$262.createRealm(),foreign=realm.global.Array(2,3);
            assert(pair(foreign)===5);
            var localIterator=Array.prototype[Symbol.iterator];
            Array.prototype[Symbol.iterator]=realm.global.Array.prototype[Symbol.iterator];
            assert(pair([2,3])===5);
            Array.prototype[Symbol.iterator]=localIterator;
            $262.gc();assert(pair([2,3])===5);
            var proto=Object.getPrototypeOf([].values()),closed=0;
            proto.return=function(){closed++;return 0;};
            var threw=false;try{pair([2,3]);}catch(e){threw=e.name==='TypeError';}
            assert(threw&&closed===1);delete proto.return;
            "#,
            2,
        );
    }

    #[test]
    fn explicit_undefined_and_skipped_elements_stay_distinct_from_holes() {
        check(
            r#"
            function pair(a){var [x,y]=a;return x===undefined&&y===3;}
            function skip(a){var [x,,z]=a;return x+z;}
            assert(pair([undefined,3]));assert(pair([,3]));
            assert(skip([2,99,3])===5);
            var reads=0,a=[2,99,3];
            Object.defineProperty(a,'1',{get:function(){reads++;return 99;}});
            assert(skip(a)===5&&reads===1);
            "#,
            2,
        );
    }
    #[test]
    fn element_getters_can_change_later_values_and_close_during_gc() {
        check(
            r#"
            var log='',proto=Object.getPrototypeOf([].values());
            function pair(a){var [x,y]=a;log+='body,';$262.gc();return x.v+y.v;}
            assert(pair([{v:2},{v:3}])===5);log='';
            var a=[0,0];Object.defineProperty(a,'0',{get:function(){
                log+='first,';
                Object.defineProperty(a,'1',{get:function(){log+='second,';$262.gc();return {v:7};},configurable:true});
                Object.defineProperty(proto,'return',{get:function(){
                    log+='return-get,';return function(){log+='return-call,';$262.gc();return {};};
                },configurable:true});
                $262.gc();return {v:5};
            }});
            assert(pair(a)===12&&log==='first,second,return-get,return-call,body,');
            delete proto.return;log='';assert(pair([{v:2},{v:3}])===5);
            "#,
            2,
        );
    }
}
