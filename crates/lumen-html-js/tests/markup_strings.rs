use lumen::embed::Value;

#[test]
fn literal_colon_element_names_remain_distinct_across_clone_import_and_adoption() {
    let mut runtime = lumen_runtime::Runtime::new_browser();
    let engine = runtime.engine();
    let _realm = lumen_html_js::install_xml(engine.ctx(),
        "<html xmlns='http://www.w3.org/1999/xhtml'><head/><body/></html>", 128,
        lumen_html_js::XmlDocumentType::Xhtml).unwrap();
    let result = engine.eval_value(r#"(() => {
        const check = (ok, message) => { if (!ok) throw new Error(message); };
        const literal = document.createElement('p:name');
        const prefixed = document.createElementNS('http://www.w3.org/1999/xhtml', 'p:name');
        check(literal.localName === 'p:name' && literal.prefix === null && literal.nodeName === 'p:name', 'literal name');
        check(prefixed.localName === 'name' && prefixed.prefix === 'p', 'qualified name');
        check(!literal.isEqualNode(prefixed), 'prefix/local distinction in equality');
        const clone = literal.cloneNode(true);
        const target = document.implementation.createHTMLDocument();
        const imported = target.importNode(literal, true);
        check(clone.localName === 'p:name' && clone.prefix === null && literal.isEqualNode(imported), 'clone/import metadata');
        target.adoptNode(literal);
        check(literal.ownerDocument === target && literal.localName === 'p:name' && literal.prefix === null &&
              literal.isEqualNode(clone), 'adopted metadata');
        const xml = document.createElement('div');
        xml.appendChild(clone);
        let rejected = false;
        try { xml.innerHTML; } catch (e) { rejected = e instanceof DOMException && e.name === 'InvalidStateError'; }
        check(rejected, 'cloned literal name stays invalid in strict XML');
        xml.replaceChildren(prefixed);
        check(xml.innerHTML === '<p:name xmlns:p="http://www.w3.org/1999/xhtml"></p:name>', 'valid prefix serialization');
        return true;
    })()"#).unwrap().ok().expect("literal colon name guard threw");
    assert!(matches!(result, Value::Bool(true)));
}

#[test]
fn shared_null_string_conversion_preserves_form_and_descriptor_setters() {
    let mut runtime = lumen_runtime::Runtime::new_browser();
    let engine = runtime.engine();
    let _realm = lumen_html_js::install(engine.ctx(),
        "<style>@font-face{font-family:Before;src:url('before.woff')}</style><input><textarea></textarea>", 128).unwrap();
    let result = engine.eval_value(r#"(() => {
        const check = (ok, message) => { if (!ok) throw new Error(message); };
        for (const control of [document.querySelector('input'), document.querySelector('textarea')]) {
            control.value = null;
            check(control.value === '', 'control null');
            control.value = undefined;
            check(control.value === 'undefined', 'control undefined');
        }
        const descriptors = document.querySelector('style').sheet.cssRules[0].style;
        let calls = 0;
        descriptors.fontFamily = { toString() { calls++; return 'After'; } };
        check(calls === 1 && descriptors.fontFamily === 'After', 'descriptor single conversion');
        const marker = new Error('conversion failure'); let caught;
        try { descriptors.fontFamily = { toString() { throw marker; } }; } catch (e) { caught = e; }
        check(caught === marker && descriptors.fontFamily === 'After', 'descriptor conversion atomic');
        descriptors.fontFamily = undefined;
        check(descriptors.fontFamily === 'undefined', 'descriptor undefined');
        descriptors.fontFamily = null;
        check(descriptors.fontFamily === '', 'descriptor null');
        return true;
    })()"#).unwrap().ok().expect("shared string conversion guard threw");
    assert!(matches!(result, Value::Bool(true)));
}

#[test]
fn markup_setters_apply_null_conversion_once_before_atomic_mutation() {
    let mut runtime = lumen_runtime::Runtime::new_browser();
    let engine = runtime.engine();
    let _realm = lumen_html_js::install(engine.ctx(), "<main></main>", 256).unwrap();
    let result = engine.eval_value(r#"(() => {
        const check = (ok, message) => { if (!ok) throw new Error(message); };
        const host = document.querySelector('main');
        host.innerHTML = '<p>old</p>'; host.innerHTML = null;
        check(host.childNodes.length === 0, 'inner null clears');
        host.innerHTML = undefined;
        check(host.textContent === 'undefined', 'inner undefined stringifies');
        host.innerHTML = '<p>old</p>'; host.firstChild.outerHTML = null;
        check(host.childNodes.length === 0, 'outer null removes');
        host.innerHTML = '<p>old</p>'; host.firstChild.outerHTML = undefined;
        check(host.textContent === 'undefined', 'outer undefined stringifies');
        for (const property of ['innerHTML', 'outerHTML']) {
            host.innerHTML = '<p>before</p>';
            const target = property === 'innerHTML' ? host : host.firstChild;
            let calls = 0;
            target[property] = { toString() { calls++; host.setAttribute('conversion', property); return '<b>after</b>'; } };
            check(calls === 1 && host.textContent === 'after' && host.firstChild.localName === 'b', 'once-only reentrant conversion');
            const child = host.firstChild, marker = new Error('conversion failure');
            let caught;
            try { (property === 'innerHTML' ? host : child)[property] = { toString() { throw marker; } }; }
            catch (e) { caught = e; }
            check(caught === marker && host.firstChild === child && host.textContent === 'after', 'failed conversion atomic');
            (property === 'innerHTML' ? host : child)[property] = { toString: undefined, valueOf() { return 42; } };
            check(host.textContent === '42', 'valueOf fallback');
        }
        globalThis.setterScriptRuns = 0;
        host.innerHTML = '<script>setterScriptRuns++</script>';
        host.firstChild.outerHTML = '<script>setterScriptRuns++</script>';
        check(setterScriptRuns === 0, 'setter scripts stay inert');
        return true;
    })()"#).unwrap().ok().expect("markup string guard threw");
    assert!(matches!(result, Value::Bool(true)));
}

#[test]
fn xml_inner_html_uses_strict_namespace_serialization_and_real_exception_branding() {
    let mut runtime = lumen_runtime::Runtime::new_browser();
    let engine = runtime.engine();
    let _realm = lumen_html_js::install_xml(engine.ctx(),
        "<html xmlns='http://www.w3.org/1999/xhtml'><head/><body/></html>", 256,
        lumen_html_js::XmlDocumentType::Xhtml).unwrap();
    let result = engine.eval_value(r#"(() => {
        const check = (ok, message) => { if (!ok) throw new Error(message); };
        const h = 'http://www.w3.org/1999/xhtml';
        const div = document.createElement('div');
        div.appendChild(document.createElement('xmp')).appendChild(document.createElement('span')).textContent = '<';
        check(div.innerHTML === '<xmp xmlns="'+h+'"><span>&lt;</span></xmp>', 'XML rawtext escaping');
        div.innerHTML = '<br/><h:br xmlns:h="'+h+'"/>';
        const markup = div.innerHTML;
        check(markup === '<br xmlns="'+h+'" /><h:br xmlns:h="'+h+'" />', 'sibling namespace scopes');
        const parsed = new DOMParser().parseFromString('<container>'+markup+'</container>', 'text/xml');
        check(parsed.documentElement.firstChild.namespaceURI === h &&
              parsed.documentElement.firstChild.localName === 'br' &&
              parsed.documentElement.lastChild.namespaceURI === h &&
              parsed.documentElement.lastChild.prefix === 'h' &&
              parsed.documentElement.lastChild.localName === 'br', 'namespace roundtrip');
        const nativeException = DOMException;
        globalThis.DOMException = function() { throw new Error('author constructor'); };
        const malformed = document.createElement('test:test');
        div.replaceChildren(malformed);
        let nameError; try { div.innerHTML; } catch (e) { nameError = e; }
        check(nameError instanceof nativeException && nameError.name === 'InvalidStateError' && nameError.code === 11,
              'invalid QName native exception');
        div.textContent = '\f';
        let dataError; try { div.innerHTML; } catch (e) { dataError = e; }
        check(dataError instanceof nativeException && dataError.name === 'InvalidStateError', 'invalid XML character');
        check(new XMLSerializer().serializeToString(div).includes('\f'), 'XMLSerializer stays lenient');
        globalThis.DOMException = nativeException;
        const html = document.implementation.createHTMLDocument();
        html.body.textContent = '\f';
        check(html.body.innerHTML === '\f', 'HTML serialization remains lenient');
        const template = document.createElement('template');
        template.innerHTML = '<span>template</span>';
        check(template.innerHTML === '<span xmlns="'+h+'">template</span>', 'template content serialization');
        const shadow = document.createElement('div').attachShadow({mode:'open'});
        shadow.innerHTML = '<span>shadow</span>';
        check(shadow.innerHTML === '<span xmlns="'+h+'">shadow</span>', 'XML shadow fragment serialization');
        return true;
    })()"#).unwrap().ok().expect("XML innerHTML guard threw");
    assert!(matches!(result, Value::Bool(true)));
}
