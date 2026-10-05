use super::*;
use lumen::Engine;
use lumen::embed::Value;
use lumen_runtime::Runtime;
use std::{cell::RefCell, rc::Rc};

fn eval(engine: &mut Engine, source: &str) -> Value {
    match engine.eval_value(source).expect("script parses") {
        Ok(value) => value,
        Err(error) => {
            let message = engine
                .ctx()
                .get_member(&error, "message")
                .ok()
                .and_then(|value| match value {
                    Value::Str(message) => Some(message.to_string()),
                    _ => None,
                })
                .unwrap_or_else(|| "script threw".into());
            panic!("{message}");
        }
    }
}

fn element(realm: &DomRealm, id: &str) -> NodeId {
    let session = realm.session.borrow();
    lumen_html::selector::get_element_by_id(session.document(), session.document().root(), id)
        .expect("valid id")
        .expect("element exists")
}

#[test]
fn form_submission_events_are_branded_trusted_and_identity_preserving() {
    let mut runtime = Runtime::new();
    let engine = runtime.engine();
    let realm = install(
        engine.ctx(),
        "<div id='container'><form id='form'><input id='field' name='field' value='v'><button id='send'></button></form></div>",
        64,
    )
    .unwrap();
    realm.set_document_url("https://example.test/form");
    let value = eval(
        engine,
        r#"(() => {
            const form = document.getElementById('form');
            const container = document.getElementById('container');
            const button = document.getElementById('send');
            const NativeFormData = FormData;
            const order = [];
            const init = {
              get bubbles() { order.push('bubbles'); return true; },
              get cancelable() { order.push('cancelable'); return false; },
              get composed() { order.push('composed'); return false; },
              get submitter() { order.push('submitter'); return button; }
            };
            const constructed = new SubmitEvent('submit', init);
            const defaultedSubmitter = new SubmitEvent('submit', {submitter: undefined});
            class SpecializedSubmitEvent extends SubmitEvent {}
            const specialized = new SpecializedSubmitEvent('submit', {submitter: button});
            const constructedOk = constructed instanceof SubmitEvent &&
              constructed instanceof Event && constructed.submitter === button &&
              defaultedSubmitter.submitter === null &&
              specialized instanceof SpecializedSubmitEvent && specialized.submitter === button &&
              !Object.hasOwn(constructed, 'submitter') && order.join(',') ===
                'bubbles,cancelable,composed,submitter';
            let fakeRejected = false;
            const fake = Object.create(NativeFormData.prototype);
            try { new FormDataEvent('formdata', {formData: fake}); }
            catch (error) { fakeRejected = error instanceof TypeError; }

            let submitOk = false;
            let formdataOk = false;
            let formdataBubbled = false;
            let observedFormData = null;
            container.addEventListener('formdata', event => {
              formdataBubbled = event.target === form && event.isTrusted;
            });
            form.addEventListener('submit', event => {
              submitOk = event instanceof SubmitEvent && event instanceof Event &&
                event.isTrusted && event.bubbles && event.cancelable && !event.composed &&
                event.submitter === button;
              event.preventDefault();
            });
            form.addEventListener('formdata', event => {
              formdataOk = event instanceof FormDataEvent && event instanceof Event &&
                event.isTrusted && event.bubbles && !event.cancelable && !event.composed &&
                !Object.hasOwn(event, 'formData') && event.formData.get('field') === 'v';
              observedFormData = event.formData;
            });
            const constructedData = new FormData();
            const event = new FormDataEvent('formdata', {formData: constructedData});
            const formDataConstructorOk = event.formData === constructedData &&
              !Object.hasOwn(event, 'formData');
            const frame = document.createElement('iframe');
            form.appendChild(frame);
            const foreignData = new frame.contentWindow.FormData();
            const foreignBrandAccepted =
              new FormDataEvent('formdata', {formData: foreignData}).formData === foreignData;
            const formData = new FormData(form, button);
            form.requestSubmit(button);
            Object.defineProperty(NativeFormData, Symbol.hasInstance, {
              configurable: true, value: () => true
            });
            globalThis.FormData = function ReplacedFormData() {};
            const spoof = Object.create(NativeFormData.prototype);
            let spoofRejectedAfterMutation = false;
            try { new FormDataEvent('formdata', {formData: spoof}); }
            catch (error) { spoofRejectedAfterMutation = error instanceof TypeError; }
            const retainedBrandAccepted = new FormDataEvent('formdata', {formData}).formData === formData;
            const failures = [];
            if (!constructedOk) failures.push('submit-event-constructor');
            if (!fakeRejected) failures.push('formdata-event-rejects-prototype-spoof');
            if (!formDataConstructorOk) failures.push('formdata-event-constructor-identity');
            if (!submitOk) failures.push('trusted-submit-event');
            if (!formdataOk) failures.push('trusted-formdata-event-flags-or-identity');
            if (!formdataBubbled) failures.push('formdata-did-not-bubble');
            if (observedFormData !== formData || formData.get('field') !== 'v')
              failures.push('formdata-object-or-entry');
            if (!foreignBrandAccepted) failures.push('cross-realm-formdata-brand');
            if (!Object.hasOwn(FormDataEvent.prototype, 'formData')) failures.push('formdata-prototype');
            if (!spoofRejectedAfterMutation) failures.push('mutable-hasinstance-spoof');
            if (!retainedBrandAccepted) failures.push('real-formdata-after-global-replacement');
            return failures.join(',');
          })()"#,
    );
    match value {
        Value::Str(failures) => assert!(failures.is_empty(), "form event failures: {failures}"),
        _ => panic!("form event diagnostics returned an unexpected value"),
    }
}

#[test]
fn click_activation_rechecks_type_and_form_owner_after_listeners() {
    let mut runtime = Runtime::new();
    let engine = runtime.engine();
    let realm = install(
        engine.ctx(),
        "<form id='old'></form><form id='new'><input id='control' type='button' name='value' value='before'></form>",
        64,
    )
    .unwrap();
    realm.set_document_url("https://example.test/form");
    let submitted = Rc::new(RefCell::new(Vec::new()));
    let captured = submitted.clone();
    let expected_form = element(&realm, "old");
    realm.set_form_submission_host(Rc::new(move |request| {
        let entry = request
            .metadata
            .entries
            .first()
            .and_then(|entry| match &entry.value {
                forms::FormEntryValue::Text(value) => Some(value.clone()),
                forms::FormEntryValue::File(_) => None,
            });
        captured
            .borrow_mut()
            .push((request.form == expected_form, entry));
        Ok(())
    }));
    let node = element(&realm, "control");
    let value = eval(
        engine,
        r#"(() => {
            const control = document.getElementById('control');
            const form = document.getElementById('old');
            let submit = false;
            let submitDetail = '';
            let clickDetail = '';
            form.addEventListener('submit', event => {
              submitDetail = [event.constructor.name, event.type, event.isTrusted,
                event.bubbles, event.cancelable, event.composed,
                event.submitter === control].join(':');
              submit = event instanceof SubmitEvent && event.isTrusted &&
                event.submitter === control && !event.composed;
            });
            control.addEventListener('click', event => {
              clickDetail = [event.constructor.name, event.type, event.isTrusted].join(':');
            }, {once: true});
            control.addEventListener('click', () => {
              control.type = 'submit';
              control.setAttribute('form', 'old');
              control.value = 'after';
            }, {once: true});
            control.click();
            const failures = [];
            if (!clickDetail) failures.push('click-missing');
            if (!submit) failures.push('submit:' + submitDetail);
            return failures.join(',');
          })()"#,
    );
    match value {
        Value::Str(failures) => {
            assert!(failures.is_empty(), "click activation failures: {failures}")
        }
        _ => panic!("click activation diagnostics returned an unexpected value"),
    }
    assert_eq!(&*submitted.borrow(), &[(true, Some("after".to_owned()))]);
    assert_eq!(
        realm
            .session
            .borrow()
            .document()
            .get_attribute_ns_ref(node, None, "type")
            .unwrap(),
        Some("submit")
    );
}

#[test]
fn trusted_enter_uses_current_implicit_submission_rule() {
    let mut runtime = Runtime::new();
    let engine = runtime.engine();
    let realm = install(
        engine.ctx(),
        "<form id='form'><input id='field' name='value'><button id='default' name='send' value='yes'></button></form>",
        64,
    )
    .unwrap();
    realm.set_document_url("https://example.test/form");
    let requests = Rc::new(RefCell::new(Vec::new()));
    let captured = requests.clone();
    realm.set_form_submission_host(Rc::new(move |request| {
        captured.borrow_mut().push(request.form);
        Ok(())
    }));
    let input = element(&realm, "field");
    let value = eval(
        engine,
        r#"(() => {
            const input = document.getElementById('field');
            const button = document.getElementById('default');
            const form = document.getElementById('form');
            const seen = [];
            input.addEventListener('keydown', event => seen.push('keydown:' + event.isTrusted));
            button.addEventListener('click', event => seen.push('click:' + event.isTrusted));
            form.addEventListener('submit', event => {
              seen.push('submit:' + (event instanceof SubmitEvent) + ':' +
                event.isTrusted + ':' + (event.submitter === button) + ':' + event.composed);
            });
            globalThis.__implicitEvents = seen;
            return true;
          })()"#,
    );
    assert!(matches!(value, Value::Bool(true)));
    keyboard_automation::trusted_send_keys(engine.ctx(), &realm, input, "\u{e007}")
        .expect("Enter is a supported implicit-submission key");
    assert_eq!(requests.borrow().len(), 1);
    let value = eval(engine, "globalThis.__implicitEvents.join(',')");
    match value {
        Value::Str(actual) => assert_eq!(
            actual.as_ref(),
            "keydown:true,click:true,submit:true:true:true:false",
            "implicit submission event sequence"
        ),
        _ => panic!("implicit submission events did not produce a string"),
    }

    // Cancellation at keydown prevents the default click and submission.
    let second = install(
        engine.ctx(),
        "<form><input id='cancel'><button></button></form>",
        32,
    )
    .unwrap();
    second.set_document_url("https://example.test/cancel");
    let canceled_input = element(&second, "cancel");
    eval(
        engine,
        "document.getElementById('cancel').addEventListener('keydown', event => event.preventDefault())",
    );
    let before = requests.borrow().len();
    keyboard_automation::trusted_send_keys(engine.ctx(), &second, canceled_input, "\u{e007}")
        .expect("canceled Enter remains a supported key");
    assert_eq!(requests.borrow().len(), before);
}

#[test]
fn trusted_enter_rechecks_form_owner_and_type_after_keydown() {
    let mut runtime = Runtime::new();
    let engine = runtime.engine();
    let realm = install(
        engine.ctx(),
        concat!(
            "<form id='first'><input id='move' form='first'></form>",
            "<form id='second'><button id='default'></button></form>",
            "<form id='third'><input id='change-type'></form>"
        ),
        64,
    )
    .unwrap();
    realm.set_document_url("https://example.test/form");
    let expected_form = element(&realm, "second");
    let requests = Rc::new(RefCell::new(Vec::new()));
    let captured = requests.clone();
    realm.set_form_submission_host(Rc::new(move |request| {
        captured.borrow_mut().push(request.form);
        Ok(())
    }));

    let moving_input = element(&realm, "move");
    eval(
        engine,
        r#"(() => {
            const input = document.getElementById('move');
            const button = document.getElementById('default');
            const form = document.getElementById('second');
            let sawTrustedDefault = false;
            button.addEventListener('click', event => { sawTrustedDefault = event.isTrusted; });
            form.addEventListener('submit', event => {
              sawTrustedDefault = sawTrustedDefault && event instanceof SubmitEvent &&
                event.isTrusted && event.submitter === button && !event.composed;
            });
            input.addEventListener('keydown', () => input.setAttribute('form', 'second'), {once: true});
            globalThis.__sawTrustedDefault = () => sawTrustedDefault;
          })()"#,
    );
    keyboard_automation::trusted_send_keys(engine.ctx(), &realm, moving_input, "\u{e007}")
        .expect("Enter remains supported after its owner changes");
    assert_eq!(&*requests.borrow(), &[expected_form]);
    assert!(matches!(
        eval(engine, "globalThis.__sawTrustedDefault()"),
        Value::Bool(true)
    ));

    // A keydown listener can also change the target out of the blocking-input state. The
    // algorithm is recomputed after the listener, so no stale form submission occurs.
    let type_changing_input = element(&realm, "change-type");
    eval(
        engine,
        r#"(() => {
            const input = document.getElementById('change-type');
            input.addEventListener('keydown', () => { input.type = 'button'; }, {once: true});
          })()"#,
    );
    keyboard_automation::trusted_send_keys(engine.ctx(), &realm, type_changing_input, "\u{e007}")
        .expect("Enter remains supported when the keydown callback changes type");
    assert_eq!(&*requests.borrow(), &[expected_form]);
}
