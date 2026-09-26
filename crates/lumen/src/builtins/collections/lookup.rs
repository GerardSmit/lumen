//! Borrowed collection reads behind the native Map/Set lookup methods.
use crate::builtins::collection_data::{CollectionData, CollectionKind};
use crate::interpreter::Interp;
use crate::value::Gc;
use crate::value::Value;

pub(crate) const MAP_GET: u8 = 13;
pub(crate) const MAP_HAS: u8 = 14;
pub(crate) const SET_HAS: u8 = 15;

fn data<'a>(
    i: &'a Interp,
    this: &Value,
    kind: CollectionKind,
) -> Result<&'a CollectionData, Value> {
    let err = || i.make_error("TypeError", "method called on an incompatible receiver");
    let object = this.as_obj().ok_or_else(err)?;
    let data = i
        .map_data
        .get(&(Gc::as_ptr(object) as usize))
        .ok_or_else(err)?;
    if data.kind() != kind {
        return Err(err());
    }
    Ok(data)
}

fn read(i: &Interp, this: &Value, key: &Value, id: u8) -> Result<Value, Value> {
    let entries = data(
        i,
        this,
        if id == SET_HAS {
            CollectionKind::Set
        } else {
            CollectionKind::Map
        },
    )?;
    Ok(if id == MAP_GET {
        entries.lookup(key).cloned().unwrap_or(Value::Undefined)
    } else {
        Value::Bool(entries.contains(key))
    })
}

pub(super) fn map_get(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    read(i, &this, args.first().unwrap_or(&Value::Undefined), MAP_GET)
}

pub(super) fn map_has(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    read(i, &this, args.first().unwrap_or(&Value::Undefined), MAP_HAS)
}

pub(super) fn set_has(i: &mut Interp, this: Value, args: &[Value]) -> Result<Value, Value> {
    read(i, &this, args.first().unwrap_or(&Value::Undefined), SET_HAS)
}

/// `this.<method>(args)` for the Map/Set natives `get`, `has`, `set`, `add` when `nat` is one
/// of them and `this` is a collection of its kind: the result, computed without the native
/// call's bookkeeping (none of them runs JS or throws then). `None` when that doesn't apply.
pub(crate) fn coll_fast(
    i: &mut Interp,
    nat: crate::value::NativeFn,
    this: &Value,
    args: &[Value],
) -> Option<Value> {
    let key = args.first().unwrap_or(&Value::Undefined);
    let object = this.as_obj()?;
    let data = i.map_data.get_mut(&(Gc::as_ptr(object) as usize))?;
    let n = nat as usize;
    let is = |f: crate::value::NativeFn| n == f as usize;
    match data.kind() {
        CollectionKind::Map if is(map_get) => Some(data.lookup(key).cloned().unwrap_or(Value::Undefined)),
        CollectionKind::Map if is(map_has) => Some(Value::Bool(data.contains(key))),
        CollectionKind::Set if is(set_has) => Some(Value::Bool(data.contains(key))),
        CollectionKind::Map if is(super::insert::map_set) => {
            data.insert(key.clone(), args.get(1).cloned().unwrap_or(Value::Undefined));
            Some(this.clone())
        }
        CollectionKind::Set if is(super::insert::set_add) => {
            let key = super::canonicalize_map_key(key.clone());
            data.insert(key.clone(), key);
            Some(this.clone())
        }
        _ => None,
    }
}

/// Whether `name` is a Map/Set method [`coll_fast`] may handle (a planning hint).
pub(crate) fn coll_fast_name(name: &str) -> bool {
    matches!(name, "get" | "has" | "set" | "add")
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    fn check(source: &str) {
        for tier in [Tier::Interp, Tier::Bytecode] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            let script = format!("function assert(x) {{ if(!x) throw new Error('assertion'); }} function drive() {{ {source} }} drive(); 'passed'");
            let result = engine.eval(&script, false).unwrap();
            match result {
                Completion::Value(v) => assert_eq!(v, "passed", "{tier:?}"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
        }
    }

    #[test]
    fn warmed_reads_preserve_keys_aliasing_and_live_mutations() {
        check(
            r#"
            const keys=[undefined,null,true,false,0,-0,NaN,Infinity,-2,1.5,1n,'key',Symbol('key'),{}];
            const m=new Map(), s=new Set();
            function get(c,k) {return c.get(k);}
            function has(c,k) {return c.has(k);}
            for(let i=0;i<1000;i++) {
                const k=keys[i%keys.length];
                m.set(k,m); s.add(k);
                assert(get(m,k)===m && has(m,k) && has(s,k));
                m.set(k,k); assert(Object.is(get(m,k),k));
                m.delete(k); s.delete(k);
                assert(get(m,k)===undefined && !has(m,k) && !has(s,k));
            }
            m.set(undefined,17); assert(m.get()===17 && m.has());
            m.set('x',m); m.clear(); assert(get(m,'x')===undefined);
            assert(get(new Map([[m,m]]),m)===m);
        "#,
        );
    }

    #[test]
    fn warmed_calls_observe_method_replacement_and_receiver_brands() {
        check(
            r#"
            const m=new Map([[1,2]]),s=new Set([1]);
            function get(c,k) {return c.get(k);}
            function has(c,k) {return c.has(k);}
            function throws(f) {let yes=false;try {f();}catch(e){yes=e instanceof TypeError;}assert(yes);}
            for(let i=0;i<1000;i++) assert(get(m,1)===2 && has(m,1) && has(s,1));
            m.get=function(k){return k+8;}; assert(get(m,1)===9); delete m.get;
            m.has=function(){return false;}; assert(!has(m,1)); delete m.has;
            s.has=function(){return false;}; assert(!has(s,1)); delete s.has;
            assert(get(m,1)===2 && has(m,1) && has(s,1));
            throws(()=>get(new Proxy(m,{}),1));
            throws(()=>get({get:Map.prototype.get},1));
            throws(()=>has({has:Set.prototype.has},1));
            s.get=Map.prototype.get; throws(()=>get(s,1));
            m.has=Set.prototype.has; throws(()=>has(m,1)); delete m.has;
            const weak=new WeakMap();weak.get=Map.prototype.get;throws(()=>get(weak,1));
        "#,
        );
    }

    #[test]
    fn foreign_methods_keep_their_error_realm_after_warmup() {
        check(
            r#"
            const m=new Map([[1,2]]);
            function get(c,k){return c.get(k);}
            for(let i=0;i<1000;i++) assert(get(m,1)===2);
            const realm=$262.createRealm();
            m.get=realm.global.Map.prototype.get;
            for(let i=0;i<300;i++) assert(get(m,1)===2);
            let threw=false;
            try {get({get:m.get},1);}catch(e){threw=e instanceof realm.global.TypeError;}
            assert(threw);
            delete m.get; assert(get(m,1)===2);
        "#,
        );
    }

    #[test]
    fn ordinary_properties_do_not_change_the_brand_after_warmup() {
        check(
            r#"
            const m=new Map([[1,2]]);
            function get(c,k){return c.get(k);}
            for(let i=0;i<1000;i++) assert(get(m,1)===2);
            m.__ck='Set';
            assert(get(m,1)===2);
            m.__ck='Map';assert(get(m,1)===2);
        "#,
        );
    }
}
