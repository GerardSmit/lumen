use lumen::embed::Value;

#[test]
fn html_parsed_colon_names_are_literal_in_html_svg_and_mathml() {
    let mut runtime = lumen_runtime::Runtime::new_browser();
    let engine = runtime.engine();
    let _realm = lumen_html_js::install(engine.ctx(),
        "<main><test:test></test:test><svg><test:test/></svg><math><test:test/></math></main>", 128).unwrap();
    let result = engine.eval_value(r#"(() => {
        const check = (ok, message) => { if (!ok) throw new Error(message); };
        const main = document.querySelector('main');
        const html = main.firstChild, svg = html.nextSibling.firstChild, math = main.lastChild.firstChild;
        for (const node of [html, svg, math]) {
            check(node.localName === 'test:test' && node.prefix === null, 'HTML tokenizer local name');
            const clone = node.cloneNode(true);
            check(clone.localName === 'test:test' && clone.prefix === null && clone.isEqualNode(node), 'parsed clone');
            const prefixed = document.createElementNS(node.namespaceURI, 'test:test');
            check(prefixed.localName === 'test' && prefixed.prefix === 'test' && !node.isEqualNode(prefixed), 'qualified distinction');
        }
        check(html.namespaceURI === 'http://www.w3.org/1999/xhtml' &&
              svg.namespaceURI === 'http://www.w3.org/2000/svg' &&
              math.namespaceURI === 'http://www.w3.org/1998/Math/MathML', 'foreign namespace preserved');
        check(html.isEqualNode(document.createElement('test:test')), 'parser and createElement equality');
        main.innerHTML = '<other:name></other:name>';
        check(main.firstChild.localName === 'other:name' && main.firstChild.prefix === null, 'fragment tokenizer path');
        const xml = new DOMParser().parseFromString('<p:root xmlns:p="urn:p"><p:child/></p:root>', 'text/xml');
        check(xml.documentElement.prefix === 'p' && xml.documentElement.localName === 'root' &&
              xml.documentElement.firstChild.prefix === 'p' && xml.documentElement.firstChild.localName === 'child', 'XML prefix preserved');
        return true;
    })()"#).unwrap().ok().expect("parser name guard threw");
    assert!(matches!(result, Value::Bool(true)));
}

#[test]
fn contextual_fragment_and_setters_use_actual_context_local_name() {
    let mut runtime = lumen_runtime::Runtime::new_browser();
    let engine = runtime.engine();
    let _realm = lumen_html_js::install(engine.ctx(), "<main></main>", 128).unwrap();
    let result = engine.eval_value(r#"(() => {
        const check = (ok, message) => { if (!ok) throw new Error(message); };
        const literal = document.createElement('h:textarea');
        const qualified = document.createElementNS('http://www.w3.org/1999/xhtml', 'h:textarea');
        const range = document.createRange();
        range.selectNodeContents(literal);
        const markup = '<b>&amp;</b>';
        const literalFragment = range.createContextualFragment(markup);
        check(literalFragment.firstChild.localName === 'b' && literalFragment.textContent === '&', 'literal context parses tags');
        range.selectNodeContents(qualified);
        const rawtext = range.createContextualFragment(markup);
        check(rawtext.firstChild.nodeType === Node.TEXT_NODE && rawtext.textContent === '<b>&</b>', 'qualified context RCDATA');
        literal.innerHTML = markup;
        qualified.innerHTML = markup;
        check(literal.firstChild.localName === 'b' && qualified.firstChild.nodeType === Node.TEXT_NODE, 'setter context');
        const literalHtml = document.createElement('h:html');
        range.selectNodeContents(literalHtml);
        const ordinary = range.createContextualFragment('<span>text</span>');
        check(ordinary.childNodes.length === 1 && ordinary.firstChild.localName === 'span', 'literal html has ordinary context');
        return true;
    })()"#).unwrap().ok().expect("fragment context name guard threw");
    assert!(matches!(result, Value::Bool(true)));
}
