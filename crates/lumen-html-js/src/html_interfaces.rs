//! Distinct native brands for HTML element interfaces backed only by inherited
//! HTMLElement behavior. Their interface-specific attributes and methods remain
//! separate work where they are not already implemented by another subsystem.
use super::{DomElement, DomHtmlElement, DomNode};
use lumen::embed::{Ctx, OpResult, Value, WeakValue};
use lumen_html::NodeId;
use std::vec::Vec;

macro_rules! define_plain_html_interfaces {
    ($( $ty:ident, $interface:literal => [$($tag:literal),+] $( { $($members:tt)* } )?; )+) => {
        $(
            #[lumen_bind::class(
                name = $interface,
                extends = super::DomHtmlElement,
                hint(js(webidl))
            )]
            pub(crate) struct $ty {
                pub(crate) base: super::DomHtmlElement,
            }

            #[lumen_bind::methods]
            impl $ty {
                $($($members)*)?
            }
        )+

        pub(crate) fn wrap_known(
            ctx: &mut Ctx,
            id: NodeId,
            node: DomNode,
            local_name: &str,
        ) -> Result<Value, DomNode> {
            match local_name {
                $(
                    $($tag)|+ => Ok(ctx.cached_instance(id, || $ty {
                        base: super::DomHtmlElement {
                            base: super::DomElement { base: node },
                        },
                    })),
                )+
                "output" => Ok(ctx.cached_instance(id, || DomHtmlOutputElement {
                    base: super::DomHtmlElement {
                        base: super::DomElement { base: node },
                    },
                })),
                _ => Err(node),
            }
        }

        pub(crate) fn constructors(ctx: &mut Ctx) -> Vec<(&'static str, Value)> {
            let mut constructors = Vec::with_capacity([$(stringify!($ty)),+].len() + 1);
            $(constructors.push(($interface, ctx.class_constructor::<$ty>()));)+
            constructors.push((
                "HTMLOutputElement",
                ctx.class_constructor::<DomHtmlOutputElement>(),
            ));
            constructors.push((
                "HTMLUnknownElement",
                ctx.class_constructor::<DomHtmlUnknownElement>(),
            ));
            constructors
        }
    };
}

define_plain_html_interfaces! {
    DomHtmlButtonElement, "HTMLButtonElement" => ["button"] {
        #[getter(name = "type")]
        fn button_type(&self) -> String {
            let node = &self.base.base.base;
            let session = node.realm.session.borrow();
            let state = lumen_html::forms::button_type_state(session.document(), node.id)
                .unwrap_or(lumen_html::forms::ButtonTypeState::Submit);
            match state {
                lumen_html::forms::ButtonTypeState::Submit => "submit",
                lumen_html::forms::ButtonTypeState::Reset => "reset",
                lumen_html::forms::ButtonTypeState::Button => "button",
            }
            .to_owned()
        }

        #[setter(name = "type", coerce)]
        fn set_button_type(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("type", value)
        }

        #[getter]
        fn labels(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
            let node = &self.base.base.base;
            super::labels::control_labels(
                ctx,
                &node.realm,
                node.id,
                this.0,
                &node.collections,
            )
        }

        #[getter]
        fn form(&self, ctx: &mut Ctx) -> Value {
            let node = &self.base.base.base;
            super::forms::form_owner_value(ctx, &node.realm, node.id)
        }

        #[getter(name = "formAction")]
        fn form_action(&self) -> OpResult<String> {
            form_action_value(&self.base.base.base)
        }

        #[setter(name = "formAction", coerce)]
        fn set_form_action(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("formaction", value)
        }

        #[getter(name = "formEnctype")]
        fn form_enctype(&self) -> OpResult<String> {
            form_enctype_value(&self.base.base.base)
        }

        #[setter(name = "formEnctype", coerce)]
        fn set_form_enctype(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("formenctype", value)
        }

        #[getter(name = "formMethod")]
        fn form_method(&self) -> OpResult<String> {
            form_method_value(&self.base.base.base)
        }

        #[setter(name = "formMethod", coerce)]
        fn set_form_method(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("formmethod", value)
        }

        #[getter(name = "formNoValidate")]
        fn form_no_validate(&self) -> OpResult<bool> {
            self.base.base.base.has_null_attribute("formnovalidate")
        }

        #[setter(name = "formNoValidate", coerce)]
        fn set_form_no_validate(&self, value: bool) -> OpResult<()> {
            let node = &self.base.base.base;
            if value {
                node.set_attribute_core("formnovalidate", "")
            } else {
                node.remove_attribute_core("formnovalidate")
            }
        }

        #[getter(name = "formTarget")]
        fn form_target(&self) -> OpResult<String> {
            Ok(self
                .base
                .base
                .base
                .get_null_attribute("formtarget")?
                .unwrap_or_default())
        }

        #[setter(name = "formTarget", coerce)]
        fn set_form_target(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("formtarget", value)
        }
    };
    DomHtmlTableCaptionElement, "HTMLTableCaptionElement" => ["caption"];
    DomHtmlTableColElement, "HTMLTableColElement" => ["col", "colgroup"];
    DomHtmlDataElement, "HTMLDataElement" => ["data"];
    DomHtmlDataListElement, "HTMLDataListElement" => ["datalist"] {
        #[getter]
        fn options(
            &self,
            ctx: &mut Ctx,
            this: lumen_bind::This<Value>,
        ) -> OpResult<Value> {
            let node = &self.base.base.base;
            if let Some(value) = node
                .collections
                .borrow()
                .get("options")
                .and_then(WeakValue::upgrade)
            {
                return Ok(value);
            }
            let value = ctx.new_instance(super::collections::DomHtmlCollection {
                base: super::collections::DomNodeList::datalist_options(
                    node.realm.clone(),
                    node.id,
                    this.0,
                ),
            });
            node.collections.borrow_mut().insert(
                "options".into(),
                ctx.weak_value(&value).expect("datalist options collection"),
            );
            Ok(value)
        }
    };
    DomHtmlDialogElement, "HTMLDialogElement" => ["dialog"] {
        #[getter]
        fn open(&self) -> OpResult<bool> {
            let node = &self.base.base.base;
            super::dialog_popover::dialog_open(&node.realm, node.id)
        }

        #[setter]
        fn set_open(&self, value: bool) -> OpResult<()> {
            let node = &self.base.base.base;
            super::dialog_popover::set_dialog_open(&node.realm, node.id, value)
        }

        #[getter(name = "returnValue")]
        fn return_value(&self) -> OpResult<String> {
            let node = &self.base.base.base;
            super::dialog_popover::dialog_return_value(&node.realm, node.id)
        }

        #[setter(name = "returnValue", coerce)]
        fn set_return_value(&self, value: &str) -> OpResult<()> {
            let node = &self.base.base.base;
            super::dialog_popover::set_dialog_return_value(&node.realm, node.id, value)
        }

        #[method(name = "show")]
        fn show(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<()> {
            let (realm, node) = ctx.with_instance::<DomHtmlDialogElement, _>(&this.0, |element| {
                let node = &element.base.base.base;
                node.realm.resolve_adopted_node(node.id)
            })?;
            super::dialog_popover::show_dialog(
                ctx,
                &realm,
                node,
                lumen_html::top_layer::DialogMode::NonModal,
            )
        }

        #[method(name = "showModal")]
        fn show_modal(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<()> {
            let (realm, node) = ctx.with_instance::<DomHtmlDialogElement, _>(&this.0, |element| {
                let node = &element.base.base.base;
                node.realm.resolve_adopted_node(node.id)
            })?;
            super::dialog_popover::show_dialog(
                ctx,
                &realm,
                node,
                lumen_html::top_layer::DialogMode::Modal,
            )
        }

        #[method(name = "close")]
        fn close(
            ctx: &mut Ctx,
            this: lumen_bind::This<Value>,
            result: lumen_bind::Passed<Value>,
        ) -> OpResult<()> {
            let result = super::option_factory::optional_string(ctx, result)?;
            let (realm, node) = ctx.with_instance::<DomHtmlDialogElement, _>(&this.0, |element| {
                let node = &element.base.base.base;
                node.realm.resolve_adopted_node(node.id)
            })?;
            super::dialog_popover::close_dialog(ctx, &realm, node, result.as_deref())
        }
    };
    DomHtmlModElement, "HTMLModElement" => ["del", "ins"];
    DomHtmlDirectoryElement, "HTMLDirectoryElement" => ["dir"];
    DomHtmlDListElement, "HTMLDListElement" => ["dl"];
    DomHtmlEmbedElement, "HTMLEmbedElement" => ["embed"];
    DomHtmlFieldSetElement, "HTMLFieldSetElement" => ["fieldset"] {
        #[getter(name = "type")]
        fn fieldset_type(&self) -> String {
            "fieldset".into()
        }

        #[getter]
        fn form(&self, ctx: &mut Ctx) -> Value {
            let node = &self.base.base.base;
            super::forms::form_owner_value(ctx, &node.realm, node.id)
        }

        #[getter]
        fn elements(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
            let node = &self.base.base.base;
            if let Some(value) = node
                .collections
                .borrow()
                .get("fieldsetElements")
                .and_then(WeakValue::upgrade)
            {
                return value;
            }
            let value = ctx.new_instance(super::collections::DomHtmlCollection {
                base: super::collections::DomNodeList::fieldset_elements(
                    node.realm.clone(),
                    node.id,
                    this.0,
                ),
            });
            node.collections.borrow_mut().insert(
                "fieldsetElements".into(),
                ctx.weak_value(&value).expect("fieldset elements collection"),
            );
            value
        }
    };
    DomHtmlFontElement, "HTMLFontElement" => ["font"];
    DomHtmlFrameElement, "HTMLFrameElement" => ["frame"];
    DomHtmlFrameSetElement, "HTMLFrameSetElement" => ["frameset"];
    DomHtmlHeadingElement, "HTMLHeadingElement" => ["h1", "h2", "h3", "h4", "h5", "h6"];
    DomHtmlHrElement, "HTMLHRElement" => ["hr"];
    DomHtmlLabelElement, "HTMLLabelElement" => ["label"] {
        #[getter(name = "htmlFor")]
        fn html_for(&self) -> OpResult<String> {
            Ok(self
                .base
                .base
                .base
                .get_null_attribute("for")?
                .unwrap_or_default())
        }

        #[setter(name = "htmlFor", coerce)]
        fn set_html_for(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("for", value)
        }

        #[getter]
        fn form(&self, ctx: &mut Ctx) -> OpResult<Value> {
            let node = &self.base.base.base;
            super::labels::label_form(ctx, &node.realm, node.id)
        }

        #[getter]
        fn control(&self, ctx: &mut Ctx) -> OpResult<Value> {
            let node = &self.base.base.base;
            super::labels::label_control(ctx, &node.realm, node.id)
        }
    };
    DomHtmlLegendElement, "HTMLLegendElement" => ["legend"] {
        #[getter]
        fn form(&self, ctx: &mut Ctx) -> OpResult<Value> {
            let node = &self.base.base.base;
            let fieldset = {
                let session = node.realm.session.borrow();
                let document = session.document();
                let mut parent = document.parent(node.id).map_err(super::dom_error)?;
                let mut found = None;
                while let Some(candidate) = parent {
                    if lumen_html::forms::html_element_local_name(document, candidate)
                        == Some("fieldset")
                    {
                        found = Some(candidate);
                        break;
                    }
                    parent = document.parent(candidate).map_err(super::dom_error)?;
                }
                found
            };
            Ok(fieldset.map_or(Value::Null, |fieldset| {
                super::forms::form_owner_value(ctx, &node.realm, fieldset)
            }))
        }
    };
    DomHtmlLiElement, "HTMLLIElement" => ["li"];
    DomHtmlMapElement, "HTMLMapElement" => ["map"];
    DomHtmlMetaElement, "HTMLMetaElement" => ["meta"];
    DomHtmlMeterElement, "HTMLMeterElement" => ["meter"] {
        #[getter]
        fn labels(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
            let node = &self.base.base.base;
            super::labels::control_labels(
                ctx,
                &node.realm,
                node.id,
                this.0,
                &node.collections,
            )
        }
    };
    DomHtmlObjectElement, "HTMLObjectElement" => ["object"] {
        #[getter]
        fn form(&self, ctx: &mut Ctx) -> Value {
            let node = &self.base.base.base;
            super::forms::form_owner_value(ctx, &node.realm, node.id)
        }
    };
    DomHtmlOListElement, "HTMLOListElement" => ["ol"];
    DomHtmlOptGroupElement, "HTMLOptGroupElement" => ["optgroup"];
    DomHtmlParagraphElement, "HTMLParagraphElement" => ["p"];
    DomHtmlParamElement, "HTMLParamElement" => ["param"];
    DomHtmlPreElement, "HTMLPreElement" => ["listing", "pre", "xmp"];
    DomHtmlProgressElement, "HTMLProgressElement" => ["progress"] {
        #[getter]
        fn labels(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
            let node = &self.base.base.base;
            super::labels::control_labels(
                ctx,
                &node.realm,
                node.id,
                this.0,
                &node.collections,
            )
        }
    };
    DomHtmlQuoteElement, "HTMLQuoteElement" => ["blockquote", "q"];
    DomHtmlSourceElement, "HTMLSourceElement" => ["source"];
    DomHtmlSpanElement, "HTMLSpanElement" => ["span"];
    DomHtmlTableElement, "HTMLTableElement" => ["table"];
    DomHtmlTableSectionElement, "HTMLTableSectionElement" => ["tbody", "tfoot", "thead"];
    DomHtmlTableCellElement, "HTMLTableCellElement" => ["td", "th"];
    DomHtmlTableRowElement, "HTMLTableRowElement" => ["tr"];
    DomHtmlTimeElement, "HTMLTimeElement" => ["time"];
    DomHtmlTrackElement, "HTMLTrackElement" => ["track"];
    DomHtmlUListElement, "HTMLUListElement" => ["ul"];
}

pub(crate) fn form_action_value(node: &DomNode) -> OpResult<String> {
    action_attribute_value(node, "formaction")
}

pub(crate) fn action_attribute_value(node: &DomNode, attribute: &str) -> OpResult<String> {
    let Some(action) = node.get_null_attribute(attribute)? else {
        return Ok(node
            .realm
            .document_url()
            .unwrap_or_else(|| "about:blank".into()));
    };
    if action.is_empty() {
        return Ok(node
            .realm
            .document_url()
            .unwrap_or_else(|| "about:blank".into()));
    }
    let base = node.realm.base_url();
    Ok(lumen_common::url::parse(&action, Some(&base))
        .map(|url| url.href())
        .unwrap_or(action))
}

pub(crate) fn form_method_value(node: &DomNode) -> OpResult<String> {
    let value = node.get_null_attribute("formmethod")?.unwrap_or_default();
    Ok(lumen_html::forms::normalized_form_method(&value).into())
}

pub(crate) fn form_enctype_value(node: &DomNode) -> OpResult<String> {
    let value = node.get_null_attribute("formenctype")?.unwrap_or_default();
    Ok(lumen_html::forms::normalized_form_enctype(&value).into())
}

#[lumen_bind::class(
    name = "HTMLOutputElement",
    extends = super::DomHtmlElement,
    hint(js(webidl))
)]
pub(crate) struct DomHtmlOutputElement {
    pub(crate) base: super::DomHtmlElement,
}

#[lumen_bind::methods]
impl DomHtmlOutputElement {
    #[getter]
    fn labels(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> {
        let node = &self.base.base.base;
        super::labels::control_labels(ctx, &node.realm, node.id, this.0, &node.collections)
    }

    #[getter(name = "type")]
    fn output_type(&self) -> String {
        "output".into()
    }

    #[getter]
    fn value(&self) -> OpResult<String> {
        let node = &self.base.base.base;
        super::forms::output_value(&node.realm, node.id)
    }

    #[setter(coerce)]
    fn set_value(&self, value: &str) -> OpResult<()> {
        let node = &self.base.base.base;
        super::forms::set_output_value(&node.realm, node.id, value)
    }

    #[getter(name = "defaultValue")]
    fn default_value(&self) -> OpResult<String> {
        let node = &self.base.base.base;
        super::forms::output_default_value(&node.realm, &node.realm.forms.borrow(), node.id)
    }

    #[setter(name = "defaultValue", coerce)]
    fn set_default_value(&self, value: &str) -> OpResult<()> {
        let node = &self.base.base.base;
        super::forms::set_output_default_value(&node.realm, node.id, value)
    }

    #[getter]
    fn name(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_null_attribute("name")?
            .unwrap_or_default())
    }

    #[setter(coerce)]
    fn set_name(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("name", value)
    }

    #[getter]
    fn form(&self, ctx: &mut Ctx) -> Value {
        let node = &self.base.base.base;
        super::forms::form_owner_value(ctx, &node.realm, node.id)
    }
}

#[lumen_bind::class(
    name = "HTMLUnknownElement",
    extends = super::DomHtmlElement,
    hint(js(webidl))
)]
pub(crate) struct DomHtmlUnknownElement {
    pub(crate) base: super::DomHtmlElement,
}

#[lumen_bind::methods]
impl DomHtmlUnknownElement {}

pub(crate) fn wrap_unknown(ctx: &mut Ctx, id: NodeId, node: DomNode) -> Value {
    ctx.cached_instance(id, || DomHtmlUnknownElement {
        base: DomHtmlElement {
            base: DomElement { base: node },
        },
    })
}

#[cfg(test)]
mod tests {
    use lumen::embed::Value;
    use lumen::Engine;

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
                panic!("HTML interface regression threw: {message}");
            }
        }
    }

    #[test]
    fn output_value_default_override_and_form_reset_are_live() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<form><output id='out'>seed</output><select><option>A</option><option>B</option></select><button id='reset' type='reset'>Reset</button><input id='reset-input' type='reset'></form>",
            128,
        )
        .unwrap();
        let result = eval(
            &mut engine,
            r#"(() => {
                    const failures = [];
                    const check = (ok, name) => { if (!ok) failures.push(name); };
                    const form = document.querySelector('form');
                    const output = document.querySelector('output');
                    const select = document.querySelector('select');
                    const options = select.options;
                    const listbox = document.createElement('select');
                    listbox.setAttribute('size', '2');
                    const listboxOption = document.createElement('option');
                    listboxOption.textContent = 'Listbox';
                    listbox.appendChild(listboxOption);
                    form.appendChild(listbox);
                    const reset = document.querySelector('#reset');
                    const inputReset = document.querySelector('#reset-input');
                    check(output instanceof HTMLOutputElement && output.type === 'output', 'brand');
                    check(output.value === 'seed' && output.defaultValue === 'seed', 'initial-default');
                    output.value = 'live';
                    check(output.value === 'live' && output.textContent === 'live' && output.defaultValue === 'seed', 'value-override');
                    output.defaultValue = 'next';
                    check(output.value === 'live' && output.defaultValue === 'next' && output.textContent === 'live', 'override-default-write');
                    options[1].selected = true;
                    HTMLFormElement.prototype.reset.call(form);
                    check(output.value === 'next' && output.defaultValue === 'next' && output.textContent === 'next', 'form-reset-output');
                    check(options[0].selected && !options[1].selected && select.selectedIndex === 0, 'single-select-reset-fallback');
                    check(listbox.selectedIndex === -1 && !listboxOption.selected, 'listbox-no-single-select-fallback');
                    output.value = 'changed';
                    let clickSeen = false;
                    reset.addEventListener('click', event => {
                        clickSeen = event instanceof PointerEvent && event.target === reset &&
                            event.view === window && !event.isTrusted && event.bubbles &&
                            event.cancelable && event.composed;
                    }, { once: true });
                    reset.click();
                    check(clickSeen && output.value === 'next', 'button-reset-click');
                    output.value = 'blocked';
                    const cancel = event => event.preventDefault();
                    reset.addEventListener('click', cancel);
                    reset.click();
                    check(output.value === 'blocked', 'canceled-click');
                    reset.removeEventListener('click', cancel);
                    output.value = 'input-click';
                    inputReset.click();
                    check(output.value === 'next', 'input-reset-click');

                    const checkbox = document.createElement('input');
                    checkbox.type = 'checkbox';
                    form.appendChild(checkbox);
                    checkbox.indeterminate = true;
                    let recursiveClicks = 0;
                    checkbox.addEventListener('click', event => {
                        recursiveClicks++;
                        checkbox.click();
                        event.preventDefault();
                    }, { once: true });
                    checkbox.click();
                    check(recursiveClicks === 1 && !checkbox.checked && checkbox.indeterminate,
                        'checkbox-canceled-click-restores-indeterminate-and-blocks-reentry');

                    checkbox.indeterminate = false;
                    checkbox.addEventListener('click', event => {
                        checkbox.indeterminate = true;
                        event.preventDefault();
                    }, { once: true });
                    checkbox.click();
                    check(!checkbox.checked && !checkbox.indeterminate,
                        'checkbox-canceled-click-restores-original-false-indeterminate');

                    const firstRadio = document.createElement('input');
                    const clickedRadio = document.createElement('input');
                    firstRadio.type = clickedRadio.type = 'radio';
                    firstRadio.name = clickedRadio.name = 'choice';
                    firstRadio.checked = true;
                    form.append(firstRadio, clickedRadio);
                    clickedRadio.addEventListener('click', event => {
                        firstRadio.name = 'other-choice';
                        event.preventDefault();
                    }, { once: true });
                    clickedRadio.click();
                    check(!firstRadio.checked && !clickedRadio.checked,
                        'canceled-radio-click-does-not-restore-a-node-outside-the-current-group');

                    const alreadyCheckedRadio = document.createElement('input');
                    alreadyCheckedRadio.type = 'radio';
                    alreadyCheckedRadio.checked = true;
                    form.appendChild(alreadyCheckedRadio);
                    alreadyCheckedRadio.addEventListener('click', event => event.preventDefault(), { once: true });
                    alreadyCheckedRadio.click();
                    check(alreadyCheckedRadio.checked,
                        'canceled-click-restores-the-already-checked-radio');

                    const invalidTypeButton = document.createElement('button');
                    invalidTypeButton.setAttribute('type', 'invalid');
                    form.appendChild(invalidTypeButton);
                    let submitEvents = 0;
                    form.addEventListener('submit', event => {
                        submitEvents++;
                        event.preventDefault();
                    }, { once: true });
                    invalidTypeButton.click();
                    check(submitEvents === 1, 'invalid-button-type-uses-submit-activation');
                    return failures.join('|');
                })()"#,
        );
        let Value::Str(failures) = result else {
            panic!("form/output regression must return diagnostics");
        };
        assert!(failures.is_empty(), "form/output failures: {failures}");
    }

    #[test]
    fn form_submission_properties_reflect_live_metadata_and_document_urls() {
        let mut engine = Engine::new();
        let realm = crate::install(
            engine.ctx(),
            "<base href='https://base.test/a/'><form id='f'></form>",
            64,
        )
        .unwrap();
        realm.set_document_url("https://origin.test/page.html");
        let result = eval(
            &mut engine,
            r#"(() => {
            const f=document.querySelector('#f');
            const check=ok=>{if(!ok)throw new Error('form reflection contract')};
            check(f.action===document.URL && f.method==='get' &&
                f.enctype==='application/x-www-form-urlencoded' &&
                f.encoding===f.enctype && f.target==='' && f.acceptCharset==='' &&
                !f.noValidate && f.autocomplete==='on');
            f.action='submit';f.method='POST';f.encoding='text/plain';
            f.target='result';f.acceptCharset='Shift_JIS';f.noValidate=true;f.autocomplete='OFF';
            check(f.action==='https://base.test/a/submit' && f.method==='post' &&
                f.enctype==='text/plain' && f.encoding==='text/plain' &&
                f.getAttribute('target')==='result' &&
                f.getAttribute('accept-charset')==='Shift_JIS' &&
                f.hasAttribute('novalidate') && f.autocomplete==='off');
            f.setAttribute('method','invalid');f.setAttribute('enctype','invalid');
            f.noValidate=false;f.setAttribute('target','changed');
            check(f.method==='get' && f.enctype==='application/x-www-form-urlencoded' &&
                !f.hasAttribute('novalidate') && f.target==='changed');
            f.action='';check(f.action===document.URL);
            return true;
        })()"#,
        );
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn form_control_submission_overrides_reflect_with_live_urls_and_owners() {
        let mut engine = Engine::new();
        let realm = crate::install(
            engine.ctx(),
            "<base id='base' href='https://base.test/a/'><form id='owner'><input id='inside'><button id='button' type='submit' formaction='send' formmethod='POST' formenctype='multipart/form-data' formtarget='result' formnovalidate></button></form><input id='outside' form='owner'>",
            128,
        )
        .unwrap();
        realm.set_document_url("https://origin.test/dir/page.html");

        let result = eval(
            &mut engine,
            r#"(() => {
                    const failures = [];
                    const check = (ok, name) => { if (!ok) failures.push(name); };
                    const form = document.querySelector('#owner');
                    const inside = document.querySelector('#inside');
                    const outside = document.querySelector('#outside');
                    const button = document.querySelector('#button');
                    const base = document.querySelector('#base');
                    check(inside instanceof HTMLInputElement && outside instanceof HTMLInputElement &&
                        button instanceof HTMLButtonElement, 'native-brands');
                    check(inside.form === form && outside.form === form && button.form === form,
                        'initial-form-owner');
                    check(inside.formAction === document.URL && outside.formAction === document.URL,
                        'missing-action-uses-document-url');
                    check(button.formAction === 'https://base.test/a/send', 'relative-action-uses-base');
                    base.href = 'https://changed.test/b/';
                    check(button.formAction === 'https://changed.test/b/send', 'action-follows-live-base');
                    button.formAction = '';
                    check(button.hasAttribute('formaction') && button.formAction === document.URL,
                        'empty-action-uses-document-url');
                    inside.setAttributeNS('urn:custom', 'formaction', '/wrong');
                    check(inside.formAction === document.URL, 'action-reads-null-namespace');
                    inside.formAction = '/next';
                    check(inside.getAttribute('formaction') === '/wrong' &&
                        inside.getAttributeNS(null, 'formaction') === '/next' &&
                        inside.getAttributeNS('urn:custom', 'formaction') === '/wrong' &&
                        inside.formAction === 'https://changed.test/next', 'action-setter-and-url');
                    inside.removeAttributeNS('urn:custom', 'formaction');
                    inside.formAction = 'http://[bad';
                    check(inside.formAction === 'http://[bad', 'invalid-action-retains-content');

                    check(button.formMethod === 'post' && inside.formMethod === 'get', 'method-default-and-normalization');
                    button.formMethod = 'dialog';
                    check(button.getAttribute('formmethod') === 'dialog' && button.formMethod === 'dialog',
                        'method-reflection');
                    inside.setAttribute('formmethod', 'unknown');
                    check(inside.formMethod === 'get', 'invalid-method-default');
                    check(button.formEnctype === 'multipart/form-data' &&
                        inside.formEnctype === 'application/x-www-form-urlencoded', 'enctype-default-and-reflection');
                    inside.formEnctype = 'text/plain';
                    check(inside.getAttribute('formenctype') === 'text/plain' && inside.formEnctype === 'text/plain',
                        'enctype-setter');
                    inside.setAttribute('formenctype', 'unknown');
                    check(inside.formEnctype === 'application/x-www-form-urlencoded', 'invalid-enctype-default');
                    check(button.formTarget === 'result' && inside.formTarget === '', 'target-reflection');
                    inside.formTarget = '_blank';
                    check(inside.getAttribute('formtarget') === '_blank' && inside.formTarget === '_blank',
                        'target-setter');
                    check(button.formNoValidate && !inside.formNoValidate, 'novalidate-initial');
                    inside.formNoValidate = true;
                    check(inside.hasAttribute('formnovalidate') && inside.formNoValidate, 'novalidate-set');
                    inside.formNoValidate = false;
                    check(!inside.hasAttribute('formnovalidate') && !inside.formNoValidate, 'novalidate-clear');

                    outside.setAttribute('form', 'missing');
                    check(outside.form === null, 'unresolved-form-owner');
                    form.id = 'renamed';
                    outside.setAttribute('form', 'renamed');
                    check(outside.form === form, 'dynamic-form-owner');
                    const detached = document.createElement('button');
                    const textarea = document.createElement('textarea');
                    const div = document.createElement('div');
                    check(detached.form === null && textarea.formAction === undefined &&
                        div.formAction === undefined, 'interface-scope-and-null-owner');
                    return failures.join('|');
                })()"#,
        );
        let Value::Str(failures) = result else {
            panic!("form-control override regression must return diagnostics");
        };
        assert!(
            failures.is_empty(),
            "form-control override failures: {failures}"
        );

        realm.set_document_url("https://origin.test/new/page.html");
        let updated = eval(
            &mut engine,
            "document.querySelector('#inside').removeAttribute('formaction'); document.querySelector('#inside').formAction === document.URL",
        );
        assert!(matches!(updated, Value::Bool(true)));
    }

    #[test]
    fn input_value_as_date_uses_date_brand_and_calendar_domains() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<input id='date' type='date' value='2024-06-15'><input id='month' type='month' value='2024-06'><input id='week' type='week' value='2024-W24'><input id='time' type='time' value='12:34:56.789'><input id='local' type='datetime-local' value='2024-06-15T12:00'>",
            128,
        )
        .unwrap();
        let result = eval(
            &mut engine,
            r#"(() => {
                const failures = [];
                const check = (ok, name) => { if (!ok) failures.push(name); };
                const date = document.getElementById('date');
                const month = document.getElementById('month');
                const week = document.getElementById('week');
                const time = document.getElementById('time');
                const local = document.getElementById('local');
                check(date.valueAsDate instanceof Date && date.valueAsDate.getTime() === Date.UTC(2024, 5, 15), 'date-get');
                check(month.valueAsDate instanceof Date && month.valueAsDate.getTime() === Date.UTC(2024, 5, 1), 'month-first-day');
                check(week.valueAsDate instanceof Date && week.valueAsDate.toISOString() === '2024-06-10T00:00:00.000Z', 'week-monday');
                check(time.valueAsDate instanceof Date && time.valueAsDate.getTime() === Date.UTC(1970, 0, 1, 12, 34, 56, 789), 'time-epoch-day');
                check(local.valueAsDate === null, 'datetime-local-unsupported');
                date.valueAsDate = new Date(Date.UTC(1999, 11, 31, 23, 30));
                month.valueAsDate = new Date(Date.UTC(2001, 1, 28, 23, 59));
                time.valueAsDate = new Date(Date.UTC(2001, 0, 2, 4, 5, 6, 7));
                check(date.value === '1999-12-31' && month.value === '2001-02' && time.value === '04:05:06.007', 'date-set');
                date.valueAsDate = null;
                check(date.value === '', 'null-clears');
                date.value = '2002-03-04';
                date.valueAsDate = undefined;
                check(date.value === '', 'undefined-clears-like-null');
                const proxy = new Proxy(new Date(0), {});
                let proxyRejected = false;
                try { date.valueAsDate = proxy; } catch (error) { proxyRejected = error instanceof TypeError; }
                check(proxyRejected, 'proxy-not-date-brand');
                let unsupportedRejected = false;
                try { local.valueAsDate = new Date(0); } catch (_) { unsupportedRejected = true; }
                check(unsupportedRejected, 'unsupported-setter');
                let unsupportedNullRejected = false;
                let unsupportedUndefinedRejected = false;
                try { local.valueAsDate = null; } catch (_) { unsupportedNullRejected = true; }
                try { local.valueAsDate = undefined; } catch (_) { unsupportedUndefinedRejected = true; }
                check(unsupportedNullRejected && unsupportedUndefinedRejected, 'unsupported-nullable-setters');
                return failures.join('|');
            })()"#,
        );
        let Value::Str(failures) = result else {
            panic!("valueAsDate regression must return diagnostics");
        };
        assert!(failures.is_empty(), "valueAsDate failures: {failures}");
    }

    #[test]
    fn fieldset_elements_follow_descendant_membership_not_form_ownership() {
        let mut engine = Engine::new();
        crate::install(
            engine.ctx(),
            "<form id='owner'><fieldset id='set'><legend id='legend'>Title</legend><input id='inside'><input id='image' type='image'><fieldset id='nested'><textarea id='nested-control'></textarea></fieldset><input id='external-owner' form='other'></fieldset></form><form id='other'></form><input id='external-descendant' form='owner'>",
            128,
        )
        .unwrap();
        let result = eval(
            &mut engine,
            r#"(() => {
                const failures = [];
                const check = (ok, name) => { if (!ok) failures.push(name); };
                const fieldset = document.getElementById('set');
                const nested = document.getElementById('nested');
                const elements = fieldset.elements;
                check(fieldset instanceof HTMLFieldSetElement && fieldset.type === 'fieldset', 'brand-type');
                check(fieldset.form === document.getElementById('owner'), 'form-owner');
                check(elements instanceof HTMLCollection && elements === fieldset.elements, 'same-object-collection');
                check(elements.length === 5 && elements[0].id === 'inside' &&
                    elements[1].id === 'image' && elements[2] === nested &&
                    elements[3].id === 'nested-control' && elements[4].id === 'external-owner',
                    'descendant-listed-order-including-image-and-external-owner');
                let outsideIncluded = false;
                for (let i = 0; i < elements.length; i++) {
                    outsideIncluded = outsideIncluded || elements[i].id === 'external-descendant';
                }
                check(!outsideIncluded, 'external-form-owner-outside-fieldset-excluded');
                check(document.getElementById('legend').form === fieldset.form, 'legend-uses-parent-fieldset-form');
                document.getElementById('inside').remove();
                check(elements.length === 4 && elements[0].id === 'image', 'collection-updates-on-removal');
                return failures.join('|');
            })()"#,
        );
        let Value::Str(failures) = result else {
            panic!("fieldset regression must return diagnostics");
        };
        assert!(failures.is_empty(), "fieldset failures: {failures}");
    }
}
