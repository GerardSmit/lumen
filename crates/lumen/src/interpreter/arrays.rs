//! Array construction from owned builtin values.
use super::Interp;
use crate::value::{Exotic, Object, Property, Value};
use std::sync::OnceLock;

impl Interp {
    pub fn make_array(&self, items: Vec<Value>) -> Value {
        if !compact_arrays() && !items.is_empty() {
            return self.make_array_classic(items);
        }
        if !items.is_empty() {
            note_compact();
        }
        Value::Obj(Object::new_array_from_vec(Some(self.array_proto.clone()), items))
    }

    /// [`Interp::make_array`] from an exact-size run of owned values (an array literal's operand
    /// stack slice): packed elements, the array one heap block when small.
    pub(crate) fn make_array_iter(&self, items: impl ExactSizeIterator<Item = Value>) -> Value {
        let len = items.len();
        if len != 0 && !compact_arrays() {
            return self.make_array_classic(items.collect());
        }
        if len != 0 {
            note_compact();
        }
        Value::Obj(Object::new_array_from_iter(Some(self.array_proto.clone()), items))
    }

    /// [`Interp::make_array_iter`] of the `n` values at `vals`, *moved* out (the caller must
    /// not drop them again): the JIT's array literals (one slab box up to ten elements, see
    /// [`Object::alloc_array`]).
    ///
    /// # Safety
    /// `vals` must point at `n` initialized values.
    pub(crate) unsafe fn make_array_moved(&self, vals: *mut Value, n: usize) -> Value {
        if n != 0 && !compact_arrays() {
            return self.make_array_classic((0..n).map(|k| std::ptr::read(vals.add(k))).collect());
        }
        if n != 0 {
            note_compact();
        }
        Value::Obj(Object::alloc_array(Some(self.array_proto.clone()), vals, n))
    }

    /// The `RegExp.prototype.exec` match array: packed captures and the `index` / `input` /
    /// `groups` data properties, built in their final shape (no per-property transitions).
    pub(crate) fn make_exec_result(
        &self,
        items: impl ExactSizeIterator<Item = Value>,
        index: Value,
        input: Value,
        groups: Value,
    ) -> Value {
        if !compact_arrays() {
            let arr = self.make_array_classic(items.collect());
            if let Value::Obj(o) = &arr {
                crate::value::set_data(o, "index", index);
                crate::value::set_data(o, "input", input);
                crate::value::set_data(o, "groups", groups);
            }
            return arr;
        }
        note_compact();
        Value::Obj(Object::new_exec_result(
            Some(self.array_proto.clone()),
            items,
            index,
            input,
            groups,
        ))
    }

    /// The keyed element map (`LUMEN_NO_COMPACT_BUILTIN_ARRAYS=1`, for A/B comparison).
    fn make_array_classic(&self, items: Vec<Value>) -> Value {
        let obj = Object::new(Some(self.array_proto.clone()));
        let len = items.len();
        let numeric = items.iter().all(|v| matches!(v, Value::Num(_)));
        {
            let mut b = obj.borrow_mut();
            b.props.mark_array();
            b.props.reserve_dense_exact(len, numeric);
        }
        obj.borrow_mut().exotic = Exotic::Array;
        {
            let mut b = obj.borrow_mut();
            // `length` first: the named prefix of `entries` precedes the elements.
            b.props.insert(
                "length",
                Property::data(Value::Num(len as f64), true, false, false),
            );
            for v in items {
                b.props.push_dense(Property::plain(v));
            }
        }
        Value::Obj(obj)
    }
}

fn compact_arrays() -> bool {
    static COMPACT: OnceLock<bool> = OnceLock::new();
    *COMPACT.get_or_init(|| std::env::var_os("LUMEN_NO_COMPACT_BUILTIN_ARRAYS").is_none())
}

#[inline(always)]
fn note_compact() {
    #[cfg(test)]
    COMPACT_ARRAYS.with(|n| n.set(n.get() + 1));
}

#[cfg(test)]
thread_local! {
    static COMPACT_ARRAYS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, value::Value, Completion, Engine};

    fn eval(engine: &mut Engine, source: &str) {
        match engine.eval(source, false).unwrap() {
            Completion::Value(_) => {}
            Completion::Throw { name, message } => panic!("{name}: {message}"),
        }
    }

    #[test]
    fn builtin_array_boundaries_preserve_reflection_growth_and_gc_edges() {
        for tier in [Tier::Interp, Tier::Bytecode] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            eval(
                &mut engine,
                "function assert(v){if(!v)throw new Error('builtin array');}",
            );
            for len in [0, 1, 2, 10, 11, 32, 33] {
                super::COMPACT_ARRAYS.with(|n| n.set(0));
                let value = engine
                    .interp
                    .make_array((0..len).map(|n| Value::Num(n as f64)).collect());
                let enabled = std::env::var_os("LUMEN_NO_COMPACT_BUILTIN_ARRAYS").is_none();
                assert_eq!(
                    super::COMPACT_ARRAYS.with(|n| n.get()),
                    usize::from(enabled && len >= 1)
                );
                crate::value::set_data(&engine.interp.global, "a", value);
                eval(
                    &mut engine,
                    &format!(
                        r#"
                    assert(Array.isArray(a)&&Object.getPrototypeOf(a)===Array.prototype);
                    assert(a.length==={len}&&Object.keys(a).length==={len});
                    var d=Object.getOwnPropertyDescriptor(a,'length');
                    assert(d.value==={len}&&d.writable&&!d.enumerable&&!d.configurable);
                    for(var j=0;j<{len};j++){{
                        var p=Object.getOwnPropertyDescriptor(a,String(j));
                        assert(p.value===j&&p.writable&&p.enumerable&&p.configurable);
                    }}
                    var child={{value:7}};a.push(child);a.note=child;child=null;$262.gc();
                    assert(a[{len}]===a.note&&a.note.value===7);
                    while(a.length<40)a.push(a.note);
                    assert(a[39]===a.note);
                    delete a[0];assert(!Object.prototype.hasOwnProperty.call(a,'0'));
                    var gets=0;Object.defineProperty(a,'0',{{get:function(){{gets++;return a.note;}},configurable:true}});
                    assert(a[0]===a.note&&gets===1);
                    a.length=1;$262.gc();assert(a.length===1&&a.note.value===7);
                "#
                    ),
                );
            }
        }
    }

    #[test]
    fn regexp_capture_arrays_keep_descriptors_indices_and_unicode_values() {
        for tier in [Tier::Interp, Tier::Bytecode] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            super::COMPACT_ARRAYS.with(|n| n.set(0));
            eval(
                &mut engine,
                r#"
                function assert(v){if(!v)throw new Error('capture arrays');}
                function match(){return /(a)(b)?/d.exec('za');}
                for(var k=0;k<80;k++){
                    var m=match();
                    assert(m.length===3&&m[0]==='a'&&m[1]==='a'&&m[2]===undefined);
                    assert(m.index===1&&m.input==='za'&&m.groups===undefined);
                    assert(Object.keys(m).join(',')==='0,1,2,index,input,groups,indices');
                    assert(m.indices.length===3&&m.indices[0][0]===1&&m.indices[0][1]===2);
                    assert(m.indices[1][0]===1&&m.indices[1][1]===2&&m.indices[2]===undefined);
                    assert(m.indices[0]!==m.indices[1]);
                    m.indices[0].push({keep:9});$262.gc();assert(m.indices[0][2].keep===9);
                    var p=Object.getOwnPropertyDescriptor(m,'indices');
                    assert(p.writable&&p.enumerable&&p.configurable);
                }
                var u=/(.)(.)/du.exec('x😀z');
                assert(u[1]==='x'&&u[2]==='😀'&&u.indices[2][0]===1&&u.indices[2][1]===3);
                var halves=/(.)(.)/d.exec('😀');
                assert(halves[1].charCodeAt(0)===0xd83d&&halves[2].charCodeAt(0)===0xde00);
                var pieces='a,b,c'.split(',');assert(pieces.join('|')==='a|b|c');
                assert(Object.entries({a:1,b:2})[1][1]===2);
            "#,
            );
            let hits = super::COMPACT_ARRAYS.with(|n| n.get());
            if std::env::var_os("LUMEN_NO_COMPACT_BUILTIN_ARRAYS").is_none() {
                assert!(hits >= 320, "missing builtin array coverage: {hits}");
            } else {
                assert_eq!(hits, 0);
            }
        }
    }
    #[test]
    fn in_box_packed_arrays_exec_results_and_appends() {
        for tier in [Tier::Interp, Tier::Bytecode] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            eval(
                &mut engine,
                r#"
                function assert(v,m){if(!v)throw new Error(m);}
                for(var k=0;k<200;k++){
                    var a=[k,{v:k}];a.push(3);assert(a.length===3&&a[1].v===k&&a[2]===3,'push');
                    var b=[1,2,3,4,5,6,7,8,9,10,11,12];b[20]=1;
                    assert(b.length===21&&!(15 in b)&&b[11]===12,'grow');
                    var c=[...b.slice(0,3),...'xy',...[]];assert(c.join()==='1,2,3,x,y','spread');
                    var e=[];e.push(1,2);e.length=0;e.push(9);assert(e[0]===9&&e.length===1,'reuse');
                    var f=[];f[0]='a';f[1]='b';assert(f.join()==='a,b'&&f.length===2,'index fill');
                    var m=/(a)(?<n>b)?/.exec('xab');
                    assert(Object.keys(m).join()==='0,1,2,index,input,groups','exec keys');
                    assert(m.index===1&&m.input==='xab'&&m.groups.n==='b','exec values');
                    m.index=7;delete m.input;m.push(0);
                    assert(m.index===7&&!('input' in m)&&m.length===4,'exec mutable');
                    var d=Object.getOwnPropertyDescriptor(m,'groups');
                    assert(d.writable&&d.enumerable&&d.configurable,'exec descriptor');
                    assert(Array.from([1,2,3]).concat([4],5).slice(1).join()==='2,3,4,5','bulk');
                }
                var o={s:'a\uD83D'};o.s+='\uDE00';assert(o.s==='a\u{1F600}','surrogate join');
            "#,
            );
        }
    }

    #[test]
    fn regexp_literals_die_by_refcount() {
        let mut engine = Engine::new();
        eval(&mut engine, "function f(s){return /a(b)/.test(s);}");
        let before = crate::value::live_objects();
        eval(&mut engine, "for(var k=0;k<20000;k++)f('ab');");
        let grown = crate::value::live_objects() - before;
        assert!(grown < 1000, "regex objects outlived their last reference: {grown}");
        assert!(engine.interp.regexps.len() < 5000, "stale regexps entries were not pruned");
    }
}
