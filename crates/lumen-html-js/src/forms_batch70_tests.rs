//! Focused native checks for form control reflection and select tree mutations.

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
            panic!("batch 70 forms contract threw: {message}");
        }
    }
}

#[test]
fn forms_batch70_select_markup_mutations_and_input_placeholder_reflection() {
    let mut runtime = Runtime::new();
    let engine = runtime.engine();
    crate::install(
        engine.ctx(),
        "<input id='placeholder'><select id='parsed'><option id='parser-first' selected>First</option><option id='parser-middle'>Middle</option><option id='parser-last' selected>Second</option></select>",
        64,
    )
    .expect("install HTML document");

    let result = eval(
        engine,
        r#"(() => {
            const failures = [];
            const check = (condition, name) => { if (!condition) failures.push(name); };

            const input = document.getElementById('placeholder');
            check(input.placeholder === '', 'input-placeholder-default');
            input.placeholder = 'first\r\nsecond';
            check(input.getAttribute('placeholder') === 'first\r\nsecond' &&
                input.placeholder === 'first\r\nsecond', 'input-placeholder-reflects-literal');

            const parsed = document.getElementById('parsed');
            const parserFirst = document.getElementById('parser-first');
            const parserMiddle = document.getElementById('parser-middle');
            const parserLast = document.getElementById('parser-last');
            check(parsed.selectedIndex === 2 && parsed.value === 'Second' &&
                parsed.selectedOptions.length === 1 && parsed.selectedOptions[0] === parserLast &&
                !parserFirst.selected && !parserMiddle.selected && parserLast.selected &&
                parserFirst.defaultSelected && parserLast.defaultSelected,
                'parser-single-select-keeps-last-without-mutating-selected-attributes');
            const checkedOptions = parsed.querySelectorAll('option:checked');
            check(checkedOptions.length === 1 && checkedOptions[0] === parserLast,
                'checked-pseudo-shares-cached-single-select-resolution');

            const select = document.createElement('select');
            document.body.append(select);
            select.innerHTML = '<option selected>A</option><option>keep</option>';
            select.selectedIndex = -1;
            select.options.length = 2;
            select.title = 'unrelated';
            check(select.selectedIndex === -1,
                'explicit-empty-selection-survives-noop-length-and-unrelated-attribute');

            select.insertAdjacentHTML('beforeend',
                '<option selected>C</option><option selected>D</option>');
            check(select.selectedIndex === 3 && select.options[3].text === 'D' &&
                !select.options[2].selected,
                'insert-adjacent-html-keeps-last-selected-option');

            select.options[1].outerHTML =
                '<option selected>E</option><option selected>F</option>';
            check(select.selectedIndex === 2 && select.options[2].text === 'F' &&
                !select.options[1].selected,
                'outer-html-replacement-keeps-last-selected-option');

            const multiple = document.createElement('select');
            multiple.multiple = true;
            multiple.innerHTML = '<option selected>First</option><option selected>Last</option>';
            check(multiple.selectedOptions.length === 2 &&
                multiple.options[0].selected && multiple.options[1].selected,
                'multiple-select-keeps-each-selected-option');

            const selected = select.options[2];
            selected.remove();
            check(select.selectedIndex === 0,
                'removing-selected-option-runs-list-reset-algorithm');
            return failures.join('|');
        })()"#,
    );
    let Value::Str(failures) = result else {
        panic!("batch 70 forms contract must return diagnostics");
    };
    assert!(failures.is_empty(), "batch 70 forms failures: {failures}");
}
