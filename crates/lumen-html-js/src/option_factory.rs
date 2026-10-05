//! The `Option()` legacy factory, separate from the `HTMLOptionElement`
//! interface constructor. It creates the element in the current Window
//! realm's associated Document and returns that node's canonical wrapper.
use super::*;

#[lumen_bind::op(name = "Option", coerce)]
fn construct_option(
    ctx: &mut Ctx,
    text: lumen_bind::Passed<Value>,
    value: lumen_bind::Passed<Value>,
    #[default(false)] default_selected: bool,
    #[default(false)] selected: bool,
) -> OpResult<Value> {
    if !ctx.is_constructing() {
        return Err(OpError::new(
            "TypeError",
            "Option constructor must be called with 'new'",
        ));
    }

    // DOMString conversion can run author code. Finish every conversion before
    // allocating a node or borrowing the shared Document.
    let text = optional_string(ctx, text)?;
    let value = optional_string(ctx, value)?;
    let realm = window_globals::current_dom_realm(ctx)
        .ok_or_else(|| OpError::new("TypeError", "Option has no associated document"))?;

    let node = {
        let mut session = realm.session.borrow_mut();
        let document = session.document_mut();
        let node = document
            .create(NodeKind::Element {
                namespace: Namespace::Html,
                name: "option".into(),
                attributes: Vec::new(),
            })
            .map_err(super::dom_error)?;
        let initialized = (|| {
            if let Some(text) = text.as_deref().filter(|text| !text.is_empty()) {
                let text_node = document.create(NodeKind::Text(text.to_owned()))?;
                if let Err(error) = document.append(node, text_node) {
                    let _ = document.destroy_subtree(text_node);
                    return Err(error);
                }
            }
            if let Some(value) = value.as_deref() {
                document.set_attribute_ns(node, None, "value", value)?;
            }
            if default_selected {
                document.set_attribute_ns(node, None, "selected", "")?;
            }
            Ok::<(), lumen_html::Error>(())
        })();
        if let Err(error) = initialized {
            let _ = document.destroy_subtree(node);
            return Err(super::dom_error(error));
        }
        node
    };

    forms::initialize_option_selectedness(&realm, node, selected)?;
    Ok(realm.wrap(ctx, node))
}

pub(crate) fn optional_string(
    ctx: &mut Ctx,
    value: lumen_bind::Passed<Value>,
) -> OpResult<Option<String>> {
    match value.0 {
        None | Some(Value::Undefined) => Ok(None),
        Some(value) => ctx
            .coerce_string(&value)
            .map(|value| Some(value.to_string()))
            .map_err(OpError::thrown),
    }
}

pub(crate) fn install(ctx: &mut Ctx) -> OpResult<()> {
    let interface_constructor = ctx.class_constructor::<DomOptionElement>();
    let prototype = ctx
        .get_member(&interface_constructor, "prototype")
        .map_err(|error| OpError::thrown(lumen::embed::abrupt_value(error)))?;
    let constructor_key = Value::str("constructor");
    let constructor_descriptor = ctx
        .reflect_get_own_property_descriptor(&prototype, &constructor_key)
        .map_err(OpError::thrown)?;
    let factory = ctx.bound_function(&lumen_bind::FnItem::of::<construct_option::Op>());
    ctx.set_constructor_prototype(&factory, &prototype);
    if !matches!(constructor_descriptor, Value::Undefined) {
        // The helper also wires prototype.constructor. Option.prototype is the
        // interface prototype, whose constructor must remain HTMLOptionElement.
        ctx.define_property_value(&prototype, constructor_key, &constructor_descriptor)
            .map_err(OpError::thrown)?;
    }
    crate::install_interface(ctx, &ctx.global_object(), "Option", factory).map_err(OpError::thrown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

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
                    .unwrap_or_else(|| "script threw an unknown error".into());
                panic!("{message}");
            }
        }
    }

    #[test]
    fn legacy_option_factory_constructs_cleanly_in_the_current_realm() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<div></div>", 32).unwrap();
        let result = eval(
            &mut engine,
            r#"(() => {
                const htmlOptionConstructor = HTMLOptionElement.prototype.constructor;
                if (Option.prototype !== HTMLOptionElement.prototype ||
                    htmlOptionConstructor !== HTMLOptionElement || Option.length !== 0)
                    throw new Error('factory interface linkage');
                const descriptor = Object.getOwnPropertyDescriptor(
                    HTMLOptionElement.prototype, 'constructor');
                if (descriptor.value !== HTMLOptionElement || !descriptor.writable ||
                    descriptor.enumerable || !descriptor.configurable)
                    throw new Error('interface constructor descriptor changed');

                let coercions = 0;
                let noNew;
                try { Option({ toString() { coercions++; return 'wrong'; } }); }
                catch (error) { noNew = error; }
                if (!(noNew instanceof TypeError) || coercions !== 0)
                    throw new Error('call without new');

                const absent = new Option();
                const undefinedArguments = new Option(undefined, undefined);
                const emptyArguments = new Option('', '');
                const falseArguments = new Option(false, false);
                if (!(absent instanceof HTMLOptionElement) || absent.hasChildNodes() ||
                    absent.hasAttribute('value') || absent.selected ||
                    undefinedArguments.hasChildNodes() || undefinedArguments.hasAttribute('value') ||
                    emptyArguments.hasChildNodes() || !emptyArguments.hasAttribute('value') ||
                    emptyArguments.value !== '' || falseArguments.textContent !== 'false' ||
                    falseArguments.value !== 'false')
                    throw new Error('optional argument conversion');

                const select = document.createElement('select');
                const multipleSelect = document.createElement('select');
                multipleSelect.multiple = true;
                if (!multipleSelect.multiple || !multipleSelect.hasAttribute('multiple'))
                    throw new Error('multiple reflects its boolean content attribute');
                multipleSelect.multiple = false;
                if (multipleSelect.multiple || multipleSelect.hasAttribute('multiple'))
                    throw new Error('multiple=false removes its content attribute');

                const first = new Option('first');
                const second = new Option('second');
                select.append(first, second);
                const selectedOptions = select.selectedOptions;
                if (select.selectedOptions !== selectedOptions || select.selectedIndex !== 0 ||
                    selectedOptions.length !== 1 ||
                    selectedOptions[0] !== first)
                    throw new Error('single-select fallback');
                second.setAttribute('selected', '');
                if (select.selectedIndex !== 1 || selectedOptions.length !== 1 ||
                    selectedOptions[0] !== second)
                    throw new Error('clean selected attribute set');
                second.removeAttribute('selected');
                if (select.selectedIndex !== 0 || selectedOptions.length !== 1 ||
                    selectedOptions[0] !== first)
                    throw new Error('clean selected attribute removal');

                const defaultOnly = new Option('default', 'value', true);
                if (defaultOnly.getAttribute('selected') !== '' || defaultOnly.selected)
                    throw new Error('defaultSelected is separate from selectedness');
                const selectedOnly = new Option('selected', undefined, false, true);
                if (!selectedOnly.selected || selectedOnly.hasAttribute('selected'))
                    throw new Error('selected argument does not reflect attribute');
                selectedOnly.setAttribute('selected', '');
                if (!selectedOnly.selected)
                    throw new Error('clean constructor selectedness follows selected attribute set');
                selectedOnly.removeAttribute('selected');
                if (selectedOnly.selected)
                    throw new Error('clean constructor selectedness follows selected attribute removal');

                const dirtySelected = new Option('dirty', undefined, false, true);
                dirtySelected.selected = true;
                dirtySelected.setAttribute('selected', '');
                dirtySelected.removeAttribute('selected');
                if (!dirtySelected.selected)
                    throw new Error('IDL selectedness remains dirty after attribute changes');
                return true;
            })()"#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn option_text_and_ownership_follow_mutations_and_namespace_boundaries() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<form><select></select></form>", 128).unwrap();
        let result = eval(
            &mut engine,
            r#"(() => {
            const option = new Option();
            const script = document.createElementNS('http://www.w3.org/2000/svg', 'script');
            script.textContent = ' excluded ';
            const mathScript = document.createElementNS('http://www.w3.org/1998/Math/MathML', 'script');
            mathScript.textContent = ' included ';
            option.append(' \tbefore\n', script, mathScript, ' after\r\u00a0 ');
            if (option.text !== 'before included after \u00a0' || option.value !== option.text || option.label !== option.text)
                throw new Error('shared option text or non-ASCII whitespace');
            option.label = '';
            if (option.label !== '' || option.text === '') throw new Error('empty label reflection');
            option.text = ' replacement ';
            if (option.childNodes.length !== 1 || option.textContent !== ' replacement ' || option.text !== 'replacement')
                throw new Error('text setter replacement');
            const select = document.querySelector('select');
            const first = new Option('first');
            select.append(first, option);
            if (option.index !== 1 || option.form !== select.form) throw new Error('live select ownership');
            const list = document.createElement('datalist');
            select.append(list);
            list.append(option);
            if (option.index !== 0 || option.form !== null) throw new Error('datalist boundary');
            const group = document.createElement('optgroup');
            const nested = document.createElement('optgroup');
            select.append(group);
            group.append(nested);
            nested.append(option);
            if (option.index !== 0 || option.form !== null) throw new Error('nested optgroup boundary');
            select.append(option);
            return option.index === 1 && option.form === select.form;
        })()"#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn multiple_select_reset_preserves_no_selection() {
        let mut engine = Engine::new();
        crate::install(engine.ctx(), "<div></div>", 32).unwrap();
        let result = eval(
            &mut engine,
            r#"(() => {
                const form = document.createElement('form');
                const text = document.createElement('input');
                text.type = 'text';
                const checkbox = document.createElement('input');
                checkbox.type = 'checkbox';
                const select = document.createElement('select');
                select.multiple = true;
                const option = document.createElement('option');
                option.value = 'option';
                const textarea = document.createElement('textarea');
                form.append(text, checkbox, textarea, select);
                select.append(option);

                text.defaultValue = 'text default';
                checkbox.defaultChecked = true;
                option.defaultSelected = true;
                textarea.defaultValue = 'textarea default';
                text.value = 'text new value';
                checkbox.checked = false;
                option.selected = false;
                textarea.value = 'textarea new value';
                form.reset();
                if (text.value !== 'text default' || checkbox.checked !== true ||
                    option.selected !== true || select.selectedIndex !== 0 ||
                    textarea.value !== 'textarea default')
                    throw new Error('first reset did not restore defaults');

                text.defaultValue = 'text new default';
                checkbox.defaultChecked = false;
                option.defaultSelected = false;
                textarea.defaultValue = 'textarea new default';
                if (text.value !== 'text new default' || checkbox.checked !== false ||
                    option.selected !== false || select.selectedIndex !== -1 ||
                    textarea.value !== 'textarea new default')
                    throw new Error('default reflection after reset');
                form.reset();
                if (text.value !== 'text new default' || checkbox.checked !== false ||
                    option.selected !== false || select.selectedIndex !== -1 ||
                    textarea.value !== 'textarea new default')
                    throw new Error('second reset restored stale selectedness');
                return true;
            })()"#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }
}
