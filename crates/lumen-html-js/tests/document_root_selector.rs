use lumen::embed::Value;

fn check(source: &str) {
    let mut runtime = lumen_runtime::Runtime::new_browser();
    let engine = runtime.engine();
    let _realm = lumen_html_js::install(engine.ctx(), "<!doctype html><main></main>", 512).unwrap();
    let result = engine.eval_value(source).unwrap().ok().expect("document root/selector guard threw");
    assert!(matches!(result, Value::Bool(true)));
}

#[test]
fn svg_document_root_tracks_namespace_local_name_replacement_and_shared_wrappers() {
    check(r#"(() => {
        const check = (ok, message) => { if (!ok) throw new Error(message); };
        const ns = 'http://www.w3.org/2000/svg';
        const getter = Object.getOwnPropertyDescriptor(Document.prototype, 'rootElement').get;
        check(document.rootElement === null && !('SVGDocument' in globalThis), 'HTML root and historical interface');
        const empty = document.implementation.createDocument(null, '', null);
        check(empty.rootElement === null, 'empty document');
        for (const mime of ['text/xml', 'image/svg+xml']) {
            const xml = new DOMParser().parseFromString('<p:svg xmlns:p="' + ns + '"/>', mime);
            const root = xml.documentElement;
            check(xml.rootElement === root && getter.call(xml) === root && root.prefix === 'p', 'prefixed root identity');
            const clone = xml.cloneNode(true);
            check(clone.rootElement === clone.documentElement && clone.rootElement !== root && clone.rootElement.isEqualNode(root), 'document clone');
            const target = document.implementation.createDocument(null, '', null);
            target.appendChild(target.importNode(root, true));
            check(target.rootElement === target.documentElement && target.rootElement.isEqualNode(root), 'import root');
            const other = xml.createElementNS(ns, 'g');
            xml.replaceChild(other, root);
            check(xml.rootElement === null, 'non-svg SVG root');
            xml.replaceChild(root, other);
            check(xml.rootElement === root, 'replacement updates getter');
            const adopted = empty.adoptNode(root);
            check(xml.rootElement === null, 'adoption removes source root');
            empty.appendChild(adopted);
            check(empty.rootElement === adopted && adopted.ownerDocument === empty, 'adoption target identity');
            empty.removeChild(adopted);
            check(empty.rootElement === null, 'removed root');
        }
        const created = document.implementation.createDocument(ns, 's:svg', null);
        check(created.rootElement === created.documentElement, 'createDocument SVG');
        const wrongNamespace = document.implementation.createDocument('urn:other', 'svg', null);
        check(wrongNamespace.rootElement === null, 'namespace required');
        const parsed = new DOMParser().parseFromString('<svg><svg:svg></svg:svg></svg>', 'text/html');
        const literal = parsed.querySelector('svg').firstChild;
        check(literal.namespaceURI === ns && literal.localName === 'svg:svg' && literal.prefix === null, 'literal colon fixture');
        empty.appendChild(empty.importNode(literal, true));
        check(empty.rootElement === null, 'literal colon is not svg local name');
        let illegal = false;
        try { getter.call({documentElement: created.documentElement}); } catch (e) { illegal = e instanceof TypeError; }
        check(illegal, 'native document receiver');
        return true;
    })()"#);
}

#[test]
fn svg_parser_scripts_stay_inert_through_root_adoption_import_and_clone() {
    check(r#"(() => {
        const check = (ok, message) => { if (!ok) throw new Error(message); };
        const ns = 'http://www.w3.org/2000/svg';
        globalThis.svgRootRuns = 0;
        for (const operation of ['append', 'import', 'prefixedImport', 'adopt', 'clone']) {
            const prefix = operation === 'prefixedImport' ? 'p:' : '';
            const declaration = prefix ? 'xmlns:p' : 'xmlns';
            const xml = new DOMParser().parseFromString('<' + prefix + 'svg ' + declaration + '="' + ns + '"><' + prefix +
                'script>globalThis.svgRootRuns++;</' + prefix + 'script></' + prefix + 'svg>', 'text/xml');
            const root = xml.rootElement;
            check(root === xml.documentElement && root.firstChild.localName === 'script', operation + ' root');
            let inserted = root;
            if (operation === 'import' || operation === 'prefixedImport') inserted = document.importNode(root, true);
            else if (operation === 'adopt') inserted = document.adoptNode(root);
            else if (operation === 'clone') inserted = root.cloneNode(true);
            document.body.appendChild(inserted);
            check(svgRootRuns === 0, operation + ' parser script must remain inert');
            inserted.remove();
        }
        const active = document.createElementNS(ns, 'svg');
        const script = document.createElementNS(ns, 'script');
        script.textContent = 'globalThis.svgRootRuns++;';
        active.appendChild(script);
        document.body.appendChild(active);
        check(svgRootRuns === 1, 'dynamic SVG script positive control');
        active.remove();
        return true;
    })()"#);
}

#[test]
fn webkit_selector_alias_reuses_namespace_matching_coercion_and_native_errors() {
    check(r#"(() => {
        const check = (ok, message) => { if (!ok) throw new Error(message); };
        for (const ns of ['', 'urn:ns', 'http://www.w3.org/2000/svg']) {
            const element = document.createElementNS(ns, 'h');
            for (const query of ['h', '*|h', 'other', '[missing]']) {
                check(element.webkitMatchesSelector(query) === element.matches(query), 'shared namespace selector result');
            }
        }
        const element = document.createElementNS('urn:ns', 'p:h');
        let calls = 0;
        const query = {toString() { calls++; element.setAttribute('id', 'reentrant'); return '#reentrant'; }};
        check(element.webkitMatchesSelector(query) && calls === 1, 'single coercion before session borrow');
        const sentinel = {};
        let thrown;
        try { element.webkitMatchesSelector({toString() { throw sentinel; }}); } catch (e) { thrown = e; }
        check(thrown === sentinel, 'coercion exception identity');
        let syntax = false, receiver = false;
        try { element.webkitMatchesSelector('['); } catch (e) { syntax = e instanceof DOMException && e.name === 'SyntaxError'; }
        try { Element.prototype.webkitMatchesSelector.call({}, 'h'); } catch (e) { receiver = e instanceof TypeError; }
        check(syntax && receiver, 'shared syntax DOMException and native receiver');
        return true;
    })()"#);
}
