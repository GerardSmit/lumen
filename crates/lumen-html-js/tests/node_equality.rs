use lumen::embed::Value;

fn check(source: &str) {
    let mut runtime = lumen_runtime::Runtime::new_browser();
    let engine = runtime.engine();
    let _realm = lumen_html_js::install(engine.ctx(), "<!doctype html><main></main>", 512).unwrap();
    let result = engine.eval_value(source).unwrap().ok().expect("node equality guard threw");
    assert!(matches!(result, Value::Bool(true)));
}

#[test]
fn equality_preserves_cross_document_structure_unordered_attributes_and_native_branding() {
    check(r#"(() => {
        const check = (ok, message) => { if (!ok) throw new Error(message); };
        const a = document.createElementNS('urn:element', 'p:root');
        a.setAttributeNS('urn:attribute', 'a:key', 'value');
        a.setAttribute('ordinary', 'second');
        a.append(document.createTextNode('text'), document.createComment('comment'));
        const foreign = document.implementation.createDocument('', '', null);
        const b = foreign.importNode(a, true);
        b.removeAttribute('ordinary'); b.setAttribute('ordinary', 'second');
        b.setAttributeNS('urn:attribute', 'b:key', 'value');
        check(a.isEqualNode(b) && b.isEqualNode(a) && a !== b, 'cross-document equality');
        check(a.attributes[0].isEqualNode(b.getAttributeNodeNS('urn:attribute', 'key')), 'Attr prefixes ignored');
        b.lastChild.data = 'mutation';
        check(!a.isEqualNode(b), 'descendant mutation');
        b.lastChild.data = 'comment';
        b.insertBefore(b.lastChild, b.firstChild);
        check(!a.isEqualNode(b), 'child order');
        check(!a.isEqualNode(null) && !a.isEqualNode(undefined) && !a.isEqualNode(), 'nullable argument');
        let invalid = false, receiver = false;
        try { a.isEqualNode({nodeType: 1}); } catch (e) { invalid = e instanceof TypeError; }
        try { Node.prototype.isEqualNode.call({}, a); } catch (e) { receiver = e instanceof TypeError; }
        check(invalid && receiver, 'native wrapper checks');
        return true;
    })()"#);
}

#[test]
fn equality_compares_namespaces_prefixes_doctypes_and_character_data() {
    check(r#"(() => {
        const check = (ok, message) => { if (!ok) throw new Error(message); };
        const a = document.createElementNS('urn:a', 'p:name');
        check(a.isEqualNode(document.createElementNS('urn:a', 'p:name')), 'same element');
        check(!a.isEqualNode(document.createElementNS('urn:b', 'p:name')) &&
              !a.isEqualNode(document.createElementNS('urn:a', 'q:name')) &&
              !a.isEqualNode(document.createElementNS('urn:a', 'p:other')), 'element identity fields');
        const attr = document.createAttribute('p:name'); attr.value = 'value';
        const other = document.createAttribute('q:name'); other.value = 'value';
        check(!attr.isEqualNode(other), 'unnamespaced Attr colon remains local name');
        const dt = document.implementation.createDocumentType('name', 'public', 'system');
        check(dt.isEqualNode(dt.cloneNode()) &&
              !dt.isEqualNode(document.implementation.createDocumentType('other', 'public', 'system')) &&
              !dt.isEqualNode(document.implementation.createDocumentType('name', 'other', 'system')) &&
              !dt.isEqualNode(document.implementation.createDocumentType('name', 'public', 'other')), 'doctype fields');
        const pi = document.createProcessingInstruction('target', 'data');
        check(pi.isEqualNode(pi.cloneNode()) &&
              !pi.isEqualNode(document.createProcessingInstruction('other', 'data')) &&
              !pi.isEqualNode(document.createProcessingInstruction('target', 'other')), 'PI fields');
        const xml = document.implementation.createDocument('', '', null);
        check(xml.isEqualNode(document.implementation.createDocument('', '', null)), 'document ownership ignored');
        const text = document.createTextNode('data');
        check(text.isEqualNode(xml.createTextNode('data')) && !text.isEqualNode(xml.createCDATASection('data')) &&
              !text.isEqualNode(document.createComment('data')), 'character node types');
        const templateA = document.createElement('template'), templateB = document.createElement('template');
        templateA.innerHTML = '<b>different template contents</b>';
        check(templateA.isEqualNode(templateB), 'template contents are a separate tree');
        const hostA = document.createElement('div'), hostB = document.createElement('div');
        hostA.attachShadow({mode: 'open'}).innerHTML = '<b>shadow</b>';
        check(hostA.isEqualNode(hostB), 'shadow contents are a separate tree');
        return true;
    })()"#);
}
