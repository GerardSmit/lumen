//! Distinct native brands for HTML element interfaces backed only by inherited
//! HTMLElement behavior. Their interface-specific attributes and methods remain
//! separate work where they are not already implemented by another subsystem.
use super::{DomElement, DomHtmlElement, DomNode};
use lumen::embed::{Ctx, OpError, OpResult, Value, WeakValue};
use lumen_html::NodeId;
use std::vec::Vec;

pub(super) enum HiddenAttribute {
    Absent,
    Hidden,
    UntilFound,
}

impl HiddenAttribute {
    fn from_string(value: &str) -> Self {
        if value.is_empty() { Self::Absent }
        else if value.eq_ignore_ascii_case("until-found") { Self::UntilFound }
        else { Self::Hidden }
    }
}

impl<'a> lumen_bind::FromArg<'a, lumen::embed::JsHost> for HiddenAttribute {
    fn from_arg(cx: &'a lumen::embed::ArgCx<'_>, value: &'a Value, _at: lumen::embed::Slot) -> Result<Self, Value> {
        <lumen::embed::JsHost as lumen_bind::Host>::with_ctx(cx, |ctx| {
            Ok(match value {
                Value::Undefined | Value::Null | Value::Bool(false) => Self::Absent,
                Value::Num(number) if *number == 0.0 || number.is_nan() => Self::Absent,
                Value::Bool(true) | Value::Num(_) => Self::Hidden,
                Value::Str(value) => Self::from_string(value.as_str()),
                _ => Self::from_string(&ctx.coerce_string(value)?),
            })
        })
    }
}

macro_rules! define_plain_html_interfaces {
    (existing { $( $existing:path, $existing_interface:literal => [$($existing_tag:literal),+], $build:expr; )+ }
     $( $ty:ident, $interface:literal => [$($tag:literal),+] $([$mixin:ident])? $( { $($members:tt)* } )?; )+) => {
        $(
            #[lumen_bind::class(
                name = $interface,
                extends = super::DomHtmlElement,
                hint(js(webidl))
            )]
            pub(crate) struct $ty {
                pub(crate) base: super::DomHtmlElement,
            }

            crate::event_content_handlers::bind_declared_html_handlers! {$ty [$($mixin)?] {
                #[constructor]
                fn new(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<crate::custom_elements::HtmlElementCtor> {
                    crate::custom_elements::construct_customized_interface(ctx, this.0, $interface)
                }
                $($($members)*)?
            }}

            impl lumen_bind::CtorRet<lumen::embed::JsHost, $ty> for crate::custom_elements::HtmlElementCtor {
                fn into_ctor(self, cx: &lumen::embed::ArgCx<'_>) -> Result<Value, Value> {
                    <lumen::embed::JsHost as lumen_bind::Host>::with_ctx(cx, |ctx: &mut Ctx| self.into_ctor_for(ctx, $interface))
                }
            }
        )+

        $(
            impl lumen_bind::CtorRet<lumen::embed::JsHost, $existing> for crate::custom_elements::HtmlElementCtor {
                fn into_ctor(self, cx: &lumen::embed::ArgCx<'_>) -> Result<Value, Value> {
                    <lumen::embed::JsHost as lumen_bind::Host>::with_ctx(cx, |ctx: &mut Ctx| self.into_ctor_for(ctx, $existing_interface))
                }
            }
        )+

        pub(crate) fn custom_interface(local_name: &str) -> Option<&'static str> {
            match local_name {
                $($($tag)|+ => Some($interface),)+
                $($($existing_tag)|+ => Some($existing_interface),)+
                _ => None,
            }
        }

        pub(crate) fn attach_custom_interface(ctx: &mut Ctx, target: &Value, base: DomHtmlElement, interface: &str) -> Result<bool, Value> {
            use lumen_bind::IntoError;
            match interface {
                $($interface => ctx.attach_native_data(target, $ty { base }).map(|()| true).map_err(|error| error.into_error(ctx)),)+
                $($existing_interface => ctx.attach_native_data(target, ($build)(base)).map(|()| true).map_err(|error| error.into_error(ctx)),)+
                _ => Ok(false),
            }
        }

        pub(crate) fn interface_constructor(ctx: &mut Ctx, interface: &str) -> Result<Value, Value> {
            match interface {
                $($interface => Ok(ctx.class_constructor::<$ty>()),)+
                $($existing_interface => Ok(ctx.class_constructor::<$existing>()),)+
                "HTMLElement" => Ok(ctx.class_constructor::<DomHtmlElement>()),
                _ => Err(ctx.make_error("TypeError", "unsupported HTML interface")),
            }
        }

        pub(crate) fn interface_prototype(ctx: &mut Ctx, interface: &str) -> Result<Value, Value> {
            let constructor = interface_constructor(ctx, interface)?;
            ctx.member_get(&constructor, "prototype")
        }

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
                $($($existing_tag)|+ => Ok(ctx.cached_instance(id, || ($build)(DomHtmlElement { base: DomElement { base: node } }))),)+
                _ => Err(node),
            }
        }

        pub(crate) fn constructors(ctx: &mut Ctx) -> Vec<(&'static str, Value)> {
            let mut constructors = Vec::with_capacity([$(stringify!($ty)),+].len() + [$(stringify!($existing)),+].len() + 1);
            $(constructors.push(($interface, ctx.class_constructor::<$ty>()));)+
            $(constructors.push(($existing_interface, ctx.class_constructor::<$existing>()));)+
            constructors.push((
                "HTMLUnknownElement",
                ctx.class_constructor::<DomHtmlUnknownElement>(),
            ));
            constructors
        }
    };
}

define_plain_html_interfaces! {
    existing {
        crate::tables::DomHtmlTableElement, "HTMLTableElement" => ["table"], |base| crate::tables::DomHtmlTableElement { base };
        crate::tables::DomHtmlTableSectionElement, "HTMLTableSectionElement" => ["tbody", "tfoot", "thead"], |base| crate::tables::DomHtmlTableSectionElement { base };
        crate::tables::DomHtmlTableRowElement, "HTMLTableRowElement" => ["tr"], |base| crate::tables::DomHtmlTableRowElement { base };
        crate::tables::DomHtmlTableCellElement, "HTMLTableCellElement" => ["td", "th"], |base| crate::tables::DomHtmlTableCellElement { base };
        crate::tables::DomHtmlTableCaptionElement, "HTMLTableCaptionElement" => ["caption"], |base| crate::tables::DomHtmlTableCaptionElement { base };
        crate::tables::DomHtmlTableColElement, "HTMLTableColElement" => ["col", "colgroup"], |base| crate::tables::DomHtmlTableColElement { base };
        crate::DomHtmlHtmlElement, "HTMLHtmlElement" => ["html"], |base| crate::DomHtmlHtmlElement { base };
        crate::DomHtmlHeadElement, "HTMLHeadElement" => ["head"], |base| crate::DomHtmlHeadElement { base };
        crate::DomHtmlDivElement, "HTMLDivElement" => ["div"], |base| crate::DomHtmlDivElement { base };
        crate::DomHtmlBrElement, "HTMLBRElement" => ["br"], |base| crate::DomHtmlBrElement { base };
        crate::DomHtmlBodyElement, "HTMLBodyElement" => ["body"], |base| crate::DomHtmlBodyElement { base };
        crate::DomHtmlTitleElement, "HTMLTitleElement" => ["title"], |base| crate::DomHtmlTitleElement { base };
        crate::DomHtmlBaseElement, "HTMLBaseElement" => ["base"], |base| crate::DomHtmlBaseElement { base };
        crate::DomHtmlLinkElement, "HTMLLinkElement" => ["link"], |base| crate::DomHtmlLinkElement { base };
        crate::DomHtmlScriptElement, "HTMLScriptElement" => ["script"], |base| crate::DomHtmlScriptElement { base };
        crate::DomHtmlImageElement, "HTMLImageElement" => ["img"], |base| crate::DomHtmlImageElement { base };
        DomHtmlOutputElement, "HTMLOutputElement" => ["output"], |base| DomHtmlOutputElement { base };
        crate::hyperlinks::DomAnchorElement, "HTMLAnchorElement" => ["a"], |base| crate::hyperlinks::DomAnchorElement { base };
        crate::hyperlinks::DomAreaElement, "HTMLAreaElement" => ["area"], |base| crate::hyperlinks::DomAreaElement { base };
        crate::DomDetailsElement, "HTMLDetailsElement" => ["details"], |base| crate::DomDetailsElement { base };
        crate::DomSlotElement, "HTMLSlotElement" => ["slot"], |base| crate::DomSlotElement { base };
        crate::canvas::DomCanvasElement, "HTMLCanvasElement" => ["canvas"], crate::canvas::DomCanvasElement::from_node;
        crate::media::DomHtmlAudioElement, "HTMLAudioElement" => ["audio"], crate::media::DomHtmlAudioElement::from_html;
        crate::media::DomHtmlVideoElement, "HTMLVideoElement" => ["video"], crate::media::DomHtmlVideoElement::from_html;
        crate::DomInputElement, "HTMLInputElement" => ["input"], |base| crate::DomInputElement { base };
        crate::DomSelectElement, "HTMLSelectElement" => ["select"], |base| crate::DomSelectElement { base };
        crate::DomOptionElement, "HTMLOptionElement" => ["option"], |base| crate::DomOptionElement { base };
        crate::DomTextAreaElement, "HTMLTextAreaElement" => ["textarea"], |base| crate::DomTextAreaElement { base };
        crate::DomFormElement, "HTMLFormElement" => ["form"], |base| crate::DomFormElement { base };
        crate::DomStyleElement, "HTMLStyleElement" => ["style"], |base| crate::DomStyleElement { base };
        crate::DomIFrameElement, "HTMLIFrameElement" => ["iframe"], |base| crate::DomIFrameElement { base };
        crate::DomTemplateElement, "HTMLTemplateElement" => ["template"], |base| crate::DomTemplateElement { base };
    }
    DomHtmlButtonElement, "HTMLButtonElement" => ["button"] {
            #[getter(name = "name")]
            fn name(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("name")?.unwrap_or_default()) }
            #[setter(name = "name", coerce, hint(js(ce_reactions)))]
            fn set_name(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("name", value) }
        #[getter(name = "value")]
        fn button_value(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("value")?.unwrap_or_default())
        }
        #[setter(name = "value", coerce, hint(js(ce_reactions)))]
        fn set_button_value(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("value", value)
        }
        #[getter(name = "command")]
        fn command(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("command")?.map(|value| lumen_html::invokers::Command::reflected(&value).to_owned()).unwrap_or_default()) }
        #[setter(name = "command", coerce, hint(js(ce_reactions)))]
        fn set_command(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("command", value) }
        #[getter(name = "commandForElement")]
        fn command_for_element(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> { super::element_reflection::get(ctx, this.0, "commandfor") }
        #[setter(name = "commandForElement", hint(js(ce_reactions)))]
        fn set_command_for_element(ctx: &mut Ctx, this: lumen_bind::This<Value>, value: Value) -> OpResult<()> { super::element_reflection::set(ctx, this.0, "commandfor", value) }
        #[getter(name = "popoverTargetElement")]
        fn popover_target_element(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<Value> { super::element_reflection::get(ctx, this.0, "popovertarget") }
        #[setter(name = "popoverTargetElement", hint(js(ce_reactions)))]
        fn set_popover_target_element(ctx: &mut Ctx, this: lumen_bind::This<Value>, value: Value) -> OpResult<()> { super::element_reflection::set(ctx, this.0, "popovertarget", value) }
        #[getter(name = "popoverTargetAction")]
        fn popover_target_action(&self) -> OpResult<String> { Ok(super::invokers::reflected_popover_action(&self.base.base.base)?) }
        #[setter(name = "popoverTargetAction", coerce, hint(js(ce_reactions)))]
        fn set_popover_target_action(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("popovertargetaction", value) }
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

        #[setter(name = "type", coerce, hint(js(ce_reactions)))]
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

        #[setter(name = "formAction", coerce, hint(js(ce_reactions)))]
        fn set_form_action(&self, value: lumen_host::webidl::Usv) -> OpResult<()> {
            self.base.base.base.set_attribute_core("formaction", &value.0)
        }

        #[getter(name = "formEnctype")]
        fn form_enctype(&self) -> OpResult<String> {
            form_enctype_value(&self.base.base.base)
        }

        #[setter(name = "formEnctype", coerce, hint(js(ce_reactions)))]
        fn set_form_enctype(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("formenctype", value)
        }

        #[getter(name = "formMethod")]
        fn form_method(&self) -> OpResult<String> {
            form_method_value(&self.base.base.base)
        }

        #[setter(name = "formMethod", coerce, hint(js(ce_reactions)))]
        fn set_form_method(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("formmethod", value)
        }

        #[getter(name = "formNoValidate")]
        fn form_no_validate(&self) -> OpResult<bool> {
            self.base.base.base.has_null_attribute("formnovalidate")
        }

        #[setter(name = "formNoValidate", coerce, hint(js(ce_reactions)))]
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

        #[setter(name = "formTarget", coerce, hint(js(ce_reactions)))]
        fn set_form_target(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("formtarget", value)
        }
    };
    DomHtmlDataElement, "HTMLDataElement" => ["data"] {
        #[getter(name = "value")]
        fn value(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("value")?.unwrap_or_default())
        }
        #[setter(name = "value", coerce, hint(js(ce_reactions)))]
        fn set_value(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("value", value)
        }
    };
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
        #[getter(name = "closedBy")]
        fn closed_by(&self) -> OpResult<String> {
            let node = &self.base.base.base;
            let raw = node.get_null_attribute("closedby")?.unwrap_or_default();
            Ok(if raw.eq_ignore_ascii_case("any") { "any" } else if raw.eq_ignore_ascii_case("closerequest") { "closerequest" } else if raw.eq_ignore_ascii_case("none") { "none" } else if lumen_html::top_layer::dialog_modal_state(node.realm.session.borrow().document(), node.id) == lumen_html::top_layer::DialogModalState::Modal { "closerequest" } else { "none" }.to_owned())
        }
        #[setter(name = "closedBy", coerce, hint(js(ce_reactions)))]
        fn set_closed_by(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("closedby", value) }
        #[method(name = "requestClose", hint(js(ce_reactions)))]
        fn request_close(ctx: &mut Ctx, this: lumen_bind::This<Value>, result: lumen_bind::Passed<Value>) -> OpResult<()> {
            let result = super::option_factory::optional_string(ctx, result)?;
            let (realm, node) = ctx.with_instance::<DomHtmlDialogElement, _>(&this.0, |element| element.base.base.base.realm.resolve_adopted_node(element.base.base.base.id))?;
            super::dialog_popover::request_close_dialog(ctx, &realm, node, result.as_deref(), Value::Null)
        }
        #[getter]
        fn open(&self) -> OpResult<bool> {
            let node = &self.base.base.base;
            super::dialog_popover::dialog_open(&node.realm, node.id)
        }

        #[setter(coerce, hint(js(ce_reactions)))]
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

        #[method(name = "show", hint(js(ce_reactions)))]
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

        #[method(name = "showModal", hint(js(ce_reactions)))]
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

        #[method(name = "close", hint(js(ce_reactions)))]
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
    DomHtmlModElement, "HTMLModElement" => ["del", "ins"] {
        #[getter(name = "cite")]
        fn cite(&self) -> OpResult<String> {
            reflected_usv_url_value(&self.base.base.base, "cite")
        }
        #[setter(name = "cite", hint(js(ce_reactions)))]
        fn set_cite(&self, value: lumen_host::webidl::Usv) -> OpResult<()> {
            self.base.base.base.set_attribute_core("cite", &value.0)
        }
        #[getter(name = "dateTime")]
        fn date_time(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("datetime")?.unwrap_or_default())
        }
        #[setter(name = "dateTime", coerce, hint(js(ce_reactions)))]
        fn set_date_time(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("datetime", value)
        }
    };
    DomHtmlDirectoryElement, "HTMLDirectoryElement" => ["dir"] {
        #[getter]
        fn compact(&self) -> OpResult<bool> { Ok(self.base.base.base.get_null_attribute("compact")?.is_some()) }
        #[setter(coerce, hint(js(ce_reactions)))]
        fn set_compact(&self, value: bool) -> OpResult<()> { self.base.base.base.set_nullable_attribute_core("compact", value.then_some("")) }
    };
    DomHtmlDListElement, "HTMLDListElement" => ["dl"] {
        #[getter(name="compact")]
        fn compact(&self)->OpResult<bool> {Ok(self.base.base.base.get_null_attribute("compact")?.is_some())}
        #[setter(name="compact",coerce,hint(js(ce_reactions)))]
        fn set_compact(&self,value:bool)->OpResult<()> {self.base.base.base.set_nullable_attribute_core("compact",value.then_some(""))}
    };
    DomHtmlEmbedElement, "HTMLEmbedElement" => ["embed"] {
        #[getter]
        fn src(&self) -> OpResult<String> {
            let node=&self.base.base.base;
            let Some(value)=node.get_null_attribute("src")? else{return Ok(String::new())};
            Ok(lumen_common::url::parse(&value,Some(&node.realm.base_url())).map(|url|url.href()).unwrap_or(value))
        }
        #[setter(hint(js(ce_reactions)))]
        fn set_src(&self,#[default(lumen_host::webidl::Usv(String::from("undefined")))] value:lumen_host::webidl::Usv)->OpResult<()> {
            self.base.base.base.set_attribute_core("src",&value.0)
        }
        #[getter(name="type")]
        fn kind(&self)->OpResult<String>{Ok(self.base.base.base.get_null_attribute("type")?.unwrap_or_default())}
        #[setter(name="type",coerce,hint(js(ce_reactions)))]
        fn set_kind(&self,value:&str)->OpResult<()>{self.base.base.base.set_attribute_core("type",value)}
        #[getter]
        fn name(&self)->OpResult<String>{Ok(self.base.base.base.get_null_attribute("name")?.unwrap_or_default())}
        #[setter(coerce,hint(js(ce_reactions)))]
        fn set_name(&self,value:&str)->OpResult<()>{self.base.base.base.set_attribute_core("name",value)}
        #[getter]
        fn align(&self)->OpResult<String>{Ok(self.base.base.base.get_null_attribute("align")?.unwrap_or_default())}
        #[setter(coerce,hint(js(ce_reactions)))]
        fn set_align(&self,value:&str)->OpResult<()>{self.base.base.base.set_attribute_core("align",value)}
        #[getter]
        fn width(&self)->OpResult<String>{Ok(self.base.base.base.get_null_attribute("width")?.unwrap_or_default())}
        #[setter(coerce,hint(js(ce_reactions)))]
        fn set_width(&self,value:&str)->OpResult<()>{self.base.base.base.set_attribute_core("width",value)}
        #[getter]
        fn height(&self)->OpResult<String>{Ok(self.base.base.base.get_null_attribute("height")?.unwrap_or_default())}
        #[setter(coerce,hint(js(ce_reactions)))]
        fn set_height(&self,value:&str)->OpResult<()>{self.base.base.base.set_attribute_core("height",value)}
        #[method(name="getSVGDocument")]
        fn svg_document(&self,ctx:&mut Ctx)->OpResult<Value>{
            let node=&self.base.base.base;
            super::object_loading::content_document(ctx,&node.realm,node.id,true)
        }
    };
    DomHtmlFieldSetElement, "HTMLFieldSetElement" => ["fieldset"] {
            #[getter(name = "name")]
            fn name(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("name")?.unwrap_or_default()) }
            #[setter(name = "name", coerce, hint(js(ce_reactions)))]
            fn set_name(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("name", value) }
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
    DomHtmlFontElement, "HTMLFontElement" => ["font"] {
        #[getter(name = "face")]
        fn face(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("face")?.unwrap_or_default())
        }
        #[setter(name = "face", coerce, hint(js(ce_reactions)))]
        fn set_face(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("face", value)
        }
        #[getter(name = "size")]
        fn size(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("size")?.unwrap_or_default())
        }
        #[setter(name = "size", coerce, hint(js(ce_reactions)))]
        fn set_size(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("size", value)
        }
        #[getter]
        fn color(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("color")?.unwrap_or_default()) }
        #[setter(coerce, hint(js(ce_reactions)))]
        fn set_color(&self, value: lumen_host::webidl::LegacyNullToEmptyString<'_>) -> OpResult<()> { self.base.base.base.set_attribute_core("color", value.0) }
    };
    DomHtmlFrameElement, "HTMLFrameElement" => ["frame"] {
            #[getter(name = "name")]
            fn name(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("name")?.unwrap_or_default()) }
            #[setter(name = "name", coerce, hint(js(ce_reactions)))]
            fn set_name(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("name", value) }
            #[getter(name = "scrolling")]
            fn scrolling(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("scrolling")?.unwrap_or_default()) }
            #[setter(name = "scrolling", coerce, hint(js(ce_reactions)))]
            fn set_scrolling(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("scrolling", value) }
            #[getter(name = "src")]
            fn src(&self) -> OpResult<String> { crate::html_interfaces::reflected_usv_url_value(&self.base.base.base, "src") }
            #[setter(name = "src", hint(js(ce_reactions)))]
            fn set_src(&self, value: lumen_host::webidl::Usv) -> OpResult<()> { (&self.base.base.base).set_attribute_core("src", &value.0) }
            #[getter(name = "frameBorder")]
            fn frame_border(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("frameborder")?.unwrap_or_default()) }
            #[setter(name = "frameBorder", coerce, hint(js(ce_reactions)))]
            fn set_frame_border(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("frameborder", value) }
            #[getter(name = "longDesc")]
            fn long_desc(&self) -> OpResult<String> { crate::html_interfaces::reflected_usv_url_value(&self.base.base.base, "longdesc") }
            #[setter(name = "longDesc", hint(js(ce_reactions)))]
            fn set_long_desc(&self, value: lumen_host::webidl::Usv) -> OpResult<()> { (&self.base.base.base).set_attribute_core("longdesc", &value.0) }
            #[getter(name = "noResize")]
            fn no_resize(&self) -> OpResult<bool> { Ok((&self.base.base.base).get_null_attribute("noresize")?.is_some()) }
            #[setter(name = "noResize", coerce, hint(js(ce_reactions)))]
            fn set_no_resize(&self, value: bool) -> OpResult<()> { (&self.base.base.base).set_nullable_attribute_core("noresize", value.then_some("")) }
            #[getter(name = "marginHeight")]
            fn margin_height(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("marginheight")?.unwrap_or_default()) }
            #[setter(name = "marginHeight", coerce, hint(js(ce_reactions)))]
            fn set_margin_height(&self, value: lumen_host::webidl::LegacyNullToEmptyString<'_>) -> OpResult<()> { (&self.base.base.base).set_attribute_core("marginheight", value.0) }
            #[getter(name = "marginWidth")]
            fn margin_width(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("marginwidth")?.unwrap_or_default()) }
            #[setter(name = "marginWidth", coerce, hint(js(ce_reactions)))]
            fn set_margin_width(&self, value: lumen_host::webidl::LegacyNullToEmptyString<'_>) -> OpResult<()> { (&self.base.base.base).set_attribute_core("marginwidth", value.0) }
    };
    DomHtmlFrameSetElement, "HTMLFrameSetElement" => ["frameset"] [window_handlers] {
        #[getter(name = "cols")]
        fn cols(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("cols")?.unwrap_or_default())
        }
        #[setter(name = "cols", coerce, hint(js(ce_reactions)))]
        fn set_cols(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("cols", value)
        }
        #[getter(name = "rows")]
        fn rows(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("rows")?.unwrap_or_default())
        }
        #[setter(name = "rows", coerce, hint(js(ce_reactions)))]
        fn set_rows(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("rows", value)
        }
    };
    DomHtmlHeadingElement, "HTMLHeadingElement" => ["h1", "h2", "h3", "h4", "h5", "h6"] {
        #[getter(name = "align")]
        fn align(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("align")?.unwrap_or_default())
        }
        #[setter(name = "align", coerce, hint(js(ce_reactions)))]
        fn set_align(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("align", value)
        }
    };
    DomHtmlHrElement, "HTMLHRElement" => ["hr"] {
        #[getter(name="align")]
        fn align(&self)->OpResult<String> {Ok(self.base.base.base.get_null_attribute("align")?.unwrap_or_default())}
        #[setter(name="align",coerce,hint(js(ce_reactions)))]
        fn set_align(&self,value:&str)->OpResult<()> {self.base.base.base.set_attribute_core("align",value)}
        #[getter(name="color")]
        fn color(&self)->OpResult<String> {Ok(self.base.base.base.get_null_attribute("color")?.unwrap_or_default())}
        #[setter(name="color",coerce,hint(js(ce_reactions)))]
        fn set_color(&self,value:&str)->OpResult<()> {self.base.base.base.set_attribute_core("color",value)}
        #[getter(name="noShade")]
        fn no_shade(&self)->OpResult<bool> {Ok(self.base.base.base.get_null_attribute("noshade")?.is_some())}
        #[setter(name="noShade",coerce,hint(js(ce_reactions)))]
        fn set_no_shade(&self,value:bool)->OpResult<()> {self.base.base.base.set_nullable_attribute_core("noshade",value.then_some(""))}
        #[getter(name="size")]
        fn size(&self)->OpResult<String> {Ok(self.base.base.base.get_null_attribute("size")?.unwrap_or_default())}
        #[setter(name="size",coerce,hint(js(ce_reactions)))]
        fn set_size(&self,value:&str)->OpResult<()> {self.base.base.base.set_attribute_core("size",value)}
        #[getter(name="width")]
        fn width(&self)->OpResult<String> {Ok(self.base.base.base.get_null_attribute("width")?.unwrap_or_default())}
        #[setter(name="width",coerce,hint(js(ce_reactions)))]
        fn set_width(&self,value:&str)->OpResult<()> {self.base.base.base.set_attribute_core("width",value)}
    };
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

        #[setter(name = "htmlFor", coerce, hint(js(ce_reactions)))]
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
        // HTML's legacy partial interface reflects the raw null-namespace
        // token. Rendering's presentational hint validates it separately.
        #[getter]
        fn align(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("align")?.unwrap_or_default())
        }
        #[setter(coerce, hint(js(ce_reactions)))]
        fn set_align(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("align", value)
        }

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
    DomHtmlLiElement, "HTMLLIElement" => ["li"] {
        #[getter(name="value")]
        fn value(&self)->OpResult<i32> {Ok(self.base.base.base.get_null_attribute("value")?.as_deref().and_then(lumen_common::html_numbers::parse_integer_i32).unwrap_or(0))}
        #[setter(name="value",coerce,hint(js(ce_reactions)))]
        fn set_value(&self,value:i32)->OpResult<()> {self.base.base.base.set_attribute_core("value",&value.to_string())}
        #[getter(name="type")]
        fn type_(&self)->OpResult<String> {Ok(self.base.base.base.get_null_attribute("type")?.unwrap_or_default())}
        #[setter(name="type",coerce,hint(js(ce_reactions)))]
        fn set_type_(&self,value:&str)->OpResult<()> {self.base.base.base.set_attribute_core("type",value)}
    };
    DomHtmlMenuElement, "HTMLMenuElement" => ["menu"] {
        #[getter]
        fn compact(&self) -> OpResult<bool> { Ok(self.base.base.base.get_null_attribute("compact")?.is_some()) }
        #[setter(coerce, hint(js(ce_reactions)))]
        fn set_compact(&self, value: bool) -> OpResult<()> { self.base.base.base.set_nullable_attribute_core("compact", value.then_some("")) }
    };
    DomHtmlMapElement, "HTMLMapElement" => ["map"] {
        #[getter(name = "name")]
        fn name(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("name")?.unwrap_or_default())
        }
        #[setter(name = "name", coerce, hint(js(ce_reactions)))]
        fn set_name(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("name", value)
        }
        #[getter]
        fn areas(&self, ctx: &mut Ctx, this: lumen_bind::This<Value>) -> Value {
            self.base.base.base.descendant_collection(ctx, this.0, "areas".into(), super::DescendantFilter::TagNs(Some("http://www.w3.org/1999/xhtml".into()), "area".into()))
        }
    };
    DomHtmlMarqueeElement, "HTMLMarqueeElement" => ["marquee"] {
        #[getter(name="behavior")]
        fn behavior(&self)->OpResult<String> {Ok(self.base.base.base.get_null_attribute("behavior")?.unwrap_or_default())}
        #[setter(name="behavior",coerce,hint(js(ce_reactions)))]
        fn set_behavior(&self,value:&str)->OpResult<()> {self.base.base.base.set_attribute_core("behavior",value)}
        #[getter(name="bgColor")]
        fn bg_color(&self)->OpResult<String> {Ok(self.base.base.base.get_null_attribute("bgcolor")?.unwrap_or_default())}
        #[setter(name="bgColor",coerce,hint(js(ce_reactions)))]
        fn set_bg_color(&self,value:&str)->OpResult<()> {self.base.base.base.set_attribute_core("bgcolor",value)}
        #[getter(name="direction")]
        fn direction(&self)->OpResult<String> {Ok(self.base.base.base.get_null_attribute("direction")?.unwrap_or_default())}
        #[setter(name="direction",coerce,hint(js(ce_reactions)))]
        fn set_direction(&self,value:&str)->OpResult<()> {self.base.base.base.set_attribute_core("direction",value)}
        #[getter(name="height")]
        fn height(&self)->OpResult<String> {Ok(self.base.base.base.get_null_attribute("height")?.unwrap_or_default())}
        #[setter(name="height",coerce,hint(js(ce_reactions)))]
        fn set_height(&self,value:&str)->OpResult<()> {self.base.base.base.set_attribute_core("height",value)}
        #[getter(name="width")]
        fn width(&self)->OpResult<String> {Ok(self.base.base.base.get_null_attribute("width")?.unwrap_or_default())}
        #[setter(name="width",coerce,hint(js(ce_reactions)))]
        fn set_width(&self,value:&str)->OpResult<()> {self.base.base.base.set_attribute_core("width",value)}
        #[getter(name="hspace")]
        fn hspace(&self)->OpResult<u32> {Ok(lumen_html::forms::reflected_unsigned_long(self.base.base.base.get_null_attribute("hspace")?.as_deref(),0))}
        #[setter(name="hspace",coerce,hint(js(ce_reactions)))]
        fn set_hspace(&self,value:u32)->OpResult<()> {self.base.base.base.set_attribute_core("hspace",&lumen_html::forms::reflected_unsigned_long_setter_value(value,0).to_string())}
        #[getter(name="vspace")]
        fn vspace(&self)->OpResult<u32> {Ok(lumen_html::forms::reflected_unsigned_long(self.base.base.base.get_null_attribute("vspace")?.as_deref(),0))}
        #[setter(name="vspace",coerce,hint(js(ce_reactions)))]
        fn set_vspace(&self,value:u32)->OpResult<()> {self.base.base.base.set_attribute_core("vspace",&lumen_html::forms::reflected_unsigned_long_setter_value(value,0).to_string())}
        #[getter(name="scrollAmount")]
        fn scroll_amount(&self)->OpResult<u32> {Ok(lumen_html::forms::reflected_unsigned_long(self.base.base.base.get_null_attribute("scrollamount")?.as_deref(),6))}
        #[setter(name="scrollAmount",coerce,hint(js(ce_reactions)))]
        fn set_scroll_amount(&self,value:u32)->OpResult<()> {self.base.base.base.set_attribute_core("scrollamount",&lumen_html::forms::reflected_unsigned_long_setter_value(value,6).to_string())}
        #[getter(name="scrollDelay")]
        fn scroll_delay(&self)->OpResult<u32> {Ok(lumen_html::forms::reflected_unsigned_long(self.base.base.base.get_null_attribute("scrolldelay")?.as_deref(),85))}
        #[setter(name="scrollDelay",coerce,hint(js(ce_reactions)))]
        fn set_scroll_delay(&self,value:u32)->OpResult<()> {self.base.base.base.set_attribute_core("scrolldelay",&lumen_html::forms::reflected_unsigned_long_setter_value(value,85).to_string())}
        #[getter(name="trueSpeed")]
        fn true_speed(&self)->OpResult<bool> {Ok(self.base.base.base.get_null_attribute("truespeed")?.is_some())}
        #[setter(name="trueSpeed",coerce,hint(js(ce_reactions)))]
        fn set_true_speed(&self,value:bool)->OpResult<()> {self.base.base.base.set_nullable_attribute_core("truespeed",value.then_some(""))}
        #[getter(name="loop")]
        fn loop_(&self)->OpResult<i32> {Ok(self.base.base.base.get_null_attribute("loop")?.as_deref().and_then(lumen_common::html_numbers::parse_integer_i32).filter(|value|*value>0).unwrap_or(-1))}
        #[setter(name="loop",coerce,hint(js(ce_reactions)))]
        fn set_loop(&self,value:i32)->OpResult<()> {
            if (value>0 || value == -1) && value != self.loop_()? {self.base.base.base.set_attribute_core("loop",&value.to_string())?;}
            Ok(())
        }
    };
    DomHtmlMetaElement, "HTMLMetaElement" => ["meta"] {
        #[getter]
        fn media(&self)->OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("media")?.unwrap_or_default())
        }
        #[setter(coerce,hint(js(ce_reactions)))]
        fn set_media(&self,value:&str)->OpResult<()> {self.base.base.base.set_attribute_core("media",value)}
        #[getter]
        fn name(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("name")?.unwrap_or_default())
        }
        #[setter(coerce, hint(js(ce_reactions)))]
        fn set_name(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("name", value) }
        #[getter]
        fn content(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("content")?.unwrap_or_default())
        }
        #[setter(coerce, hint(js(ce_reactions)))]
        fn set_content(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("content", value) }
        #[getter(rename(js = "httpEquiv"))]
        fn http_equiv(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("http-equiv")?.unwrap_or_default())
        }
        #[setter(coerce, rename(js = "httpEquiv"), hint(js(ce_reactions)))]
        fn set_http_equiv(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("http-equiv", value) }
        #[getter]
        fn scheme(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("scheme")?.unwrap_or_default())
        }
        #[setter(coerce, hint(js(ce_reactions)))]
        fn set_scheme(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("scheme", value) }
    };
    DomHtmlMeterElement, "HTMLMeterElement" => ["meter"] {
        #[getter]
        fn value(&self) -> OpResult<f64> { Ok(meter_state(&self.base.base.base)?.value) }
        #[setter(coerce, hint(js(ce_reactions)))]
        fn set_value(&self, ctx: &mut Ctx, value: f64) -> OpResult<()> { set_reflected_double(&self.base.base.base, ctx, "value", value, false) }
        #[getter]
        fn min(&self) -> OpResult<f64> { Ok(meter_state(&self.base.base.base)?.minimum) }
        #[setter(coerce, hint(js(ce_reactions)))]
        fn set_min(&self, ctx: &mut Ctx, value: f64) -> OpResult<()> { set_reflected_double(&self.base.base.base, ctx, "min", value, false) }
        #[getter]
        fn max(&self) -> OpResult<f64> { Ok(meter_state(&self.base.base.base)?.maximum) }
        #[setter(coerce, hint(js(ce_reactions)))]
        fn set_max(&self, ctx: &mut Ctx, value: f64) -> OpResult<()> { set_reflected_double(&self.base.base.base, ctx, "max", value, false) }
        #[getter]
        fn low(&self) -> OpResult<f64> { Ok(meter_state(&self.base.base.base)?.low) }
        #[setter(coerce, hint(js(ce_reactions)))]
        fn set_low(&self, ctx: &mut Ctx, value: f64) -> OpResult<()> { set_reflected_double(&self.base.base.base, ctx, "low", value, false) }
        #[getter]
        fn high(&self) -> OpResult<f64> { Ok(meter_state(&self.base.base.base)?.high) }
        #[setter(coerce, hint(js(ce_reactions)))]
        fn set_high(&self, ctx: &mut Ctx, value: f64) -> OpResult<()> { set_reflected_double(&self.base.base.base, ctx, "high", value, false) }
        #[getter]
        fn optimum(&self) -> OpResult<f64> { Ok(meter_state(&self.base.base.base)?.optimum) }
        #[setter(coerce, hint(js(ce_reactions)))]
        fn set_optimum(&self, ctx: &mut Ctx, value: f64) -> OpResult<()> { set_reflected_double(&self.base.base.base, ctx, "optimum", value, false) }
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
            #[getter(name = "align")]
            fn align(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("align")?.unwrap_or_default()) }
            #[setter(name = "align", coerce, hint(js(ce_reactions)))]
            fn set_align(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("align", value) }
            #[getter(name = "archive")]
            fn archive(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("archive")?.unwrap_or_default()) }
            #[setter(name = "archive", coerce, hint(js(ce_reactions)))]
            fn set_archive(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("archive", value) }
            #[getter(name = "code")]
            fn code(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("code")?.unwrap_or_default()) }
            #[setter(name = "code", coerce, hint(js(ce_reactions)))]
            fn set_code(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("code", value) }
            #[getter(name = "declare")]
            fn declare(&self) -> OpResult<bool> { Ok((&self.base.base.base).get_null_attribute("declare")?.is_some()) }
            #[setter(name = "declare", coerce, hint(js(ce_reactions)))]
            fn set_declare(&self, value: bool) -> OpResult<()> { (&self.base.base.base).set_nullable_attribute_core("declare", value.then_some("")) }
            #[getter(name = "hspace")]
            fn hspace(&self) -> OpResult<u32> { Ok(lumen_html::forms::reflected_unsigned_long((&self.base.base.base).get_null_attribute("hspace")?.as_deref(), 0)) }
            #[setter(name = "hspace", coerce, hint(js(ce_reactions)))]
            fn set_hspace(&self, value: u32) -> OpResult<()> { (&self.base.base.base).set_attribute_core("hspace", &lumen_html::forms::reflected_unsigned_long_setter_value(value, 0).to_string()) }
            #[getter(name = "vspace")]
            fn vspace(&self) -> OpResult<u32> { Ok(lumen_html::forms::reflected_unsigned_long((&self.base.base.base).get_null_attribute("vspace")?.as_deref(), 0)) }
            #[setter(name = "vspace", coerce, hint(js(ce_reactions)))]
            fn set_vspace(&self, value: u32) -> OpResult<()> { (&self.base.base.base).set_attribute_core("vspace", &lumen_html::forms::reflected_unsigned_long_setter_value(value, 0).to_string()) }
            #[getter(name = "standby")]
            fn standby(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("standby")?.unwrap_or_default()) }
            #[setter(name = "standby", coerce, hint(js(ce_reactions)))]
            fn set_standby(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("standby", value) }
            #[getter(name = "codeType")]
            fn code_type(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("codetype")?.unwrap_or_default()) }
            #[setter(name = "codeType", coerce, hint(js(ce_reactions)))]
            fn set_code_type(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("codetype", value) }
            #[getter(name = "useMap")]
            fn use_map(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("usemap")?.unwrap_or_default()) }
            #[setter(name = "useMap", coerce, hint(js(ce_reactions)))]
            fn set_use_map(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("usemap", value) }
            #[getter(name = "border")]
            fn border(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("border")?.unwrap_or_default()) }
            #[setter(name = "border", coerce, hint(js(ce_reactions)))]
            fn set_border(&self, value: lumen_host::webidl::LegacyNullToEmptyString<'_>) -> OpResult<()> { (&self.base.base.base).set_attribute_core("border", value.0) }
            #[getter(name = "codeBase")]
            fn code_base(&self) -> OpResult<String> { crate::html_interfaces::reflected_url_value(&self.base.base.base, "codebase") }
            #[setter(name = "codeBase", coerce, hint(js(ce_reactions)))]
            fn set_code_base(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("codebase", value) }
        #[getter]
        fn form(&self, ctx: &mut Ctx) -> Value {
            let node = &self.base.base.base;
            super::forms::form_owner_value(ctx, &node.realm, node.id)
        }
        #[getter]
        fn data(&self) -> OpResult<String> {
            let node = &self.base.base.base;
            let Some(value) = node.get_null_attribute("data")? else { return Ok(String::new()); };
            Ok(lumen_common::url::parse(&value, Some(&node.realm.base_url()))
                .map(|url| url.href()).unwrap_or(value))
        }
        #[setter(hint(js(ce_reactions)))]
        fn set_data(&self, #[default(lumen_host::webidl::Usv(String::from("undefined")))] value: lumen_host::webidl::Usv) -> OpResult<()> {
            self.base.base.base.set_attribute_core("data", &value.0)
        }
        #[getter(name = "type")]
        fn kind(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("type")?.unwrap_or_default())
        }
        #[setter(name = "type", coerce, hint(js(ce_reactions)))]
        fn set_kind(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("type", value) }
        #[getter]
        fn name(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("name")?.unwrap_or_default())
        }
        #[setter(coerce, hint(js(ce_reactions)))]
        fn set_name(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("name", value) }
        #[getter]
        fn width(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("width")?.unwrap_or_default())
        }
        #[setter(coerce, hint(js(ce_reactions)))]
        fn set_width(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("width", value) }
        #[getter]
        fn height(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("height")?.unwrap_or_default())
        }
        #[setter(coerce, hint(js(ce_reactions)))]
        fn set_height(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("height", value) }
        #[getter(name = "contentWindow")]
        fn content_window(&self) -> Value {
            let node = &self.base.base.base;
            node.realm.represented_object_context(node.id).and_then(|frame| frame.window_proxy()).unwrap_or(Value::Null)
        }
        #[getter(name = "contentDocument")]
        fn content_document(&self, ctx: &mut Ctx) -> OpResult<Value> {
            let node = &self.base.base.base;
            super::object_loading::content_document(ctx, &node.realm, node.id, false)
        }
        #[method(name = "getSVGDocument")]
        fn svg_document(&self, ctx: &mut Ctx) -> OpResult<Value> {
            let node = &self.base.base.base;
            super::object_loading::content_document(ctx, &node.realm, node.id, true)
        }
    };
    DomHtmlOListElement, "HTMLOListElement" => ["ol"] {
        #[getter(name="reversed")]
        fn reversed(&self)->OpResult<bool> {Ok(self.base.base.base.get_null_attribute("reversed")?.is_some())}
        #[setter(name="reversed",coerce,hint(js(ce_reactions)))]
        fn set_reversed(&self,value:bool)->OpResult<()> {self.base.base.base.set_nullable_attribute_core("reversed",value.then_some(""))}
        #[getter(name="start")]
        fn start(&self)->OpResult<i32> {Ok(self.base.base.base.get_null_attribute("start")?.as_deref().and_then(lumen_common::html_numbers::parse_integer_i32).unwrap_or(1))}
        #[setter(name="start",coerce,hint(js(ce_reactions)))]
        fn set_start(&self,value:i32)->OpResult<()> {self.base.base.base.set_attribute_core("start",&value.to_string())}
        #[getter(name="type")]
        fn type_(&self)->OpResult<String> {Ok(self.base.base.base.get_null_attribute("type")?.unwrap_or_default())}
        #[setter(name="type",coerce,hint(js(ce_reactions)))]
        fn set_type_(&self,value:&str)->OpResult<()> {self.base.base.base.set_attribute_core("type",value)}
        #[getter(name="compact")]
        fn compact(&self)->OpResult<bool> {Ok(self.base.base.base.get_null_attribute("compact")?.is_some())}
        #[setter(name="compact",coerce,hint(js(ce_reactions)))]
        fn set_compact(&self,value:bool)->OpResult<()> {self.base.base.base.set_nullable_attribute_core("compact",value.then_some(""))}
    };
    DomHtmlOptGroupElement, "HTMLOptGroupElement" => ["optgroup"] {
            #[getter(name = "label")]
            fn label(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("label")?.unwrap_or_default()) }
            #[setter(name = "label", coerce, hint(js(ce_reactions)))]
            fn set_label(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("label", value) }
    };
    DomHtmlParagraphElement, "HTMLParagraphElement" => ["p"] {
        #[getter(name="align")]
        fn align(&self)->OpResult<String> {Ok(self.base.base.base.get_null_attribute("align")?.unwrap_or_default())}
        #[setter(name="align",coerce,hint(js(ce_reactions)))]
        fn set_align(&self,value:&str)->OpResult<()> {self.base.base.base.set_attribute_core("align",value)}
    };
    DomHtmlParamElement, "HTMLParamElement" => ["param"] {
        #[getter(name = "name")]
        fn name(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("name")?.unwrap_or_default())
        }
        #[setter(name = "name", coerce, hint(js(ce_reactions)))]
        fn set_name(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("name", value)
        }
        #[getter(name = "value")]
        fn value(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("value")?.unwrap_or_default())
        }
        #[setter(name = "value", coerce, hint(js(ce_reactions)))]
        fn set_value(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("value", value)
        }
        #[getter(name = "type")]
        fn kind(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("type")?.unwrap_or_default())
        }
        #[setter(name = "type", coerce, hint(js(ce_reactions)))]
        fn set_kind(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("type", value)
        }
        #[getter(name = "valueType")]
        fn value_type(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("valuetype")?.unwrap_or_default())
        }
        #[setter(name = "valueType", coerce, hint(js(ce_reactions)))]
        fn set_value_type(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("valuetype", value)
        }
    };
    DomHtmlPreElement, "HTMLPreElement" => ["listing", "pre", "xmp"] {
        #[getter(name="width")]
        fn width(&self)->OpResult<i32> {Ok(self.base.base.base.get_null_attribute("width")?.as_deref().and_then(lumen_common::html_numbers::parse_integer_i32).unwrap_or(0))}
        #[setter(name="width",coerce,hint(js(ce_reactions)))]
        fn set_width(&self,value:i32)->OpResult<()> {self.base.base.base.set_attribute_core("width",&value.to_string())}
    };
    DomHtmlProgressElement, "HTMLProgressElement" => ["progress"] {
        #[getter]
        fn value(&self) -> OpResult<f64> { Ok(progress_state(&self.base.base.base)?.value) }
        #[setter(coerce, hint(js(ce_reactions)))]
        fn set_value(&self, ctx: &mut Ctx, value: f64) -> OpResult<()> { set_reflected_double(&self.base.base.base, ctx, "value", value, false) }
        #[getter]
        fn max(&self) -> OpResult<f64> { Ok(progress_state(&self.base.base.base)?.maximum) }
        #[setter(coerce, hint(js(ce_reactions)))]
        fn set_max(&self, ctx: &mut Ctx, value: f64) -> OpResult<()> { set_reflected_double(&self.base.base.base, ctx, "max", value, true) }
        #[getter]
        fn position(&self) -> OpResult<f64> { Ok(progress_state(&self.base.base.base)?.position) }
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
    DomHtmlQuoteElement, "HTMLQuoteElement" => ["blockquote", "q"] {
        #[getter(name="cite")]
        fn cite(&self)->OpResult<String> {reflected_usv_url_value(&self.base.base.base,"cite")}
        #[setter(name="cite",hint(js(ce_reactions)))]
        fn set_cite(&self,value:lumen_host::webidl::Usv)->OpResult<()> {self.base.base.base.set_attribute_core("cite",&value.0)}
    };
    DomHtmlSourceElement, "HTMLSourceElement" => ["source"] {
        #[getter(name = "src")]
        fn src(&self) -> OpResult<String> {
            reflected_usv_url_value(&self.base.base.base, "src")
        }
        #[setter(name = "src", hint(js(ce_reactions)))]
        fn set_src(&self, value: lumen_host::webidl::Usv) -> OpResult<()> {
            self.base.base.base.set_attribute_core("src", &value.0)
        }
        #[getter(name = "type")]
        fn kind(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("type")?.unwrap_or_default())
        }
        #[setter(name = "type", coerce, hint(js(ce_reactions)))]
        fn set_kind(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("type", value)
        }
        #[getter(name = "sizes")]
        fn sizes(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("sizes")?.unwrap_or_default())
        }
        #[setter(name = "sizes", coerce, hint(js(ce_reactions)))]
        fn set_sizes(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("sizes", value)
        }
        #[getter(name = "media")]
        fn media(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("media")?.unwrap_or_default())
        }
        #[setter(name = "media", coerce, hint(js(ce_reactions)))]
        fn set_media(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("media", value)
        }
        #[getter]
        fn srcset(&self) -> OpResult<String> { reflected_usv_value(&self.base.base.base, "srcset") }
        #[setter(hint(js(ce_reactions)))]
        fn set_srcset(&self, value: lumen_host::webidl::Usv) -> OpResult<()> { self.base.base.base.set_attribute_core("srcset", &value.0) }
        #[getter]
        fn width(&self) -> OpResult<u32> {
            let value = self.base.base.base.get_null_attribute("width")?;
            Ok(lumen_html::forms::reflected_unsigned_long(value.as_deref(), 0))
        }
        #[setter(coerce, hint(js(ce_reactions)))]
        fn set_width(&self, value: u32) -> OpResult<()> {
            let value = lumen_html::forms::reflected_unsigned_long_setter_value(value, 0);
            self.base.base.base.set_attribute_core("width", &value.to_string())
        }
        #[getter]
        fn height(&self) -> OpResult<u32> {
            let value = self.base.base.base.get_null_attribute("height")?;
            Ok(lumen_html::forms::reflected_unsigned_long(value.as_deref(), 0))
        }
        #[setter(coerce, hint(js(ce_reactions)))]
        fn set_height(&self, value: u32) -> OpResult<()> {
            let value = lumen_html::forms::reflected_unsigned_long_setter_value(value, 0);
            self.base.base.base.set_attribute_core("height", &value.to_string())
        }
    };
    DomHtmlSpanElement, "HTMLSpanElement" => ["span"];
    DomHtmlTimeElement, "HTMLTimeElement" => ["time"] {
        #[getter(name = "dateTime")]
        fn date_time(&self) -> OpResult<String> {
            Ok(self.base.base.base.get_null_attribute("datetime")?.unwrap_or_default())
        }
        #[setter(name = "dateTime", coerce, hint(js(ce_reactions)))]
        fn set_date_time(&self, value: &str) -> OpResult<()> {
            self.base.base.base.set_attribute_core("datetime", value)
        }
    };
    DomHtmlTrackElement, "HTMLTrackElement" => ["track"] {
            #[getter(name = "src")]
            fn src(&self) -> OpResult<String> { crate::html_interfaces::reflected_usv_url_value(&self.base.base.base, "src") }
            #[setter(name = "src", hint(js(ce_reactions)))]
            fn set_src(&self, value: lumen_host::webidl::Usv) -> OpResult<()> { (&self.base.base.base).set_attribute_core("src", &value.0) }
            #[getter(name = "srclang")]
            fn srclang(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("srclang")?.unwrap_or_default()) }
            #[setter(name = "srclang", coerce, hint(js(ce_reactions)))]
            fn set_srclang(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("srclang", value) }
            #[getter(name = "label")]
            fn label(&self) -> OpResult<String> { Ok((&self.base.base.base).get_null_attribute("label")?.unwrap_or_default()) }
            #[setter(name = "label", coerce, hint(js(ce_reactions)))]
            fn set_label(&self, value: &str) -> OpResult<()> { (&self.base.base.base).set_attribute_core("label", value) }
            #[getter(name = "default")]
            fn default_(&self) -> OpResult<bool> { Ok((&self.base.base.base).get_null_attribute("default")?.is_some()) }
            #[setter(name = "default", coerce, hint(js(ce_reactions)))]
            fn set_default_(&self, value: bool) -> OpResult<()> { (&self.base.base.base).set_nullable_attribute_core("default", value.then_some("")) }
        #[getter]
        fn kind(&self) -> OpResult<String> {
            reflected_keyword(&self.base.base.base, "kind", &["subtitles","captions","descriptions","chapters","metadata"], "subtitles", "metadata")
        }
        #[setter(coerce, hint(js(ce_reactions)))]
        fn set_kind(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("kind", value) }
    };
    DomHtmlUListElement, "HTMLUListElement" => ["ul"] {
        #[getter(name="compact")]
        fn compact(&self)->OpResult<bool> {Ok(self.base.base.base.get_null_attribute("compact")?.is_some())}
        #[setter(name="compact",coerce,hint(js(ce_reactions)))]
        fn set_compact(&self,value:bool)->OpResult<()> {self.base.base.base.set_nullable_attribute_core("compact",value.then_some(""))}
        #[getter(name="type")]
        fn type_(&self)->OpResult<String> {Ok(self.base.base.base.get_null_attribute("type")?.unwrap_or_default())}
        #[setter(name="type",coerce,hint(js(ce_reactions)))]
        fn set_type_(&self,value:&str)->OpResult<()> {self.base.base.base.set_attribute_core("type",value)}
    };
}

/// HTML reflected URL attributes resolve against the adopted owner's base.
fn progress_state(node: &DomNode) -> OpResult<lumen_html::forms::ProgressState> {
    let (owner, id) = node.realm.resolve_adopted_node(node.id);
    let session = owner.session.borrow();
    lumen_html::forms::progress_state(session.document(), id)
        .ok_or_else(|| OpError::type_error("Expected an HTML progress element"))
}

fn meter_state(node: &DomNode) -> OpResult<lumen_html::forms::MeterState> {
    let (owner, id) = node.realm.resolve_adopted_node(node.id);
    let session = owner.session.borrow();
    lumen_html::forms::meter_state(session.document(), id)
        .ok_or_else(|| OpError::type_error("Expected an HTML meter element"))
}

fn set_reflected_double(node: &DomNode, ctx: &mut Ctx, attribute: &str, value: f64, positive: bool) -> OpResult<()> {
    // WebIDL double conversion rejects nonfinite numbers before either the
    // positive-only no-op or the ordinary mutation/CE reaction authority.
    if !value.is_finite() { return Err(OpError::type_error("Expected a finite double")); }
    if positive && value <= 0.0 { return Ok(()); }
    let serialized = ctx.coerce_string(&Value::Num(value))?;
    node.set_attribute_core(attribute, &serialized)
}

pub(super) fn reflected_cors(node: &DomNode) -> OpResult<lumen::embed::Nullable<String>> {
    Ok(lumen::embed::Nullable(node.get_null_attribute("crossorigin")?.map(|value|
        if value.eq_ignore_ascii_case("use-credentials") { "use-credentials".into() } else { "anonymous".into() })))
}

pub(super) fn reflected_keyword(node: &DomNode, attribute: &str, keywords: &[&str], missing: &str, invalid: &str) -> OpResult<String> {
    let value = node.get_null_attribute(attribute)?;
    Ok(match value {
        None => missing,
        Some(ref value) => keywords.iter().copied().find(|keyword| value.eq_ignore_ascii_case(keyword)).unwrap_or(invalid),
    }.into())
}

pub(super) fn reflected_url_value(node:&DomNode,attribute:&str)->OpResult<String> {
    let Some(value)=node.get_null_attribute(attribute)? else{return Ok(String::new());};
    let (owner,_)=node.realm.resolve_adopted_node(node.id);
    Ok(lumen_common::url::parse(&value,Some(&owner.base_url())).map(|url|url.href()).unwrap_or(value))
}

pub(super) fn reflected_usv_value(node:&DomNode,attribute:&str)->OpResult<String> {
    let value=node.get_null_attribute(attribute)?.unwrap_or_default();
    match lumen::well_formed_utf8(&value) {
        std::borrow::Cow::Borrowed(_)=>Ok(value),
        std::borrow::Cow::Owned(value)=>Ok(value),
    }
}

pub(super) fn reflected_usv_url_value(node:&DomNode,attribute:&str)->OpResult<String> {
    let Some(value)=node.get_null_attribute(attribute)? else{return Ok(String::new());};
    let value=lumen::well_formed_utf8(&value);
    let (owner,_)=node.realm.resolve_adopted_node(node.id);
    Ok(lumen_common::url::parse(&value,Some(&owner.base_url())).map(|url|url.href()).unwrap_or_else(|_|value.into_owned()))
}

pub(crate) fn form_action_value(node: &DomNode) -> OpResult<String> {
    action_attribute_value(node, "formaction")
}

pub(crate) fn action_attribute_value(node: &DomNode, attribute: &str) -> OpResult<String> {
    let (owner, _) = node.realm.resolve_adopted_node(node.id);
    let Some(action) = node.get_null_attribute(attribute)? else {
        return Ok(owner
            .document_url()
            .unwrap_or_else(|| "about:blank".into()));
    };
    if action.is_empty() {
        return Ok(owner
            .document_url()
            .unwrap_or_else(|| "about:blank".into()));
    }
    let action = lumen::well_formed_utf8(&action);
    let base = owner.base_url();
    Ok(lumen_common::url::parse(&action, Some(&base))
        .map(|url| url.href())
        .unwrap_or_else(|_| action.into_owned()))
}

pub(crate) fn form_method_value(node: &DomNode) -> OpResult<String> {
    Ok(node.get_null_attribute("formmethod")?
        .map_or_else(String::new, |value| lumen_html::forms::normalized_form_method(&value).into()))
}

pub(crate) fn form_enctype_value(node: &DomNode) -> OpResult<String> {
    Ok(node.get_null_attribute("formenctype")?
        .map_or_else(String::new, |value| lumen_html::forms::normalized_form_enctype(&value).into()))
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
    #[constructor]
    fn new(ctx: &mut Ctx, this: lumen_bind::This<Value>) -> OpResult<crate::custom_elements::HtmlElementCtor> {
        crate::custom_elements::construct_customized_class::<Self>(ctx, this.0)
    }
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

    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_value(&self, value: &str) -> OpResult<()> {
        let node = &self.base.base.base;
        super::forms::set_output_value(&node.realm, node.id, value)
    }

    #[getter(name = "defaultValue")]
    fn default_value(&self) -> OpResult<String> {
        let node = &self.base.base.base;
        super::forms::output_default_value(&node.realm, &node.realm.forms.borrow(), node.id)
    }

    #[setter(name = "defaultValue", coerce, hint(js(ce_reactions)))]
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

    #[setter(coerce, hint(js(ce_reactions)))]
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

    #[test]
    fn specification_html_metadata_reflection_uses_idl_conversion_and_shared_reactions() {
        let mut engine=Engine::new();
        let _realm=crate::install(engine.ctx(),"<table id=t><tbody><tr><td id=c></td></tr></tbody></table><dialog id=d></dialog>",256).unwrap();
        assert!(matches!(eval(&mut engine,r#"(() => {
            const check=(ok,message)=>{if(!ok)throw Error(message)},t=document.getElementById('t'),c=document.getElementById('c');
            check(t.title==='' && t.accessKey==='' && t.accessKeyLabel==='','unset metadata');
            t.setAttributeNS('urn:foreign','x:title','ignored');check(t.title==='','null namespace reflection');
            let converted=0;
            t.title={toString(){converted++;return 'author title'}};
            check(converted===1 && t.getAttribute('title')==='author title','DOMString conversion occurs once');
            t.accessKey=null;check(t.getAttribute('accesskey')==='null','null uses DOMString conversion');
            t.accessKey=' k ';check(t.accessKey===' k ','reflection preserves raw tokens');
            t.autofocus={toString(){throw Error('boolean must not stringify')}};
            check(t.autofocus && t.getAttribute('autofocus')==='','WebIDL boolean uses truthiness');
            t.autofocus=undefined;check(!t.hasAttribute('autofocus'),'undefined is false');
            c.noWrap='yes';check(c.noWrap && c.hasAttribute('nowrap'),'legacy reflected boolean');
            c.noWrap=0;check(!c.noWrap && !c.hasAttribute('nowrap'),'zero removes boolean');
            const d=document.getElementById('d');d.open='yes';check(d.open,'dialog IDL boolean');d.open=null;check(!d.open,'dialog false conversion');
            const calls=[];
            class MetadataElement extends HTMLElement {
                static observedAttributes=['title','accesskey','autofocus'];
                attributeChangedCallback(name,old,value){calls.push(name+':'+value)}
            }
            customElements.define('x-metadata',MetadataElement);
            const element=document.createElement('x-metadata');element.title='named';element.accessKey='a';element.autofocus=true;
            check(calls.join(',')==='title:named,accesskey:a,autofocus:','actual shared CE reactions');
            const foreign=document.implementation.createHTMLDocument('other');foreign.adoptNode(element);
            element.title='adopted';check(element.ownerDocument===foreign && element.getAttribute('title')==='adopted','adopted reflection follows real owner');
            return true;
        })()"#),Value::Bool(true)));
    }

    #[test]
    fn specification_button_value_reflects_attribute_and_customized_reactions() {
        let mut engine=Engine::new();
        let _realm=crate::install(engine.ctx(),"<form id=f><button id=b name=action>label</button></form>",256).unwrap();
        assert!(matches!(eval(&mut engine,r#"(() => {
            const check=(value,label)=>{if(!value)throw Error(label)};
            const button=document.getElementById('b');
            check(button.value==='' && !button.hasAttribute('value'),'missing value');
            button.value='outer';check(button.value==='outer' && button.getAttribute('value')==='outer','IDL reflects content attribute');
            button.setAttribute('value','author');check(button.value==='author','attribute reflects IDL');
            document.getElementById('f').reset();check(button.value==='author','reset has no dirty value sidecar');
            button.value=null;check(button.value==='null','DOMString null conversion');
            button.removeAttribute('value');check(button.value==='','removed value');
            button.setAttributeNS('urn:test','t:value','foreign');check(button.value==='','null namespace only');
            const events=[];
            class ReactiveButton extends HTMLButtonElement {
                static observedAttributes=['value'];
                attributeChangedCallback(name,before,after){events.push([name,before,after])}
            }
            customElements.define('x-value-button',ReactiveButton,{extends:'button'});
            const customized=document.createElement('button',{is:'x-value-button'});
            customized.value='reactive';
            check(customized.getAttribute('value')==='reactive' && events.length===1 && events[0].join(',')==='value,,reactive','shared synchronous CE reactions');
            return true;
        })()"#),Value::Bool(true)));
    }

    // /html/rendering/non-replaced-elements/the-fieldset-and-legend-elements/legend-align-justify-self.html
    #[test]
    fn specification_legend_align_reflection_uses_shared_conversion_reactions_and_css_hint() {
        let mut engine=Engine::new();
        let _realm=crate::install(engine.ctx(),"<fieldset><legend id=l>x</legend></fieldset>",256).unwrap();
        assert!(matches!(eval(&mut engine,r#"(() => {
            const check=(ok,message)=>{if(!ok)throw Error(message)};
            const legend=document.getElementById('l');
            check(legend.align==='' && getComputedStyle(legend).justifySelf==='auto','initial reflection and CSS endpoint');
            for(const [raw,expected] of [['left','left'],['center','center'],['right','right'],['lEfT','left'],['cEnTeR','center'],['rIgHt','right'],['justify','auto'],['left ','auto']]){
                legend.align=raw;
                check(legend.getAttribute('align')===raw && legend.align===raw,'raw DOMString is preserved');
                check(getComputedStyle(legend).justifySelf===expected,'live presentational hint '+raw);
            }
            let conversions=0;
            legend.align={toString(){conversions++;return 'center'}};
            check(conversions===1 && legend.align==='center','shared DOMString conversion occurs once');
            legend.align=null;check(legend.align==='null','null is a DOMString token');
            legend.align=undefined;check(legend.align==='undefined','undefined is a DOMString token');
            legend.removeAttribute('align');
            legend.setAttributeNS('urn:foreign','foreign:align','right');
            check(legend.align==='' && getComputedStyle(legend).justifySelf==='auto','foreign attribute does not enter reflection or hints');
            legend.setAttribute('align','left');check(legend.align==='left','content attribute updates IDL');
            const events=[];
            class ReactiveLegend extends HTMLLegendElement {
                static observedAttributes=['align'];
                attributeChangedCallback(name,before,after){events.push([name,before,after])}
            }
            customElements.define('x-aligned-legend',ReactiveLegend,{extends:'legend'});
            const customized=document.createElement('legend',{is:'x-aligned-legend'});
            customized.align='center';
            check(customized.getAttribute('align')==='center' && events.length===1 && events[0].join(',')==='align,,center','shared synchronous CE reactions');
            return true;
        })()"#),Value::Bool(true)));
    }

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
    fn specification_hidden_inert_reflection_and_rendered_focus_fixup() {
        let mut engine = Engine::new();
        let realm = crate::install(engine.ctx(), "<body><div id=source><button>focus</button></div><div id=target></div></body>", 128).unwrap();
        let result = eval(&mut engine, r#"(() => {
            const check=(ok,message)=>{if(!ok)throw new Error(message)};
            const source=document.getElementById('source'),target=document.getElementById('target');
            check(target.hidden===false&&!target.inert,'missing attributes');
            target.hidden='UNTIL-FOUND';check(target.hidden==='until-found'&&target.getAttribute('hidden')==='until-found','until-found state');
            for(const value of [false,null,undefined,0,-0,NaN,'']) {
                target.hidden=true;target.hidden=value;
                check(target.hidden===false&&!target.hasAttribute('hidden'),'hidden removal');
            }
            for(const value of [true,1,-1,Infinity,'false',{},0n]) {
                target.hidden=value;check(target.hidden===true&&target.getAttribute('hidden')==='','hidden state');
            }
            target.hidden=false;
            const failure={};try{target.hidden={toString(){throw failure}};throw new Error('missing conversion exception')}catch(error){check(error===failure,'conversion exception identity')}
            const button=source.firstChild;button.focus();target.inert=true;
            check(target.inert&&target.hasAttribute('inert'),'inert reflection');
            target.moveBefore(button,null);check(document.activeElement===button,'synchronous move focus');
            return true;
        })()"#);
        assert!(matches!(result, Value::Bool(true)));
        realm.update_rendered_focus(engine.ctx()).unwrap();
        assert!(matches!(eval(&mut engine, "document.activeElement===document.body"), Value::Bool(true)));
        assert!(matches!(eval(&mut engine, "document.getElementById('target').inert=false; !document.getElementById('target').hasAttribute('inert')"), Value::Bool(true)));
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
