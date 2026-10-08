use lumen::embed::Value;

#[test]
fn element_class_list_put_forwards_preserves_live_target_conversion_and_errors() {
    std::thread::Builder::new().stack_size(64 * 1024 * 1024).spawn(|| {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        runtime.set_deadline(std::time::Duration::from_secs(10));
        let engine = runtime.engine();
        let _realm = lumen_html_js::install(engine.ctx(), "<!doctype html><html class='reftest-wait'><body></body></html>", 1024).unwrap();
        let result = engine.eval_value(r#"(() => {
            'use strict';
            const check = (ok, message) => { if (!ok) throw new Error(message); };
            const root = document.documentElement;
            const list = root.classList;
            const setter = Object.getOwnPropertyDescriptor(Element.prototype, 'classList').set;
            check(typeof setter === 'function' && setter.length === 1, 'native forwarding descriptor');
            const observer = new MutationObserver(() => {});
            observer.observe(root, {attributes:true, attributeOldValue:true});
            document.addEventListener('TestRendered', () => { root.classList = 'tweak'; });
            root.dispatchEvent(new Event('TestRendered', {bubbles:true}));
            check(root.className === 'tweak' && !list.contains('reftest-wait') && list.contains('tweak'), 'upstream marker callback clears wait');
            check(root.classList === list && !Object.hasOwn(root, 'classList'), 'same live token list');
            const changes = observer.takeRecords();
            check(changes.length === 1 && changes[0].attributeName === 'class' && changes[0].oldValue === 'reftest-wait', 'native attribute mutation');
            root.classList = {toString(){ return '  one one\ttwo  '; }};
            check(root.getAttribute('class') === '  one one\ttwo  ' && list.length === 2 && list[1] === 'two', 'conversion delegated to token list setter');
            root.classList = null;
            check(root.className === 'null', 'DOMString null conversion');
            let rejected = false;
            try { root.classList = Symbol('bad'); } catch (error) { rejected = error instanceof TypeError; }
            check(rejected && root.className === 'null', 'conversion failure preserves attribute');
            const target = {}, input = {toString(){ throw new Error('must not coerce'); }};
            let gets = 0, received;
            Object.defineProperty(target, 'value', {set(value){ received = value; }});
            Object.defineProperty(root, 'classList', {configurable:true, get(){ ++gets; return target; }});
            setter.call(root, input);
            check(gets === 1 && received === input && root.className === 'null', 'live overridden getter and unconverted forwarded value');
            const failure = {};
            Object.defineProperty(root, 'classList', {configurable:true, get(){ throw failure; }});
            let caught;
            try { setter.call(root, input); } catch (error) { caught = error; }
            check(caught === failure, 'getter exception identity');
            for (const primitive of [undefined, null, 1, true, 'text', Symbol('target'), 1n]) {
                Object.defineProperty(root, 'classList', {configurable:true, get(){ return primitive; }});
                rejected = false;
                try { setter.call(root, input); } catch (error) { rejected = error instanceof TypeError; }
                check(rejected, 'non-object forwarding target');
            }
            const frozen = Object.freeze({value:'unchanged'});
            Object.defineProperty(root, 'classList', {configurable:true, get(){ return frozen; }});
            setter.call(root, input);
            check(frozen.value === 'unchanged', 'Set with Throw=false in strict caller');
            rejected = false;
            try { frozen.value = 'bad'; } catch (error) { rejected = error instanceof TypeError; }
            check(rejected, 'caller strict policy restored after rejected write');
            const throwingTarget = {set value(value){ throw failure; }};
            Object.defineProperty(root, 'classList', {configurable:true, get(){ return throwingTarget; }});
            caught = undefined;
            try { setter.call(root, input); } catch (error) { caught = error; }
            check(caught === failure, 'forwarded setter exception identity');
            rejected = false;
            try { frozen.value = 'bad'; } catch (error) { rejected = error instanceof TypeError; }
            check(rejected, 'caller strict policy restored after thrown setter');
            rejected = false;
            try { setter.call({}, 'bad'); } catch (error) { rejected = error instanceof TypeError; }
            check(rejected, 'Element receiver guard');
            delete root.classList;
            root.classList = 'restored';
            check(root.classList === list && list.contains('restored'), 'cached list resumes live behavior');
            return true;
        })()"#).unwrap();
        let result = match result {
            Ok(value) => value,
            Err(error) => {
                let detail = engine.ctx().get_member(&error, "stack").ok().and_then(|value| match value { Value::Str(text) => Some(text.as_str().to_owned()), _ => None });
                panic!("classList forwarding contract threw: {detail:?}");
            }
        };
        assert!(matches!(result, Value::Bool(true)));
    }).unwrap().join().unwrap();
}

#[test]
fn document_head_and_body_use_actual_html_namespaces_roots_and_tree_order() {
    let mut runtime = lumen_runtime::Runtime::new_browser();
    let engine = runtime.engine();
    let _realm = lumen_html_js::install(engine.ctx(), "<!doctype html><html><head></head><body></body></html>", 1024).unwrap();
    let result = engine.eval_value(r#"(() => {
        const check = (ok, message) => {if (!ok) throw new Error(message);};
        const ns = 'http://www.w3.org/1999/xhtml';
        const parser = new DOMParser();
        const xml = parser.parseFromString('<h:html xmlns:h="' + ns + '"><head xmlns="urn:foreign"/>' +
            '<h:head/><body xmlns="urn:foreign"/><h:frameset/><h:body/></h:html>', 'application/xhtml+xml');
        const root = xml.documentElement;
        const head = root.childNodes[1], frameset = root.childNodes[3], body = root.childNodes[4];
        check(xml.head === head && xml.head === xml.head && xml.body === frameset, 'qualified names and first eligible body');
        root.insertBefore(body, frameset);
        check(xml.body === body, 'live document order');
        root.removeChild(head);
        check(xml.head === null, 'removed head');
        const clone = xml.cloneNode(true);
        check(clone.body.localName === 'body' && clone.body !== body && clone.body.ownerDocument === clone, 'document clone queries');
        const other = document.implementation.createDocument('urn:foreign', 'html', null);
        const fake = other.createElementNS(ns, 'head'); other.documentElement.appendChild(fake);
        check(other.head === null && other.body === null, 'foreign root cannot supply HTML document children');
        const literal = document.implementation.createDocument(null, '', null);
        const literalRoot = document.createElement('h:html');
        literal.adoptNode(literalRoot); literal.appendChild(literalRoot);
        literalRoot.appendChild(literal.createElementNS(ns, 'head'));
        check(literal.head === null && literal.body === null, 'literal-colon root is not html');
        const imported = other.importNode(root, true);
        other.replaceChild(imported, other.documentElement);
        check(other.body === imported.childNodes[2] && other.body.localName === 'body', 'root replacement and imported namespace ownership');
        const template = document.createElement('template');
        const inert = template.content.ownerDocument;
        check(inert !== document && inert.head === null && inert.body === null, 'inert owner root never queries main document');
        let rejected = false;
        const getter = Object.getOwnPropertyDescriptor(Document.prototype, 'head').get;
        try {getter.call(document.body);} catch (error) {rejected = error instanceof TypeError;}
        check(rejected, 'native Document receiver');
        return true;
    })()"#).unwrap();
    let result = match result {
        Ok(value) => value,
        Err(error) => {
            let detail = engine.ctx().get_member(&error, "stack").ok().and_then(|value| match value { Value::Str(text) => Some(text.as_str().to_owned()), _ => None });
            panic!("document namespace query guard threw: {detail:?}");
        }
    };
    assert!(matches!(result, Value::Bool(true)));
}
