use lumen::embed::Value;

#[test]
fn document_stream_does_not_execute_detached_document_scripts_in_the_callers_window() {
    let mut engine = lumen::Engine::new();
    lumen_html_js::install(engine.ctx(), "<!doctype html><p>active</p>", 128).unwrap();
    let result = engine
        .eval_value(
            r#"(() => {
                globalThis.detachedStreamRuns = 0;
                for (const detached of [
                    document.implementation.createHTMLDocument('detached'),
                    document.cloneNode(false)
                ]) {
                    if (detached.defaultView !== null) throw new Error('detached document has a Window');
                    detached.open();
                    detached.write('<script>globalThis.detachedStreamRuns++;</script><b id=parsed>parsed</b>');
                    detached.close();
                    if (detached.getElementById('parsed').textContent !== 'parsed')
                        throw new Error('detached stream did not construct its DOM');
                }
                return detachedStreamRuns === 0 && document.querySelector('p').textContent === 'active';
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
            panic!("detached document stream regression threw: {message}");
        }
        _ => panic!("detached stream changed its caller's Window"),
    }
}

#[test]
fn document_stream_uses_incremental_parser_and_preserves_document_options() {
    let mut engine = lumen::Engine::new();
    lumen_html_js::install(
        engine.ctx(),
        "<!doctype html><div id='old'>original</div>",
        128,
    )
    .unwrap();

    let result = engine
        .eval_value(
            r#"(() => {
                const check = (label, condition) => {
                    if (!condition) throw new Error('document stream: ' + label);
                };
                const oldDocument = document;
                const oldNode = document.getElementById('old');
                const oldParent = oldNode.parentNode;
                check('open returns its document', document.open() === document);
                document.write('<!doctype html><div id="new">A &am');
                document.write('p; B</div>');
                document.close();

                const replacement = document.getElementById('new');
                check('split character reference and new node',
                    replacement && replacement.textContent === 'A & B');
                check('old node remains the same detached object',
                    oldNode.id === 'old' && !oldNode.isConnected &&
                    oldNode.parentNode === oldParent && oldParent.firstChild === oldNode &&
                    !oldParent.isConnected);

                const clone = oldDocument.cloneNode(true);
                clone.open();
                globalThis.detachedStreamScripts = 0;
                clone.write('<script>globalThis.detachedStreamScripts++</script><div id="host"><template shadowrootmode="open"><b>shadow</b></template></div>');
                clone.close();
                const host = clone.getElementById('host');
                check('clone retains declarative shadow permission across open/write/close',
                    host && host.shadowRoot && host.shadowRoot.textContent === 'shadow');
                check('detached document stream does not execute parser scripts',
                    detachedStreamScripts === 0);

                const xml = document.implementation.createDocument('urn:stream-test', 'root');
                let xmlRejected = false;
                try { xml.write('<child/>'); }
                catch (error) { xmlRejected = error.name === 'InvalidStateError'; }
                check('XML document rejects write', xmlRejected);
                return true;
            })()"#,
        )
        .unwrap();
    let result = match result {
        Ok(value) => value,
        Err(error) => {
            let message = engine
                .ctx()
                .coerce_string(&error)
                .map(|message| message.to_string())
                .unwrap_or_else(|_| "unknown exception".into());
            panic!("document stream regression threw: {message}");
        }
    };

    assert!(matches!(result, Value::Bool(true)));
}

#[test]
fn document_stream_executes_inline_parser_scripts_at_reentrant_checkpoints() {
    let mut engine = lumen::Engine::new();
    lumen_html_js::install(engine.ctx(), "<!doctype html><p>initial</p>", 128).unwrap();

    let result = engine
        .eval_value(
            r#"(() => {
                const check = (label, condition) => {
                    if (!condition) throw new Error('document stream checkpoint: ' + label);
                };
                globalThis.checkpointEvents = [];
                document.open();
                document.write(
                    '<div id="ordered"><script id="writer">' +
                    'checkpointEvents.push("outer-before:" + document.currentScript.id);' +
                    'document.write("<b id=nested>nested</b><script id=inline-nested>' +
                        'document.write(\'<em id=innermost>inner</em>\');' +
                        'if (document.getElementById(\'outer-write-tail\')) throw new Error(\'nested write consumed caller tail\');' +
                        'checkpointEvents.push(document.currentScript.id)' +
                    '</scr" + "ipt><u id=outer-write-tail>outer tail</u>");' +
                    'if (!document.getElementById("nested")) throw new Error("write was not parsed synchronously");' +
                    'if (!document.getElementById("outer-write-tail")) throw new Error("caller write tail was not resumed synchronously");' +
                    'document.write("<span id=cr>");' +
                    'document.write("A" + String.fromCharCode(13));' +
                    'checkpointEvents.push("outer-after");' +
                    '</scr',
                    'ipt></span><i id="tail">tail</i></div>'
                );
                check('inserted parser script runs before document.write returns',
                    checkpointEvents.join(',') === 'outer-before:writer,inline-nested,outer-after');
                check('missing doctype selects quirks after the first non-whitespace token',
                    document.compatMode === 'BackCompat');
                const writer = document.getElementById('writer');
                const nested = document.getElementById('nested');
                const inlineNested = document.getElementById('inline-nested');
                const innermost = document.getElementById('innermost');
                const outerWriteTail = document.getElementById('outer-write-tail');
                const tail = document.getElementById('tail');
                check('nested write is inserted before unread source',
                    writer.nextSibling === nested && nested.nextSibling === inlineNested &&
                    inlineNested.nextSibling === innermost && innermost.nextSibling === outerWriteTail &&
                    outerWriteTail.nextSibling === document.getElementById('cr') &&
                    document.getElementById('cr').nextSibling === tail &&
                    document.getElementById('cr').textContent === 'A\n' &&
                    innermost.textContent === 'inner');
                document.close();
                check('close reaches complete after the checkpoint resumes',
                    document.readyState === 'complete');

                document.open();
                check('document.open starts in no-quirks mode',
                    document.compatMode === 'CSS1Compat');
                document.write(
                    '<script id="reopener">' +
                    'globalThis.currentDuringOpen = document.currentScript;' +
                    'document.open();' +
                    'globalThis.currentAfterOpen = document.currentScript;' +
                    'document.write("<p id=after-open>new stream</p>");' +
                    'document.close();' +
                    '</script><span id="retained-tail">old stream</span>'
                );
                check('open is a no-op during parser-script execution',
                    currentDuringOpen === currentAfterOpen &&
                    currentDuringOpen.id === 'reopener');
                check('nested write and unread parser tail are both retained',
                    document.getElementById('after-open') !== null &&
                    document.getElementById('retained-tail') !== null &&
                    document.getElementById('after-open').nextSibling ===
                        document.getElementById('retained-tail'));
                check('reentrant close completes the replacement stream',
                    document.readyState === 'complete');

                document.open();
                document.write('<span id="discarded">old stream</span>');
                document.open();
                check('replacement open also starts in no-quirks mode',
                    document.compatMode === 'CSS1Compat');
                document.write('<p id="replacement">new stream</p>');
                document.close();
                check('open outside parser-script execution replaces the stream',
                    document.getElementById('discarded') === null &&
                    document.getElementById('replacement') !== null);
                return true;
            })()"#,
        )
        .unwrap();
    let result = match result {
        Ok(value) => value,
        Err(error) => {
            let message = engine
                .ctx()
                .coerce_string(&error)
                .map(|message| message.to_string())
                .unwrap_or_else(|_| "unknown exception".into());
            panic!("document stream checkpoint regression threw: {message}");
        }
    };

    assert!(matches!(result, Value::Bool(true)));
}

#[test]
fn document_open_erases_old_event_listeners_and_keeps_new_listeners() {
    let mut engine = lumen::Engine::new();
    lumen_html_js::install(
        engine.ctx(),
        "<!doctype html><body onload='globalThis.oldBodyHandler++'><button id=old onclick='globalThis.oldNodeHandler++'>old</button></body>",
        128,
    )
    .unwrap();
    let result = engine
        .eval_value(
            r#"(() => {
                globalThis.oldBodyHandler = 0;
                globalThis.oldNodeHandler = 0;
                let oldDocumentEvents = 0;
                let oldWindowEvents = 0;
                let oldNodeEvents = 0;
                let oldShadowEvents = 0;
                let newDocumentEvents = 0;
                let newWindowEvents = 0;
                let newNodeEvents = 0;
                const oldDocument = document;
                const oldWindow = window;
                const oldBody = document.body;
                const oldNode = document.getElementById('old');
                const shadowHost = document.createElement('div');
                document.body.appendChild(shadowHost);
                const oldShadowRoot = shadowHost.attachShadow({mode: 'closed'});
                const oldShadowNode = document.createElement('button');
                oldShadowRoot.appendChild(oldShadowNode);
                document.addEventListener('readystatechange', () => oldDocumentEvents++);
                window.addEventListener('open-probe', () => oldWindowEvents++);
                oldNode.addEventListener('open-probe', () => oldNodeEvents++);
                oldShadowNode.addEventListener('open-probe', () => oldShadowEvents++);
                oldNode.dispatchEvent(new Event('click'));
                oldWindow.dispatchEvent(new Event('load'));
                if (oldNodeHandler !== 1 || oldBodyHandler !== 1)
                    throw new Error('content-handler fixtures were not active before open');

                document.open();
                if (oldDocumentEvents !== 0)
                    throw new Error('pre-open Document listener observed the loading transition');
                oldDocument.dispatchEvent(new Event('readystatechange'));
                oldWindow.dispatchEvent(new Event('open-probe'));
                oldNode.dispatchEvent(new Event('open-probe'));
                oldShadowNode.dispatchEvent(new Event('open-probe'));
                oldNode.dispatchEvent(new Event('click'));
                oldWindow.dispatchEvent(new Event('load'));
                if (oldDocumentEvents || oldWindowEvents || oldNodeEvents || oldShadowEvents ||
                    oldNodeHandler !== 1 || oldBodyHandler !== 1 ||
                    oldNode.onclick !== null || oldBody.onload !== null)
                    throw new Error('old Document, Window, descendant listener, or content handler survived open');

                document.addEventListener('readystatechange', () => newDocumentEvents++);
                window.addEventListener('open-probe', () => newWindowEvents++);
                document.write('<button id=new>new</button>');
                const newNode = document.getElementById('new');
                newNode.addEventListener('open-probe', () => newNodeEvents++);
                oldWindow.dispatchEvent(new Event('open-probe'));
                newNode.dispatchEvent(new Event('open-probe'));
                document.close();
                return oldDocument === document && oldWindow === window &&
                    oldDocumentEvents === 0 && oldWindowEvents === 0 && oldNodeEvents === 0 &&
                    oldShadowEvents === 0 &&
                    oldNodeHandler === 1 && oldBodyHandler === 1 &&
                    newDocumentEvents > 0 && newWindowEvents === 1 && newNodeEvents === 1;
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
            panic!("document.open event erasure regression threw: {message}");
        }
        _ => panic!("document.open retained old event listeners or erased new ones"),
    }
}
