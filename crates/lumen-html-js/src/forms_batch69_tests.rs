//! Focused native checks for form IDL reflection and select reset behavior.

use lumen::embed::Value;
use lumen::Engine;
use lumen_runtime::Runtime;

fn eval(engine: &mut Engine, source: &str) -> Value {
    match engine.eval_value(source).expect("script parses") {
        Ok(value) => value,
        Err(error) => {
            let message = engine
                .ctx()
                .coerce_string(&error)
                .map(|message| message.to_string())
                .unwrap_or_else(|_| "unknown exception".into());
            panic!("batch 69 forms contract threw: {message}");
        }
    }
}

#[test]
fn forms_batch69_name_button_textarea_object_and_select_reflection() {
    let mut runtime = Runtime::new();
    let engine = runtime.engine();
    crate::install(
        engine.ctx(),
        "<form id='owner'><textarea id='ta' maxlength='bad' rows='0' cols='bad' wrap='off'>\nstart</textarea><button id='button'></button><object id='inside'></object></form><object id='external' form='owner'></object><p id='name1' name='Match'></p><p id='name2' name='match'></p><svg id='foreign' name='Match'></svg>",
        128,
    )
    .expect("install HTML document");

    let result = eval(
        engine,
        r#"(() => {
            const failures = [];
            const check = (condition, name) => { if (!condition) failures.push(name); };

            const named = document.getElementsByName('Match');
            check(named instanceof NodeList && !(named instanceof HTMLCollection),
                'name-returns-nodelist');
            check(named.length === 1 && named[0].id === 'name1',
                'name-is-case-sensitive-and-html-namespace-filtered');
            document.getElementById('name2').setAttribute('name', 'Match');
            check(named.length === 2 && named[1].id === 'name2', 'name-list-is-live');
            const added = document.createElement('input');
            added.name = 'Match';
            document.body.append(added);
            check(named.length === 3 && named[2] === added, 'name-list-sees-new-elements');
            added.remove();
            check(named.length === 2, 'name-list-sees-removal');

            const button = document.getElementById('button');
            check(button.type === 'submit', 'button-type-default');
            button.type = 'ReSeT';
            check(button.type === 'reset', 'button-type-ascii-case');
            button.type = 'not-a-button-state';
            check(button.type === 'submit', 'button-type-invalid-default');

            const textarea = document.getElementById('ta');
            check(textarea.type === 'textarea' && textarea.maxLength === -1 &&
                textarea.minLength === -1, 'textarea-type-and-limit-defaults');
            check(textarea.rows === 1 && textarea.cols === 20 && textarea.wrap === 'soft',
                'textarea-size-and-wrap-normalization');
            textarea.value = 'A😀';
            check(textarea.textLength === 3, 'textarea-text-length-utf16');
            textarea.maxLength = 'not-a-number';
            check(textarea.maxLength === 0, 'textarea-long-setter-coercion');
            let indexSize = false;
            try { textarea.minLength = -5; }
            catch (error) { indexSize = error.name === 'IndexSizeError'; }
            check(indexSize, 'textarea-negative-limit-throws');
            textarea.wrap = 'hard';
            textarea.readOnly = true;
            textarea.placeholder = 'hint';
            check(textarea.wrap === 'hard' && textarea.readOnly &&
                textarea.placeholder === 'hint', 'textarea-reflected-members');

            const form = document.getElementById('owner');
            check(document.getElementById('inside').form === form &&
                document.getElementById('external').form === form,
                'object-form-owner-in-tree-and-by-id');

            const select = document.createElement('select');
            const first = document.createElement('option');
            first.id = 'first';
            first.defaultSelected = true;
            const later = document.createElement('option');
            later.id = 'later';
            later.defaultSelected = true;
            const fragment = document.createDocumentFragment();
            fragment.append(first, later);
            select.appendChild(fragment);
            check(select.selectedIndex === 1 && later.selected && !first.selected,
                'insertion-keeps-last-selected-option');
            select.multiple = true;
            first.selected = true;
            later.selected = true;
            select.removeAttribute('multiple');
            check(select.selectedIndex === 0 && first.selected && !later.selected,
                'removing-multiple-keeps-first-selected-option');
            later.remove();
            check(select.selectedIndex === 0, 'removing-selected-option-resets-selection');
            return failures.join('|');
        })()"#,
    );
    let Value::Str(failures) = result else {
        panic!("batch 69 forms contract must return diagnostics");
    };
    assert!(failures.is_empty(), "batch 69 forms failures: {failures}");
}
