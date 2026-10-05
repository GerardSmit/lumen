//! Focused regression contracts for HTML forms collection, disabledness, and
//! date conversion behavior.

use lumen::Engine;
use lumen::embed::Value;
use lumen_runtime::Runtime;

fn eval(engine: &mut Engine, source: &str) -> Value {
    let result = engine.eval_value(source).expect("script parses");
    match result {
        Ok(value) => value,
        Err(error) => {
            let message = engine
                .ctx()
                .coerce_string(&error)
                .map(|message| message.to_string())
                .unwrap_or_else(|_| "unknown exception".into());
            panic!("batch 68 forms contract threw: {message}");
        }
    }
}

#[test]
fn value_as_date_checks_applicability_before_date_brand_conversion() {
    let mut runtime = Runtime::new();
    let engine = runtime.engine();
    crate::install(
        engine.ctx(),
        "<input id='supported' type='date'><input id='unsupported' type='text'>",
        32,
    )
    .expect("install HTML document");

    let result = eval(
        engine,
        r#"(() => {
            const failures = [];
            const check = (condition, name) => { if (!condition) failures.push(name); };
            const supported = document.getElementById('supported');
            const unsupported = document.getElementById('unsupported');
            let unsupportedObjectName = '';
            let unsupportedObjectCode = -1;
            try { unsupported.valueAsDate = {}; }
            catch (error) {
                unsupportedObjectName = error.name;
                unsupportedObjectCode = error.code;
            }
            check(unsupportedObjectName === 'InvalidStateError' && unsupportedObjectCode === 11,
                'unsupported-before-object-brand');

            let supportedObjectName = '';
            try { supported.valueAsDate = {}; }
            catch (error) { supportedObjectName = error.name; }
            check(supportedObjectName === 'TypeError', 'supported-requires-date-brand');

            supported.value = '2030-01-01';
            supported.valueAsDate = new Date(NaN);
            check(supported.value === '', 'invalid-date-clears');
            supported.value = '2030-01-01';
            supported.valueAsDate = undefined;
            check(supported.value === '', 'undefined-nullable-date-clears');

            const beyondTimeClip = document.createElement('input');
            beyondTimeClip.type = 'date';
            beyondTimeClip.value = '275760-09-14';
            const date = beyondTimeClip.valueAsDate;
            check(date instanceof Date && Number.isNaN(date.getTime()), 'getter-timeclip');

            const alteredPrototype = new Date(Date.UTC(2020, 0, 2));
            Object.setPrototypeOf(alteredPrototype, {});
            supported.valueAsDate = alteredPrototype;
            check(supported.value === '2020-01-02', 'brand-survives-prototype-change');
            return failures.join('|');
        })()"#,
    );
    let Value::Str(failures) = result else {
        panic!("valueAsDate contract must return diagnostics");
    };
    assert!(failures.is_empty(), "valueAsDate failures: {failures}");
}

#[test]
fn date_values_created_by_html_obey_time_clip() {
    let mut engine = Engine::new();
    let date = engine.ctx().new_date_value(8_640_000_000_000_001.0);
    let time = engine
        .ctx()
        .date_value(&date)
        .expect("native Date construction carries an internal slot");
    assert!(
        time.is_nan(),
        "out-of-range date values must be TimeClipped"
    );
}

#[test]
fn options_collection_length_indexing_and_named_item_are_live() {
    let mut engine = Engine::new();
    crate::install(
        engine.ctx(),
        "<select id='s'><option id='first'>one</option><optgroup><option name='named'>two</option></optgroup><option id='last'>three</option></select>",
        64,
    )
    .expect("install HTML document");
    let result = eval(
        &mut engine,
        r#"(() => {
            const failures = [];
            const check = (condition, name) => { if (!condition) failures.push(name); };
            const select = document.getElementById('s');
            const options = select.options;
            check(select.length === 3 && options.length === 3 && options === select.options,
                'live-length-and-same-object');
            check(select[0] === options[0] && select.item(1) === options[1],
                'select-indexed-and-item-access');
            check(options.namedItem('named') === options[1] &&
                options.namedItem('last') === options[2] && options.namedItem('missing') === null,
                'named-item-id-or-name-tree-order');
            check(select.namedItem('named') === options[1], 'select-named-item');

            options.length = 5;
            check(select.length === 5 && select.children.length === 5 && options[4].value === '',
                'length-growth-creates-empty-options');
            select.length = 3;
            check(options.length === 3 && options[4] === undefined, 'length-shrink-removes-tail');

            options[2] = null;
            check(select.length === 2 && select.children.length === 2, 'null-index-removes');
            const replacement = document.createElement('option');
            replacement.value = 'replacement';
            options[0] = replacement;
            check(select.length === 2 && select[0] === replacement &&
                select[0].value === 'replacement', 'indexed-replacement-preserves-length');
            const appended = document.createElement('option');
            appended.value = 'appended';
            options[4] = appended;
            check(select.length === 5 && select[4] === appended && select[3].value === '',
                'indexed-gap-appends-empty-options');
            return failures.join('|');
        })()"#,
    );
    let Value::Str(failures) = result else {
        panic!("options collection contract must return diagnostics");
    };
    assert!(
        failures.is_empty(),
        "options collection failures: {failures}"
    );
}

#[test]
fn datalist_options_and_input_list_follow_live_tree_roots() {
    let mut engine = Engine::new();
    crate::install(engine.ctx(), "<body></body>", 64).expect("install HTML document");
    let result = eval(
        &mut engine,
        r#"(() => {
            const failures = [];
            const check = (condition, name) => { if (!condition) failures.push(name); };
            const root = document.createElement('div');
            root.innerHTML = '<input list="suggestions"><datalist id="suggestions"><select><option value="a"><option value="b"></select><option value="c"></datalist>';
            const input = root.querySelector('input');
            const datalist = root.querySelector('datalist');
            check(input.list === datalist, 'list-resolves-in-detached-root');
            const options = datalist.options;
            check(options === datalist.options && options.length === 3 &&
                options[0].value === 'a' && options.item(1).value === 'b' && options[2].value === 'c',
                'datalist-options-tree-order-includes-nested-select');
            const extra = document.createElement('option');
            extra.value = 'd';
            datalist.append(extra);
            check(options.length === 4 && options[3] === extra, 'options-live-after-insertion');
            extra.remove();
            check(options.length === 3, 'options-live-after-removal');

            const otherRoot = document.createElement('div');
            const unrelated = document.createElement('datalist');
            unrelated.id = 'suggestions';
            otherRoot.append(unrelated);
            check(input.list === datalist, 'duplicate-id-in-other-root-is-ignored');
            datalist.id = 'renamed';
            check(input.list === null, 'list-requires-same-root-id-match');
            return failures.join('|');
        })()"#,
    );
    let Value::Str(failures) = result else {
        panic!("datalist contract must return diagnostics");
    };
    assert!(failures.is_empty(), "datalist failures: {failures}");
}

#[test]
fn disabled_nested_fieldsets_follow_the_first_legend_exception() {
    let mut engine = Engine::new();
    crate::install(
        engine.ctx(),
        "<fieldset disabled id='outer'><fieldset id='inner'><legend><input id='legend-control'></legend><input id='ordinary'></fieldset></fieldset><fieldset disabled id='owner'><legend><fieldset id='shielded'></fieldset></legend><legend></legend></fieldset>",
        64,
    )
    .expect("install HTML document");
    let result = eval(
        &mut engine,
        r#"(() => {
            const failures = [];
            const check = (condition, name) => { if (!condition) failures.push(name); };
            const inner = document.getElementById('inner');
            const legendControl = document.getElementById('legend-control');
            const ordinary = document.getElementById('ordinary');
            check(inner.matches(':disabled') && legendControl.matches(':disabled') &&
                ordinary.matches(':disabled'), 'outer-disabled-fieldset-inheritance');
            const owner = document.getElementById('owner');
            const shielded = document.getElementById('shielded');
            check(!shielded.matches(':disabled'), 'first-legend-exemption');
            owner.prepend(document.createElement('legend'));
            check(shielded.matches(':disabled'), 'first-legend-mutation-recomputes');
            return failures.join('|');
        })()"#,
    );
    let Value::Str(failures) = result else {
        panic!("fieldset contract must return diagnostics");
    };
    assert!(failures.is_empty(), "fieldset failures: {failures}");
}
