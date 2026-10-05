//! HTML event-handler content attributes backed by the native event-target list.
use super::*;
use lumen::embed::{JsFunction, Value};

pub(crate) enum Compilation {
    Inactive,
    Failed,
    Compiled(JsFunction),
}

pub(crate) fn compile(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    source: &events::RawContentHandler,
) -> Compilation {
    if !realm.has_browsing_context {
        return Compilation::Inactive;
    }

    let scopes = if source.window_target {
        Vec::new()
    } else {
        let document = realm.document_value(ctx);
        let form_owner = {
            let session = realm.session.borrow();
            lumen_html::forms::form_owner(session.document(), source.node)
        };
        let mut scopes = vec![document];
        if let Some(form) = form_owner {
            scopes.push(realm.wrap(ctx, form));
        }
        scopes.push(realm.wrap(ctx, source.node));
        scopes
    };

    match ctx.compile_event_handler(
        &source.name,
        &source.body,
        &scopes,
        source.window_target && source.name == "onerror",
    ) {
        Ok(function) => match JsFunction::from_value(function) {
            Some(function) => Compilation::Compiled(function),
            None => Compilation::Failed,
        },
        Err(exception) => {
            DomRealm::report_exception(ctx, lumen::embed::abrupt_value(exception));
            Compilation::Failed
        }
    }
}

pub(crate) fn attribute_changed(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    namespace_uri: Option<&str>,
    qualified_name: &str,
    value: Option<&str>,
) -> OpResult<()> {
    if namespace_uri.is_some() {
        return Ok(());
    }

    let event = handler_attribute(qualified_name, realm.is_html_document);
    let Some((attribute_name, event_type)) = event else {
        return Ok(());
    };
    let window_target = is_window_reflecting_body_attribute(realm, node, attribute_name);
    if !matches!(
        realm.session.borrow().document().kind(node),
        Ok(NodeKind::Element { .. })
    ) {
        return Ok(());
    }

    let (target, owner) = if window_target {
        let Some(data) = realm.window_target.borrow().as_ref().cloned() else {
            return Ok(());
        };
        let Some(owner) = realm
            .window_wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
        else {
            return Ok(());
        };
        (events::DomEventTarget::from_data(data), owner)
    } else {
        (
            events::DomEventTarget::node(realm, node),
            realm.wrap(ctx, node),
        )
    };
    let handler = value.map(|body| events::RawContentHandler {
        node,
        name: attribute_name.to_owned(),
        body: body.to_owned(),
        window_target,
        location: realm.base_url(),
    });
    target.set_content_handler(ctx, &owner, event_type, handler);
    Ok(())
}

pub(crate) fn window_handler_value(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    event_type: &str,
) -> OpResult<Value> {
    let Some(data) = realm.window_target.borrow().as_ref().cloned() else {
        return Ok(Value::Null);
    };
    let Some(owner) = realm
        .window_wrapper
        .borrow()
        .as_ref()
        .and_then(WeakValue::upgrade)
    else {
        return Ok(Value::Null);
    };
    events::DomEventTarget::from_data(data).handler_value(ctx, &owner, event_type)
}

pub(crate) fn set_window_handler(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    event_type: &str,
    callback: Option<JsFunction>,
) {
    let Some(data) = realm.window_target.borrow().as_ref().cloned() else {
        return;
    };
    let Some(owner) = realm
        .window_wrapper
        .borrow()
        .as_ref()
        .and_then(WeakValue::upgrade)
    else {
        return;
    };
    events::DomEventTarget::from_data(data).set_handler(ctx, &owner, event_type, callback);
}

pub(crate) fn element_handler_value(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    owner: &Value,
    event_type: &str,
) -> OpResult<Value> {
    let attribute = match event_type {
        "load" => "onload",
        "error" => "onerror",
        _ => "",
    };
    if !attribute.is_empty() && is_window_reflecting_body_attribute(realm, node, attribute) {
        window_handler_value(ctx, realm, event_type)
    } else {
        events::DomEventTarget::node(realm, node).handler_value(ctx, owner, event_type)
    }
}

pub(crate) fn set_element_handler(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    owner: &Value,
    event_type: &str,
    callback: Option<JsFunction>,
) {
    let attribute = match event_type {
        "load" => "onload",
        "error" => "onerror",
        _ => "",
    };
    if !attribute.is_empty() && is_window_reflecting_body_attribute(realm, node, attribute) {
        set_window_handler(ctx, realm, event_type, callback);
    } else {
        events::DomEventTarget::node(realm, node).set_handler(ctx, owner, event_type, callback);
    }
}

pub(crate) fn initialize_document(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    let root = realm.session.borrow().document().root();
    initialize_subtree(ctx, realm, root)
}

pub(crate) fn initialize_subtree(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    root: NodeId,
) -> OpResult<()> {
    let handlers = {
        let session = realm.session.borrow();
        let document = session.document();
        let mut handlers = Vec::new();
        let mut pending = vec![root];
        while let Some(node) = pending.pop() {
            if let NodeKind::Element { attributes, .. } = document.kind(node).map_err(dom_error)? {
                for (index, (name, value)) in attributes.iter().enumerate() {
                    if document.attribute_namespace_uri_at(node, index).is_none()
                        && handler_attribute(name.as_str(), realm.is_html_document).is_some()
                    {
                        handlers.push((node, name.to_string(), value.clone()));
                    }
                }
            }

            if let Some(content) = document.template_content(node).map_err(dom_error)? {
                pending.push(content);
            }
            let mut children = Vec::new();
            let mut child = document.first_child(node).map_err(dom_error)?;
            while let Some(current) = child {
                children.push(current);
                child = document.next_sibling(current).map_err(dom_error)?;
            }
            pending.extend(children.into_iter().rev());
        }
        handlers
    };

    for (node, name, body) in handlers {
        let Some((attribute, event_type)) = handler_attribute(&name, realm.is_html_document) else {
            continue;
        };
        let already_installed = if is_window_reflecting_body_attribute(realm, node, attribute) {
            realm.window_target.borrow().as_ref().is_some_and(|target| {
                events::DomEventTarget::from_data(target.clone()).has_handler(event_type)
            })
        } else {
            events::DomEventTarget::node(realm, node).has_handler(event_type)
        };
        if !already_installed {
            attribute_changed(ctx, realm, node, None, &name, Some(&body))?;
        }
    }
    Ok(())
}

fn handler_attribute(name: &str, html_document: bool) -> Option<(&'static str, &'static str)> {
    const HANDLERS: &[(&str, &str)] = &[
        ("onload", "load"),
        ("onerror", "error"),
        ("onkeydown", "keydown"),
        ("onkeyup", "keyup"),
        ("onkeypress", "keypress"),
        ("onbeforeinput", "beforeinput"),
        ("onbeforetoggle", "beforetoggle"),
        ("ontoggle", "toggle"),
        ("onclose", "close"),
        ("oncancel", "cancel"),
        ("onchange", "change"),
        ("oninvalid", "invalid"),
        ("onfocus", "focus"),
        ("onblur", "blur"),
        ("onfocusin", "focusin"),
        ("onfocusout", "focusout"),
        ("ondblclick", "dblclick"),
        ("onpointerdown", "pointerdown"),
        ("onpointerup", "pointerup"),
        ("onpointermove", "pointermove"),
        ("onpointercancel", "pointercancel"),
        ("onmousedown", "mousedown"),
        ("onmouseup", "mouseup"),
        ("onmousemove", "mousemove"),
        ("onmouseover", "mouseover"),
        ("onmouseout", "mouseout"),
        ("onmouseenter", "mouseenter"),
        ("onmouseleave", "mouseleave"),
        ("onwheel", "wheel"),
        ("onscroll", "scroll"),
        ("oncompositionstart", "compositionstart"),
        ("oncompositionupdate", "compositionupdate"),
        ("oncompositionend", "compositionend"),
        ("ontouchstart", "touchstart"),
        ("ontouchend", "touchend"),
        ("ontouchmove", "touchmove"),
        ("ontouchcancel", "touchcancel"),
        ("onvolumechange", "volumechange"),
        ("oninput", "input"),
        ("onclick", "click"),
        ("onsubmit", "submit"),
        ("onreset", "reset"),
    ];
    HANDLERS.iter().copied().find(|(attribute, _)| {
        if html_document {
            name.eq_ignore_ascii_case(attribute)
        } else {
            name == *attribute
        }
    })
}

fn is_window_reflecting_body_attribute(realm: &DomRealm, node: NodeId, attribute: &str) -> bool {
    if !matches!(attribute, "onload" | "onerror") {
        return false;
    }
    let document = realm.session.borrow();
    matches!(
        document.document().kind(node),
        Ok(NodeKind::Element {
            namespace: Namespace::Html,
            name,
            ..
        }) if if realm.is_html_document {
            name.as_str().eq_ignore_ascii_case("body")
                || name.as_str().eq_ignore_ascii_case("frameset")
        } else {
            matches!(name.as_str(), "body" | "frameset")
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_runtime::Runtime;

    fn evaluate(page: &str, source: &str) -> Value {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        crate::install(engine.ctx(), page, 256).expect("install document");
        match engine.eval_value(source).expect("valid event-handler test") {
            Ok(value) => value,
            Err(error) => match engine.describe_throw(error) {
                lumen::Completion::Throw { name, message } => panic!("{name}: {message}"),
                _ => unreachable!("describe_throw returns a throw completion"),
            },
        }
    }

    #[test]
    fn volumechange_handler_uses_native_slot_receiver_and_queued_event_order() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm = crate::install(engine.ctx(),
            r#"<audio id="audio" onvolumechange="calls.push('content')"></audio>"#, 64).unwrap();
        let evaluate = |engine: &mut lumen::Engine, source: &str| {
            engine.eval_value(source).expect("valid media-handler script")
                .ok().expect("media-handler script threw")
        };
        evaluate(engine, r#"
            var calls = [], receiverCorrect = false;
            const audio = document.getElementById('audio');
            const content = audio.onvolumechange;
            audio.addEventListener('volumechange', () => calls.push('listener'));
            audio.onvolumechange = function(event) {
                calls.push('idl');
                receiverCorrect = this === audio && event.currentTarget === audio && event.target === audio;
            };
            audio.muted = true;
            Promise.resolve().then(() => calls.push('micro'));
            if (calls.length) throw new Error('media handler fired synchronously');
        "#);
        while engine.run_one_job() {}
        assert!(matches!(evaluate(engine, "calls.join(',')==='micro'"), Value::Bool(true)));
        assert_eq!(realm.queue_media_tasks(engine.ctx()).unwrap(), 1);
        assert!(scheduling::run_tasks(engine, 64).is_empty());
        assert!(matches!(evaluate(engine,
            "receiverCorrect && calls.join(',')==='micro,idl,listener'"), Value::Bool(true)));
        evaluate(engine, "calls=[];audio.onvolumechange=null;audio.muted=false");
        assert_eq!(realm.queue_media_tasks(engine.ctx()).unwrap(), 1);
        assert!(scheduling::run_tasks(engine, 64).is_empty());
        assert!(matches!(evaluate(engine,
            "audio.onvolumechange===null && calls.join(',')==='listener'"), Value::Bool(true)));
        evaluate(engine, "calls=[];audio.setAttribute('onvolumechange',\"calls.push('attribute')\");audio.muted=true");
        assert_eq!(realm.queue_media_tasks(engine.ctx()).unwrap(), 1);
        assert!(scheduling::run_tasks(engine, 64).is_empty());
        assert!(matches!(evaluate(engine,
            "calls.join(',')==='listener,attribute'"), Value::Bool(true)));
    }

    #[test]
    fn content_handlers_compile_with_form_scope_and_keep_attribute_order() {
        let value = evaluate(
            r#"<form id="formScope"><input id="target" name="control" onload="globalThis.calls.push('content'); globalThis.scopeCheck = [documentMarker, formMarker, elementMarker, event.type, arguments.length, this === document.getElementById('target')].join('|')"></form><main id="mount"></main>"#,
            r#"
                globalThis.calls = [];
                const target = document.getElementById('target');
                const form = document.getElementById('formScope');
                globalThis.documentMarker = 'global';
                globalThis.formMarker = 'global';
                globalThis.elementMarker = 'global';
                document.documentMarker = 'document';
                form.formMarker = 'form';
                target.elementMarker = 'element';
                const first = target.onload;
                target.addEventListener('load', () => calls.push('listener'));
                target.dispatchEvent(new Event('load'));
                const parserAttributeRunsFirst = calls.join(',') === 'content,listener';
                const scopeIsCorrect = scopeCheck === 'document|form|element|load|1|true' &&
                  first.name === 'onload';

                target.onload = () => calls.push('property');
                target.dispatchEvent(new Event('load'));
                const idlOverrideKeepsSlot = calls.slice(2).join(',') === 'property,listener';
                target.setAttribute('onload', "calls.push('replacement')");
                target.dispatchEvent(new Event('load'));
                const attributeOverridesIdl = calls.slice(4).join(',') === 'replacement,listener';
                target.removeAttribute('onload');
                target.dispatchEvent(new Event('load'));
                const removalDeactivatesOnlyHandler = target.onload === null &&
                  calls.slice(6).join(',') === 'listener';

                const mount = document.getElementById('mount');
                mount.innerHTML = '<img id="inserted" onerror="globalThis.insertedError = this === event.currentTarget && event.type === \'error\'">';
                const inserted = document.getElementById('inserted');
                inserted.dispatchEvent(new Event('error'));
                parserAttributeRunsFirst && scopeIsCorrect && idlOverrideKeepsSlot &&
                  attributeOverridesIdl && removalDeactivatesOnlyHandler && insertedError
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn invalid_content_handler_is_reported_and_getter_returns_null() {
        let value = evaluate(
            r#"<div id="broken" onload="return ("></div>"#,
            r#"
                let reports = 0;
                let reportedSyntaxError = false;
                window.addEventListener('error', event => {
                  reports++;
                  reportedSyntaxError = event.error instanceof SyntaxError &&
                    event.message.length > 0;
                });
                const target = document.getElementById('broken');
                const getter = target.onload;
                getter === null && reports === 1 && reportedSyntaxError
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn click_keyboard_form_and_submit_content_handlers_use_native_targets() {
        let value = evaluate(
            r#"<form id="scope-form"></form><button id="button" form="scope-form" onclick="globalThis.clickScope = [documentMarker, formMarker, elementMarker, this === event.currentTarget].join('|'); return false" onkeydown="globalThis.keyValue = event.key"></button><input id="control" form="scope-form" oninput="globalThis.inputScope = [documentMarker, formMarker, controlMarker, this === event.currentTarget].join('|')"><form id="submit-form" onsubmit="globalThis.submits = (globalThis.submits || 0) + 1; return false" onreset="globalThis.resets = (globalThis.resets || 0) + 1"></form>"#,
            r#"
                globalThis.documentMarker = 'global';
                globalThis.formMarker = 'global';
                globalThis.elementMarker = 'global';
                globalThis.controlMarker = 'global';
                document.documentMarker = 'document';
                const scopeForm = document.getElementById('scope-form');
                scopeForm.formMarker = 'form';
                const button = document.getElementById('button');
                button.elementMarker = 'button';
                const clickHandler = button.onclick;
                const clickAllowed = button.dispatchEvent(new MouseEvent('click', { cancelable: true }));
                const clickPassed = !clickAllowed &&
                  clickScope === 'document|form|button|true' && clickHandler === button.onclick;

                const keyAllowed = button.dispatchEvent(new KeyboardEvent('keydown', { key: 'K' }));
                const keyPassed = keyAllowed && keyValue === 'K' && typeof button.onkeydown === 'function';

                const control = document.getElementById('control');
                control.controlMarker = 'control';
                control.dispatchEvent(new Event('input'));
                const formScopePassed = inputScope === 'document|form|control|true';

                const submitForm = document.getElementById('submit-form');
                const submitAllowed = submitForm.dispatchEvent(new Event('submit', { cancelable: true }));
                const resetAllowed = submitForm.dispatchEvent(new Event('reset', { cancelable: true }));
                clickPassed && keyPassed && formScopePassed && !submitAllowed &&
                  submitForm.dispatchEvent(new Event('submit', { cancelable: true })) === false &&
                  resetAllowed && submits === 2 && resets === 1
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn body_load_and_error_attributes_reflect_window_handlers() {
        let value = evaluate(
            r#"<body onload="globalThis.bodyLoadState = this === window && event.currentTarget === window && event.target === window" onerror="globalThis.bodyErrorArgs = [event, source, lineno, colno, error]; return true"></body>"#,
            r#"
                const body = document.body;
                const loadHandler = body.onload;
                const reflectedLoad = loadHandler === window.onload;
                const loadAllowed = window.dispatchEvent(new Event('load'));

                const original = new Error('original body error');
                const errorEvent = new ErrorEvent('error', {
                  message: 'resource failed', filename: 'asset.js', lineno: 12,
                  colno: 7, error: original, cancelable: true
                });
                const errorAllowed = window.dispatchEvent(errorEvent);
                const errorHandler = body.onerror;
                reflectedLoad && loadAllowed && bodyLoadState && !errorAllowed &&
                  errorHandler === window.onerror && bodyErrorArgs[0] === 'resource failed' &&
                  bodyErrorArgs[1] === 'asset.js' && bodyErrorArgs[2] === 12 &&
                  bodyErrorArgs[3] === 7 && bodyErrorArgs[4] === original
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }
}
