use lumen_runtime::{Completion, Runtime};

fn run(source: &str) -> String {
    match Runtime::new().eval(source).expect("parse script") {
        Completion::Value(value) => value,
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

#[test]
fn static_url_parse_returns_live_url_or_null() {
    assert_eq!(
        run(r#"
        const u=URL.parse('../x?name=old#mark','https://example.com/root/a');
        u.searchParams.set('name','new');
        class Sub extends URL {}
        const detached=URL.parse;
        JSON.stringify([
            require('node:url').URL===URL, URL.parse.length,
            Object.getOwnPropertyDescriptor(URL,'parse').enumerable,
            u instanceof URL, u.href, u.origin,
            URL.parse('invalid')===null,
            URL.parse('https://example.com','invalid')===null,
            detached('https://example.com') instanceof URL,
            Sub.parse('https://example.com') instanceof Sub
        ]);
    "#),
        r#"[true,1,true,true,"https://example.com/x?name=new#mark","https://example.com",true,true,true,false]"#
    );
}

#[test]
fn static_url_parse_propagates_coercion_errors_in_argument_order() {
    assert_eq!(
        run(r#"
        const order=[];
        const u=URL.parse({toString(){order.push('input');return '/x'}},{toString(){order.push('base');return 'https://example.com'}});
        const errors=[];
        for (const v of [Symbol('bad'),{toString(){throw new Error('coercion')}}]) {
            try { URL.parse(v); errors.push('unexpected'); }
            catch(e) { errors.push(e.name+':'+(e.message==='coercion')); }
        }
        try { URL.parse(); } catch(e) { errors.push(e.name); }
        JSON.stringify([u.href,order,errors]);
    "#),
        r#"["https://example.com/x",["input","base"],["TypeError:false","Error:true","TypeError"]]"#
    );
}
