//! Array construction from owned builtin values.
use super::Interp;
use crate::value::{Exotic, Object, Property, Props, Value};
use std::sync::OnceLock;

impl Interp {
    pub fn make_array(&self, items: Vec<Value>) -> Value {
        static COMPACT: OnceLock<bool> = OnceLock::new();
        if (1..=32).contains(&items.len())
            && *COMPACT
                .get_or_init(|| std::env::var_os("LUMEN_NO_COMPACT_BUILTIN_ARRAYS").is_none())
        {
            let props = Props::packed_array_from_values(items.into_iter());
            let obj = Object::new_with_parts(Some(self.array_proto.clone()), props, Exotic::Array);
            #[cfg(test)]
            COMPACT_ARRAYS.with(|n| n.set(n.get() + 1));
            return Value::Obj(obj);
        }
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
                    usize::from(enabled && (1..=32).contains(&len))
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
}
