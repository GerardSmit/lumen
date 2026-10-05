use lumen::embed::Value;

#[test]
fn xml_prefixed_template_fragment_contents_are_real_and_namespace_aware() {
    let mut runtime = lumen_runtime::Runtime::new();
    let engine = runtime.engine();
    let _realm = lumen_html_js::install_xml(
        engine.ctx(),
        "<svg xmlns='http://www.w3.org/2000/svg' xmlns:h='http://www.w3.org/1999/xhtml'><h:div id='host'/></svg>",
        128,
        lumen_html_js::XmlDocumentType::Svg,
    ).unwrap();
    let result = engine.eval_value(r#"(() => {
        const host = document.getElementById('host');
        host.innerHTML = "<e><h:template xmlns=''><g/></h:template><h/></e>";
        const template = host.firstChild.firstChild;
        const content = template.content;
        if (!(template instanceof HTMLTemplateElement) || !(content instanceof DocumentFragment)
            || content !== template.content || content.parentNode !== null
            || template.firstChild !== null || content.firstChild.namespaceURI !== null
            || host.firstChild.lastChild.namespaceURI !== 'http://www.w3.org/2000/svg') return false;
        const copy = template.cloneNode(true);
        if (copy.content === content || copy.firstChild !== null
            || copy.content.firstChild.namespaceURI !== null) return false;
        template.innerHTML = '<next/>';
        return template.content === content && content.firstChild.localName === 'next'
            && content.firstChild.namespaceURI === null && template.firstChild === null;
    })()"#).unwrap().ok().unwrap();
    assert!(matches!(result, Value::Bool(true)));
}

#[test]
fn adjacent_node_insertion_preserves_identity_adoption_and_literal_text() {
    let mut runtime = lumen_runtime::Runtime::new();
    let engine = runtime.engine();
    let realm = lumen_html_js::install(
        engine.ctx(),
        "<!doctype html><main><div id=host><span id=seed></span></div><textarea></textarea><template></template></main>",
        256,
    )
    .unwrap();
    let result = engine.eval_value(r#"(() => {
        const check = (ok, label) => { if (!ok) throw new Error(label); };
        const host = document.getElementById('host');
        const positions = [['beforebegin','previousSibling'],['afterend','nextSibling'],
            ['afterbegin','firstChild'],['beforeend','lastChild']];
        for (const [position, property] of positions) {
            const child = document.createElement('b');
            child.id = position;
            check(host.insertAdjacentElement(position.toUpperCase(), child) === child &&
                host[property] === child, 'element identity and position ' + position);
        }
        host.insertAdjacentText('beforeend', '<em>literal</em>');
        check(host.lastChild.nodeType === Node.TEXT_NODE &&
            host.lastChild.data === '<em>literal</em>', 'text remains literal');
        const textarea = document.querySelector('textarea');
        textarea.insertAdjacentText('afterbegin', 'a&b');
        check(textarea.value === 'a&b', 'textarea value follows literal insertion');
        const template = document.querySelector('template');
        template.insertAdjacentText('beforeend', 'ordinary child');
        check(template.firstChild.data === 'ordinary child' && template.content.firstChild === null,
            'template ordinary child insertion');
        const detached = document.createElement('div');
        const orphan = document.createElement('b');
        check(detached.insertAdjacentElement('beforebegin', orphan) === null &&
            orphan.parentNode === null, 'detached outside element insertion');
        detached.insertAdjacentText('afterend', 'unused');
        check(detached.firstChild === null, 'detached outside text insertion');
        const donor = document.implementation.createHTMLDocument();
        const foreign = donor.createElement('b');
        donor.body.appendChild(foreign);
        let events = 0;
        foreign.addEventListener('adopted-probe', () => events++);
        check(host.insertAdjacentElement('beforeend', foreign) === foreign &&
            foreign.ownerDocument === document && donor.body.firstChild === null,
            'foreign element adoption preserves wrapper identity');
        foreign.dispatchEvent(new Event('adopted-probe'));
        check(events === 1, 'adoption preserves listener state');
        const root = document.documentElement;
        let hierarchy = '';
        try { host.insertAdjacentElement('beforeend', root); } catch (error) { hierarchy = error.name; }
        check(hierarchy === 'HierarchyRequestError' && root.parentNode === document,
            'ancestor insertion failure preserves the source tree');
        const OriginalDOMException = DOMException;
        globalThis.DOMException = function() { throw new Error('author DOMException constructor'); };
        for (const method of ['insertAdjacentHTML','insertAdjacentElement','insertAdjacentText']) {
            let syntax = null;
            try { host[method]('invalid', method === 'insertAdjacentElement' ? orphan : 'text'); }
            catch (error) { syntax = error; }
            check(syntax instanceof OriginalDOMException && syntax.name === 'SyntaxError' &&
                syntax.code === 12, 'DOMException for invalid position ' + method);
        }
        const xml = document.implementation.createDocument('urn:root', 'root');
        for (const operation of [
            () => { xml.documentElement.innerHTML = '<unclosed>'; },
            () => { xml.documentElement.insertAdjacentHTML('beforeend', '<unclosed>'); }
        ]) {
            let syntax = null;
            try { operation(); } catch (error) { syntax = error; }
            check(syntax instanceof OriginalDOMException && syntax.name === 'SyntaxError' &&
                syntax.code === 12, 'XML markup parse DOMException');
        }
        globalThis.DOMException = OriginalDOMException;
        for (const invalid of [null, document.createTextNode('text')]) {
            let brand = '';
            try { host.insertAdjacentElement('beforeend', invalid); } catch (error) { brand = error.name; }
            check(brand === 'TypeError', 'element argument brand');
        }
        return true;
    })()"#).unwrap();
    match result {
        Ok(Value::Bool(true)) => {}
        Err(error) => {
            let message = engine
                .ctx()
                .coerce_string(&error)
                .map(|message| message.to_string())
                .unwrap_or_else(|_| "unknown exception".into());
            panic!("adjacent node regression threw: {message}");
        }
        _ => panic!("adjacent node regression returned an unexpected value"),
    }
    let initial = realm.with_session(|session| session.document().node_count());
    let result = engine.eval_value(r#"(() => {
        for (let i = 0; i < 100; i++) {
            let name = '';
            try { document.documentElement.insertAdjacentText('beforebegin', 'forbidden'); }
            catch (error) { name = error.name; }
            if (name !== 'HierarchyRequestError') throw new Error('Document text insertion must fail');
        }
        return true;
    })()"#).unwrap();
    assert!(matches!(result, Ok(Value::Bool(true))));
    assert_eq!(
        realm.with_session(|session| session.document().node_count()),
        initial
    );
}

#[test]
fn adjacent_markup_reclaims_failed_fragment_allocations() {
    let mut engine = lumen::Engine::new();
    let realm =
        lumen_html_js::install(engine.ctx(), "<!doctype html><main id=host></main>", 24).unwrap();
    let initial = realm.with_session(|session| session.document().node_count());
    let result = engine
        .eval_value(
            r#"(() => {
                const host = document.getElementById('host');
                const oversized = '<b>x</b>'.repeat(24);
                for (let i = 0; i < 100; i++) {
                    let name = '';
                    try { host.insertAdjacentHTML('beforeend', oversized); }
                    catch (error) { name = error.name; }
                    if (name !== 'QuotaExceededError' || host.firstChild !== null)
                        throw new Error('failed fragment must release its nodes and preserve the target');
                }
                return true;
            })()"#,
        )
        .unwrap();
    match result {
        Ok(Value::Bool(true)) => {}
        Err(error) => {
            let message = engine
                .ctx()
                .coerce_string(&error)
                .map(|message| message.to_string())
                .unwrap_or_else(|_| "unknown exception".into());
            panic!("fragment allocation regression threw: {message}");
        }
        _ => panic!("fragment allocation regression returned an unexpected value"),
    }
    assert_eq!(
        realm.with_session(|session| session.document().node_count()),
        initial
    );
    let result = engine
        .eval_value("document.getElementById('host').insertAdjacentHTML('beforeend', '<b>x</b>')")
        .unwrap();
    assert!(result.is_ok(), "released capacity must be reusable");
    assert_eq!(
        realm.with_session(|session| session.document().node_count()),
        initial + 2
    );
}

#[test]
fn adjacent_markup_uses_shared_context_parser_and_preserves_existing_nodes() {
    let mut engine = lumen::Engine::new();
    lumen_html_js::install(engine.ctx(),
        "<!doctype html><main><div id='host'><span id='seed'>seed</span></div><table></table><svg></svg><template></template><textarea></textarea></main>",
        256).unwrap();
    let result = engine.eval_value(r#"(() => {
        const check = (ok, name) => { if (!ok) throw new Error(name); };
        const host = document.getElementById('host');
        const seed = document.getElementById('seed');
        host.insertAdjacentHTML('BeFoReBeGiN', '<p id="before">before</p>');
        host.insertAdjacentHTML('afterend', '<p id="after">after</p>');
        host.insertAdjacentHTML('afterbegin', '<b id="first">first</b>');
        host.insertAdjacentHTML('beforeend', '<b id="last">last</b>');
        check(host.previousSibling.id === 'before' && host.nextSibling.id === 'after', 'outside positions');
        check(host.firstChild.id === 'first' && host.lastChild.id === 'last', 'inside positions');
        check(document.getElementById('seed') === seed && seed.parentNode === host, 'existing identity');
        const table = document.querySelector('table');
        table.insertAdjacentHTML('beforeend', '<tr><td>cell</td></tr>');
        check(table.firstChild.localName === 'tbody' && table.firstChild.firstChild.firstChild.textContent === 'cell', 'table context');
        const htmlNamespace = 'http://www.w3.org/1999/xhtml';
        const prefixedTable = document.createElementNS(htmlNamespace, 'p:table');
        prefixedTable.insertAdjacentHTML('beforeend', '<tr><td>prefixed cell</td></tr>');
        check(prefixedTable.firstChild.localName === 'tbody' &&
            prefixedTable.firstChild.firstChild.firstChild.textContent === 'prefixed cell',
            'prefixed table uses local-name context');
        const prefixedArea = document.createElementNS(htmlNamespace, 'p:textarea');
        prefixedArea.insertAdjacentHTML('beforeend', '<b>&amp;</b>');
        check(prefixedArea.firstChild.nodeType === Node.TEXT_NODE &&
            prefixedArea.textContent === '<b>&</b>', 'prefixed textarea uses raw-text context');
        const prefixedHtml = document.createElementNS(htmlNamespace, 'p:html');
        const marker = document.createElement('i');
        prefixedHtml.appendChild(marker);
        marker.insertAdjacentHTML('beforebegin', '<tr><td>body context</td></tr>');
        check(prefixedHtml.firstChild.nodeType === Node.TEXT_NODE &&
            prefixedHtml.firstChild.data === 'body context', 'prefixed html uses temporary body context');
        const svg = document.querySelector('svg');
        svg.insertAdjacentHTML('beforeend', '<circle/>');
        check(svg.firstChild.namespaceURI === 'http://www.w3.org/2000/svg', 'foreign namespace context');
        const template = document.querySelector('template');
        template.insertAdjacentHTML('beforeend', '<em>ordinary child</em>');
        check(template.firstChild.localName === 'em' && template.content.firstChild === null, 'template is ordinary insertion target');
        const textarea = document.querySelector('textarea');
        textarea.insertAdjacentHTML('beforeend', 'a&amp;b');
        check(textarea.value === 'a&b', 'textarea live default follows insertion');
        globalThis.adjacentScriptRuns = 0;
        host.insertAdjacentHTML('beforeend', '<script>globalThis.adjacentScriptRuns++;</script>');
        check(adjacentScriptRuns === 0, 'parsed scripts remain inert');
        for (const [target, position, expected] of [
            [host, 'invalid', 'SyntaxError'],
            [document.createElement('div'), 'beforebegin', 'NoModificationAllowedError'],
            [document.documentElement, 'afterend', 'NoModificationAllowedError']
        ]) {
            let name = '';
            try { target.insertAdjacentHTML(position, '<i/>'); } catch (error) { name = error.name; }
            check(name === expected, 'position error ' + position);
        }
        const xml = document.implementation.createDocument('urn:root', 'root');
        xml.documentElement.insertAdjacentHTML('beforeend', '<p:child xmlns:p="urn:child"/>');
        check(xml.documentElement.firstChild.namespaceURI === 'urn:child', 'XML parser namespace');
        let malformed = '';
        try { xml.documentElement.insertAdjacentHTML('beforeend', '<broken>'); } catch (error) { malformed = error.name; }
        check(malformed === 'SyntaxError' && xml.documentElement.childNodes.length === 1, 'XML parse failure is atomic');
        xml.documentElement.innerHTML = '<p:replacement xmlns:p="urn:replacement"/>';
        check(xml.documentElement.firstChild.namespaceURI === 'urn:replacement', 'innerHTML reuses XML parser');
        const detachedHtml = document.implementation.createHTMLDocument();
        detachedHtml.head.innerHTML = '<link rel="stylesheet" href="https://example.test/style.css">';
        detachedHtml.body.insertAdjacentHTML('beforeend', '<img src="image.png">');
        check(detachedHtml.head.firstChild.localName === 'link' &&
            detachedHtml.body.firstChild.localName === 'img' &&
            detachedHtml.cloneNode(true).head.firstChild.localName === 'link',
            'created HTML documents and clones preserve their parser kind');
        return true;
    })()"#).unwrap();
    let result = match result {
        Ok(value) => value,
        Err(error) => {
            let message = engine
                .ctx()
                .coerce_string(&error)
                .map(|message| message.to_string())
                .unwrap_or_else(|_| "unknown exception".into());
            panic!("adjacent markup regression threw: {message}");
        }
    };
    assert!(matches!(result, Value::Bool(true)));
}
