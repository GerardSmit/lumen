use lumen::embed::Value;

fn check(source: &str) {
    let mut runtime = lumen_runtime::Runtime::new_browser();
    let engine = runtime.engine();
    let _realm = lumen_html_js::install(engine.ctx(), "<!doctype html><main></main>", 512).unwrap();
    let result = engine.eval_value(source).unwrap().ok().expect("SVG/parser DOM guard threw");
    assert!(matches!(result, Value::Bool(true)));
}

#[test]
fn svg_native_hierarchy_dispatch_preserves_actual_names_cache_and_clone_import() {
    check(r#"(() => {
        const check = (ok, message) => { if (!ok) throw new Error(message); };
        const ns = 'http://www.w3.org/2000/svg';
        const hierarchy = [
            [SVGElement, Element], [SVGGraphicsElement, SVGElement], [SVGGeometryElement, SVGGraphicsElement],
            [SVGSVGElement, SVGGraphicsElement], [SVGGElement, SVGGraphicsElement],
            [SVGRectElement, SVGGeometryElement], [SVGPathElement, SVGGeometryElement],
            [SVGTextContentElement, SVGGraphicsElement], [SVGTextPositioningElement, SVGTextContentElement],
            [SVGTextElement, SVGTextPositioningElement], [SVGTextPathElement, SVGTextContentElement],
            [SVGLinearGradientElement, SVGGradientElement], [SVGGradientElement, SVGElement],
            [SVGScriptElement, SVGElement]
        ];
        for (const [child, parent] of hierarchy) {
            check(Object.getPrototypeOf(child.prototype) === parent.prototype, 'SVG prototype hierarchy');
            let illegal = false;
            try { new child(); } catch (error) { illegal = error instanceof TypeError; }
            check(illegal, 'SVG illegal constructors');
        }
        for (const [name, constructor] of [['svg', SVGSVGElement], ['g', SVGGElement], ['rect', SVGRectElement],
                ['path', SVGPathElement], ['textPath', SVGTextPathElement], ['linearGradient', SVGLinearGradientElement],
                ['foreignObject', SVGForeignObjectElement], ['script', SVGScriptElement]]) {
            const plain = document.createElementNS(ns, name);
            const prefixed = document.createElementNS(ns, 'p:' + name);
            for (const node of [plain, prefixed]) {
                check(Object.getPrototypeOf(node) === constructor.prototype && node instanceof SVGElement &&
                      node instanceof Element && node instanceof Node && !(node instanceof HTMLElement), 'native element dispatch');
                check(Object.getPrototypeOf(node.cloneNode(true)) === constructor.prototype, 'clone brand');
                const foreign = document.implementation.createDocument(null, '', null);
                check(Object.getPrototypeOf(foreign.importNode(node, true)) === constructor.prototype, 'import brand');
                check(foreign.adoptNode(node) === node && node instanceof constructor && node.ownerDocument === foreign, 'adopt keeps wrapper brand');
            }
        }
        const xml = new DOMParser().parseFromString('<p:svg xmlns:p="' + ns + '"><p:g><p:rect id="leaf"/></p:g></p:svg>', 'text/xml');
        check(xml.rootElement instanceof SVGSVGElement && xml.rootElement === xml.documentElement, 'XML root cache');
        check(xml.rootElement.getElementById('leaf') === xml.getElementById('leaf') &&
              xml.getElementById('leaf') instanceof SVGRectElement, 'scoped lookup wrapper identity');
        const other = document.createElementNS(ns, 'svg');
        other.id = 'leaf';
        check(other.getElementById('leaf') === null && xml.rootElement.getElementById('') === null, 'lookup excludes root');
        const parsed = new DOMParser().parseFromString('<svg><svg:svg></svg:svg><rect/><foreignobject/></svg>', 'text/html');
        const svg = parsed.querySelector('svg');
        check(svg instanceof SVGSVGElement && svg.firstChild.localName === 'svg:svg' &&
              Object.getPrototypeOf(svg.firstChild) === SVGElement.prototype &&
              svg.firstChild.nextSibling instanceof SVGRectElement && svg.lastChild instanceof SVGForeignObjectElement, 'HTML foreign tokenizer brands');
        check(document.createElementNS('urn:other', 'svg') instanceof Element &&
              !(document.createElementNS('urn:other', 'svg') instanceof SVGElement), 'namespace gate');
        return true;
    })()"#);
}

#[test]
fn svg_owner_ancestry_tracks_nested_detached_adopted_and_shadow_trees() {
    check(r#"(() => {
        const check = (ok, message) => { if (!ok) throw new Error(message); };
        const ns = 'http://www.w3.org/2000/svg';
        const outer = document.createElementNS(ns, 'p:svg');
        const group = document.createElementNS(ns, 'g');
        const inner = document.createElementNS(ns, 'svg');
        const rect = document.createElementNS(ns, 'rect');
        outer.appendChild(group); group.appendChild(inner); inner.appendChild(rect);
        check(outer.ownerSVGElement === null && group.ownerSVGElement === outer &&
              inner.ownerSVGElement === outer && rect.ownerSVGElement === inner, 'nearest ancestor excludes self');
        group.removeChild(inner);
        check(inner.ownerSVGElement === null && rect.ownerSVGElement === inner, 'detached tree ownership');
        const foreign = document.implementation.createDocument(null, '', null);
        check(foreign.adoptNode(inner) === inner && inner.ownerSVGElement === null && rect.ownerSVGElement === inner, 'adopted detached tree');
        foreign.appendChild(inner);
        check(inner.ownerSVGElement === null && rect.ownerSVGElement === inner, 'document root ancestry');
        const host = document.createElement('div');
        outer.appendChild(host);
        const shadow = host.attachShadow({mode: 'open'});
        const shadowRootSvg = document.createElementNS(ns, 'svg');
        const shadowRect = document.createElementNS(ns, 'rect');
        shadow.appendChild(shadowRootSvg); shadowRootSvg.appendChild(shadowRect);
        check(shadowRootSvg.ownerSVGElement === null && shadowRect.ownerSVGElement === shadowRootSvg, 'ordinary parents stop at shadow root');
        const getter = Object.getOwnPropertyDescriptor(SVGElement.prototype, 'ownerSVGElement').get;
        let illegal = false;
        try { getter.call(document.createElement('div')); } catch (error) { illegal = error instanceof TypeError; }
        check(illegal && getter.call(rect) === inner, 'native SVG receiver');
        return true;
    })()"#);
}

#[test]
fn xml_domparser_repairs_lone_surrogates_without_recoercing_or_breaking_pairs() {
    check(r#"(() => {
        const check = (ok, message) => { if (!ok) throw new Error(message); };
        const high = String.fromCharCode(0xD83C), low = String.fromCharCode(0xDD25);
        const paired = String.fromCharCode(0xD83D, 0xDD25);
        for (const mime of ['text/xml', 'application/xml', 'application/xhtml+xml', 'image/svg+xml']) {
            let calls = 0;
            const input = {toString() { calls++; return '<root attr="' + high + '"><![CDATA[' + low + paired + high + ']]></root>'; }};
            const xml = new DOMParser().parseFromString(input, mime);
            check(calls === 1 && xml.contentType === mime && xml.documentElement.localName === 'root', 'single DOMString conversion and MIME');
            check(xml.documentElement.getAttribute('attr') === '\uFFFD' &&
                  xml.documentElement.textContent === '\uFFFD' + paired + '\uFFFD', 'scalar repair and valid pair preservation');
            const bad = new DOMParser().parseFromString('<root>\u0000</root>', mime);
            check(bad.getElementsByTagName('parsererror').length === 1, 'XML invalid scalar still errors');
        }
        const sentinel = {};
        let thrown;
        try { new DOMParser().parseFromString({toString() { throw sentinel; }}, 'text/xml'); } catch (error) { thrown = error; }
        check(thrown === sentinel, 'input conversion exception identity');
        return true;
    })()"#);
}

#[test]
fn html_root_fragment_tail_comments_preserve_dom_order_and_document_parsing() {
    check(r#"(() => {
        const check = (ok, message) => { if (!ok) throw new Error(message); };
        for (const source of ['<head></head><body></body><!-- tail -->', '<body></body><!-- tail -->',
                              '<body></body></html><!-- tail -->']) {
            const html = document.createElement('html');
            html.innerHTML = source;
            check(html.childNodes.length === 3 && html.firstChild.localName === 'head' &&
                  html.childNodes[1].localName === 'body' && html.lastChild.nodeType === Node.COMMENT_NODE &&
                  html.lastChild.data === ' tail ' && html.childNodes[1].childNodes.length === 0, 'fragment tail sibling');
        }
        const parsed = new DOMParser().parseFromString('<!doctype html><html><head></head><body></body><!-- tail --></html><!-- outside -->', 'text/html');
        check(parsed.documentElement.lastChild.nodeType === Node.COMMENT_NODE &&
              parsed.documentElement.lastChild.data === ' tail ' && parsed.lastChild.nodeType === Node.COMMENT_NODE &&
              parsed.lastChild.data === ' outside ', 'full-document comment distinction');
        return true;
    })()"#);
}

#[test]
fn html_native_brands_distinguish_literal_colon_from_qualified_names_across_copy_and_adoption() {
    check(r#"(() => {
        const check = (ok, message) => { if (!ok) throw new Error(message); };
        const ns = 'http://www.w3.org/1999/xhtml';
        for (const [name, constructor] of [['html', HTMLHtmlElement], ['div', HTMLDivElement], ['br', HTMLBRElement],
                ['head', HTMLHeadElement], ['a', HTMLAnchorElement], ['area', HTMLAreaElement], ['body', HTMLBodyElement],
                ['title', HTMLTitleElement], ['base', HTMLBaseElement], ['link', HTMLLinkElement], ['script', HTMLScriptElement],
                ['img', HTMLImageElement], ['audio', HTMLAudioElement], ['video', HTMLVideoElement], ['canvas', HTMLCanvasElement],
                ['form', HTMLFormElement], ['details', HTMLDetailsElement], ['style', HTMLStyleElement], ['template', HTMLTemplateElement],
                ['iframe', HTMLIFrameElement], ['input', HTMLInputElement], ['select', HTMLSelectElement], ['option', HTMLOptionElement],
                ['textarea', HTMLTextAreaElement], ['slot', HTMLSlotElement], ['button', HTMLButtonElement]]) {
            const literal = document.createElement('x:' + name);
            const qualified = document.createElementNS(ns, 'x:' + name);
            check(literal.localName === 'x:' + name && literal.prefix === null &&
                  Object.getPrototypeOf(literal) === HTMLUnknownElement.prototype && !(literal instanceof constructor), 'literal native brand: ' + name);
            check(qualified.localName === name && qualified.prefix === 'x' && qualified instanceof constructor, 'qualified native brand: ' + name);
            for (const original of [literal, qualified]) {
                const brand = Object.getPrototypeOf(original);
                const clone = original.cloneNode(true);
                const foreign = document.implementation.createDocument(null, '', null);
                const imported = foreign.importNode(original, true);
                check(Object.getPrototypeOf(clone) === brand && Object.getPrototypeOf(imported) === brand &&
                      clone.isEqualNode(original) && imported.isEqualNode(original), 'copy brand and metadata: ' + name);
                check(foreign.adoptNode(original) === original && original.ownerDocument === foreign &&
                      Object.getPrototypeOf(original) === brand, 'adopt preserves native identity: ' + name);
            }
        }
        const getter = Object.getOwnPropertyDescriptor(HTMLScriptElement.prototype, 'src').get;
        const literalScript = document.createElement('x:script');
        const qualifiedScript = document.createElementNS(ns, 'x:script');
        let rejected = false;
        try { getter.call(literalScript); } catch (error) { rejected = error instanceof TypeError; }
        check(rejected && getter.call(qualifiedScript) === '', 'specialized native receiver follows real brand');
        globalThis.colonScriptRuns = 0;
        literalScript.textContent = 'globalThis.colonScriptRuns++;';
        document.body.appendChild(literalScript);
        check(colonScriptRuns === 0, 'connected literal colon script remains inert');
        qualifiedScript.textContent = 'globalThis.colonScriptRuns++;';
        document.body.appendChild(qualifiedScript);
        check(colonScriptRuns === 1, 'qualified HTML script executes through existing activation');
        literalScript.remove(); qualifiedScript.remove();
        const parsed = new DOMParser().parseFromString('<x:script></x:script><x:div></x:div><x:button></x:button>', 'text/html');
        for (const child of parsed.body.children) check(Object.getPrototypeOf(child) === HTMLUnknownElement.prototype, 'parser literal native brand');
        return true;
    })()"#);
}
