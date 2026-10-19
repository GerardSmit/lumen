use super::*;
use lumen::embed::Value;
use lumen::Engine;
use lumen_runtime::Runtime;

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

#[test]
fn option_selectedness_survives_removal_insertion_and_moves() {
    let mut runtime = Runtime::new();
    let engine = runtime.engine();
    let realm = install(engine.ctx(), "<body><div id='outside'></div></body>", 128).unwrap();
    let result = eval(
        engine,
        r#"(() => {
          const failures = [];
          const check = (condition, name) => { if (!condition) failures.push(name); };
          const outside = document.getElementById('outside');

          const select = document.createElement('select');
          select.innerHTML = '<option id="move-a">A</option><optgroup><option id="move-b">B</option></optgroup><option id="move-c">C</option>';
          document.body.appendChild(select);
          const a = document.getElementById('move-a');
          const b = document.getElementById('move-b');
          const c = document.getElementById('move-c');
          check(a.selected && !b.selected && !c.selected, 'initial-first-option-default');
          document.body.moveBefore(a, null);
          check(a.selected && b.selected && !c.selected, 'move-out-keeps-option-and-select-falls-back');
          document.body.moveBefore(b, null);
          check(a.selected && b.selected && c.selected, 'second-move-keeps-both-detached-options');
          select.moveBefore(a, c);
          check(a.selected && b.selected && !c.selected, 'move-back-restores-option-selectedness');

          const same = document.createElement('select');
          same.innerHTML = '<option id="same-a">A</option><option id="same-b">B</option><option id="same-c" selected>C</option>';
          document.body.appendChild(same);
          const sameA = document.getElementById('same-a');
          const sameB = document.getElementById('same-b');
          const sameC = document.getElementById('same-c');
          same.moveBefore(sameC, sameA);
          same.moveBefore(sameB, sameA);
          check(same.selectedIndex === 0 && sameC.selected && !sameA.selected && !sameB.selected,
                'same-select-reorder-preserves-selectedness');
          let invalidReferenceRejected = false;
          try { same.insertBefore(sameC, document.getElementById('outside')); }
          catch (error) { invalidReferenceRejected = error.name === 'NotFoundError'; }
          check(invalidReferenceRejected && same.selectedIndex === 0 && sameC.selected,
                'failed-insert-does-not-change-selectedness');

          const nestedOuter = document.createElement('select');
          const outerOption = document.createElement('option');
          outerOption.textContent = 'outer';
          const nestedInner = document.createElement('select');
          const innerOption = document.createElement('option');
          innerOption.textContent = 'inner';
          nestedInner.appendChild(innerOption);
          nestedOuter.append(outerOption, nestedInner);
          document.body.appendChild(nestedOuter);
          outside.appendChild(nestedInner);
          check(outerOption.selected && nestedOuter.selectedIndex === 0 && innerOption.selected,
                'nested-select-options-retain-their-own-list-cache');

          const ordinary = document.createElement('select');
          ordinary.innerHTML = '<option id="ordinary-a">A</option><option id="ordinary-b">B</option>';
          document.body.appendChild(ordinary);
          const ordinaryA = document.getElementById('ordinary-a');
          const ordinaryB = document.getElementById('ordinary-b');
          outside.appendChild(ordinaryA);
          check(ordinaryA.selected && ordinaryB.selected, 'append-out-preserves-and-normalizes-source');
          ordinary.appendChild(ordinaryA);
          check(ordinaryA.selected && !ordinaryB.selected && ordinary.selectedIndex === 1 && ordinary.options[1] === ordinaryA,
                'append-back-applies-selectedness-to-new-owner');
          ordinary.removeChild(ordinaryA);
          check(ordinaryA.selected && ordinaryB.selected && ordinary.selectedIndex === 0,
                'remove-child-retains-detached-selection-and-fallback');

          const dirtySelect = document.createElement('select');
          dirtySelect.innerHTML = '<option id="dirty-first">First</option><option id="dirty-option" selected>Option</option>';
          document.body.appendChild(dirtySelect);
          const dirtyFirst = document.getElementById('dirty-first');
          const dirtyOption = document.getElementById('dirty-option');
          dirtyOption.selected = false;
          outside.appendChild(dirtyOption);
          dirtyOption.removeAttribute('selected');
          dirtyOption.setAttribute('selected', '');
          check(!dirtyOption.selected && dirtyFirst.selected,
                'dirty-selectedness-survives-owner-change-and-attribute-mutation');

          const fragmentSelect = document.createElement('select');
          fragmentSelect.innerHTML = '<option id="fragment-old">old</option>';
          document.body.appendChild(fragmentSelect);
          const fragmentOld = document.getElementById('fragment-old');
          const fragment = document.createDocumentFragment();
          const fragmentOption = document.createElement('option');
          fragmentOption.textContent = 'new';
          fragmentOption.selected = true;
          fragment.appendChild(fragmentOption);
          fragmentSelect.appendChild(fragment);
          check(fragmentOption.selected && !fragmentOld.selected && fragment.childNodes.length === 0,
                'drained-fragment-option-normalizes-target-select');

          const replaceTarget = document.createElement('select');
          replaceTarget.innerHTML = '<option id="replace-a">A</option><option id="replace-old">old</option>';
          const donor = document.createElement('select');
          donor.innerHTML = '<option id="replace-new">new</option><option id="replace-fallback">fallback</option>';
          document.body.appendChild(replaceTarget);
          document.body.appendChild(donor);
          const replaceA = document.getElementById('replace-a');
          const replaceOld = document.getElementById('replace-old');
          const replaceNew = document.getElementById('replace-new');
          const replaceFallback = document.getElementById('replace-fallback');
          replaceTarget.replaceChild(replaceNew, replaceOld);
          check(replaceNew.selected && !replaceA.selected && replaceFallback.selected,
                'replace-child-normalizes-both-select-lists');

          const adoptionDocument = document.implementation.createHTMLDocument('selectedness adoption');
          adoptionDocument.body.innerHTML = '<select id="adopt-select"><option id="adopted-option">adopted</option><option id="adopt-fallback">fallback</option></select>';
          const adoptionSelect = adoptionDocument.getElementById('adopt-select');
          const adoptedOption = adoptionDocument.getElementById('adopted-option');
          const adoptFallback = adoptionDocument.getElementById('adopt-fallback');
          const adoptedIdentity = document.adoptNode(adoptedOption);
          check(adoptedIdentity === adoptedOption && adoptedOption.ownerDocument === document &&
                adoptedOption.selected && adoptFallback.selected && adoptionSelect.selectedIndex === 0,
                'adopt-node-preserves-detached-option-and-normalizes-donor');

          return failures.join('|');
        })()"#,
    );
    match result {
        Value::Str(failures) => assert!(failures.is_empty(), "selectedness failures: {failures}"),
        _ => panic!("selectedness diagnostics returned an unexpected value"),
    }
    drop(realm);
}
