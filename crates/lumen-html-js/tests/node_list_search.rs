use lumen::embed::Value;

#[test]
fn static_node_list_index_of_preserves_length_prototype_identity_and_coercion() {
    std::thread::Builder::new().stack_size(64 * 1024 * 1024).spawn(|| {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        runtime.set_deadline(std::time::Duration::from_secs(10));
        let engine = runtime.engine();
        let _realm = lumen_html_js::install(engine.ctx(), "<!doctype html><body><div><span></span><span></span><span></span></div></body>", 1024).unwrap();
        let result = engine.eval_value(r#"(() => {
            const check = (ok, message) => { if (!ok) throw new Error(message); };
            const root = document.querySelector('div'), list = root.querySelectorAll('span');
            const target = list[1], indexOf = Array.prototype.indexOf;
            check(indexOf.call(list, target) === 1 && indexOf.call(list, target, 2) === -1, 'native indices and fromIndex');
            check(indexOf.call(list, target, -2) === 1 && indexOf.call(list, target, -1) === -1, 'negative fromIndex');
            check(indexOf.call(list, target, Infinity) === -1 && indexOf.call(list, target, NaN) === 1, 'infinite and NaN fromIndex');
            check(indexOf.call(list) === -1 && indexOf.call(list, undefined) === -1, 'absent target is not a supported node');
            const foreign = document.implementation.createHTMLDocument('').createElement('span');
            check(indexOf.call(list, foreign) === -1 && indexOf.call(list, new Proxy(target, {})) === -1, 'foreign native identities and author node proxy');
            let count = 0;
            Object.defineProperty(list, 'length', {configurable:true, get(){ ++count; return 1; }});
            check(indexOf.call(list, target) === -1 && count === 1, 'shorter own length read once');
            let converted = false;
            Object.defineProperty(list, 'length', {configurable:true, get(){ return 0; }});
            check(indexOf.call(list, target, {valueOf(){ converted=true; throw 1; }}) === -1 && !converted, 'empty length precedes fromIndex coercion');
            delete list.length;
            const nativePrototype = Object.getPrototypeOf(list), originalLength = Object.getOwnPropertyDescriptor(NodeList.prototype, 'length');
            Object.defineProperty(NodeList.prototype, 'length', {configurable:true, get(){ return 1; }});
            check(indexOf.call(list, target) === -1, 'tampered NodeList prototype length');
            Object.defineProperty(NodeList.prototype, 'length', originalLength);
            Object.setPrototypeOf(list, {get length(){ return 1; }});
            check(indexOf.call(list, target) === -1 && list[1] === target, 'replaced prototype preserves supported native indices');
            Object.setPrototypeOf(list, nativePrototype);
            let extraGets = 0;
            const extra = document.createElement('p');
            Object.defineProperty(NodeList.prototype, '3', {configurable:true, get(){ ++extraGets; return extra; }});
            Object.defineProperty(list, 'length', {configurable:true, get(){ return 4; }});
            check(indexOf.call(list, extra) === 3 && extraGets === 1, 'longer length falls back to inherited numeric getter');
            Object.defineProperty(NodeList.prototype, '3', {configurable:true, get(){ return undefined; }});
            check(indexOf.call(list) === 3, 'absent search argument matches inherited undefined value');
            const failure = {};
            Object.defineProperty(NodeList.prototype, '3', {configurable:true, get(){ throw failure; }});
            let caught;
            try { indexOf.call(list, extra); } catch (error) { caught = error; }
            check(caught === failure, 'inherited getter exception');
            delete NodeList.prototype[3]; delete list.length;
            const order = [];
            Object.defineProperty(list, 'length', {configurable:true, get(){ order.push('length'); return 3; }});
            check(indexOf.call(list, target, {valueOf(){ order.push('from'); return 1; }}) === 1 && order.join(',') === 'length,from', 'coercion ordering');
            caught = undefined;
            try { indexOf.call(list, target, {valueOf(){ throw failure; }}); } catch (error) { caught = error; }
            check(caught === failure, 'fromIndex exception');
            Object.defineProperty(list, 'length', {configurable:true, get(){ throw failure; }});
            caught = undefined; converted = false;
            try { indexOf.call(list, target, {valueOf(){ converted=true; return 0; }}); } catch (error) { caught = error; }
            check(caught === failure && !converted, 'length exception precedes fromIndex');
            delete list.length;
            const proxyOrder = [];
            const proxy = new Proxy(list, {
                get(object, key){ proxyOrder.push('get:' + key); return key === 'length' ? 3 : object[key]; },
                has(object, key){ proxyOrder.push('has:' + key); return key in object; }
            });
            check(indexOf.call(proxy, target) === 1 && proxyOrder.join(',') === 'get:length,has:0,get:0,has:1,get:1', 'author Proxy has/get traps remain observable');
            const live = root.childNodes;
            root.removeChild(target);
            check(indexOf.call(live, target) === -1 && indexOf.call(list, target) === 1 && list.length === 3, 'live collection fallback and static retained identity and length');
            const end = document.createElement('span'); root.appendChild(end);
            check(indexOf.call(live, end) === 2, 'live collection updates');
            const adopted = document.implementation.createHTMLDocument('');
            adopted.body.appendChild(adopted.adoptNode(target));
            check(indexOf.call(list, target) === 1 && list.length === 3, 'adopted retained node identity and snapshot length');
            return true;
        })()"#).unwrap();
        let result = match result {
            Ok(value) => value,
            Err(error) => {
                let detail = engine.ctx().get_member(&error, "stack").ok().and_then(|value| match value { Value::Str(text) => Some(text.as_str().to_owned()), _ => None });
                panic!("native indexed search contract threw: {detail:?}");
            }
        };
        assert!(matches!(result, Value::Bool(true)));
    }).unwrap().join().unwrap();
}
