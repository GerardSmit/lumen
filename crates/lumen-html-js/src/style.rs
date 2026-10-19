use super::*;
use std::sync::Arc;
use lumen_html::{
    css,
    paint::FontSizeAdjustValue,
};

#[lumen_bind::class(name = "CSSStyleDeclaration", hint(js(webidl)))]
pub struct DomStyle {
    pub(crate) realm: Rc<DomRealm>,
    pub(crate) node: NodeId,
    pub(crate) computed: bool,
    pub(crate) pseudo: Option<css::PseudoElement>,
    pub(crate) invalid_pseudo: bool,
    pub(crate) _owner: Value,
}

impl DomStyle {
    fn raw_property_value(&self, name: &str) -> OpResult<String> {
        self.property_value(name, true)
    }

    fn property_value(&self, name: &str, resolved: bool) -> OpResult<String> {
        if self.computed {
            self.realm.synchronize_embedding_color_scheme()?;
            if resolved && !self.computed_declarations_available()? { return Ok(String::new()); }
            let property = if !name.starts_with("--") && name.bytes().any(|byte| byte.is_ascii_uppercase()) {
                std::borrow::Cow::Owned(name.to_ascii_lowercase())
            } else { std::borrow::Cow::Borrowed(name) };
            let needs_layout = resolved && css::computed_values::needs_layout(&property);
            let pending_fonts=if computed_font_property(&property) {
                Some(super::canvas::realm_font_source(&self.realm)?)
            } else {super::canvas::initialized_realm_font_source(&self.realm)?};
            let needs_container_context = if needs_layout { false } else {
                let mut session = self.realm.session.borrow_mut();
                let inherited = css::property_inherits_query_context(&property);
                let mut current = Some(self.node);
                let mut pending = false;
                for _ in 0..512 {
                    let Some(node) = current else { break; };
                    if matches!(session.document().kind(node), Ok(NodeKind::Element { .. })) && session
                        .computed_style_with_text(node,pending_fonts.as_ref().map(|fonts|fonts as &dyn lumen_html::paint::TextShaper))
                        .map_err(|error| OpError::new("InvalidStateError", format!("computed style failed: {error:?}")))?
                        .property_query_context_pending(&property) {
                        pending = true;
                        break;
                    }
                    if !inherited { break; }
                    current = session.document().composed_parent(node).map_err(dom_error)?;
                }
                pending
            };
            if (needs_layout || needs_container_context)
                && (self.realm.layout_flusher.borrow().is_some()
                    || self.realm.session.borrow().viewport_size().is_some())
            {
                self.realm.flush_layout()?;
            }
            let fonts = if computed_font_property(&property) {
                Some(super::canvas::realm_font_source(&self.realm)?)
            } else {super::canvas::initialized_realm_font_source(&self.realm)?};
            let (style, mut context, grid_tracks,timeline_source) = {
                let mut session = self.realm.session.borrow_mut();
                let style = self.resolved_style(&mut session,fonts.as_ref())
                    .map_err(|error| OpError::new("InvalidStateError", format!("computed style failed: {error:?}")))?;
                let margin = property.starts_with("margin");
                let used_margins = (self.pseudo.is_none() && needs_layout && margin).then(|| session.layout_used_margins(self.node)).flatten();
                let (border_box, replaced_element) = if needs_layout && !margin {
                    match self.pseudo {
                        Some(pseudo) => (session.pseudo_layout_rect(self.node,pseudo), false),
                        None => session.layout_rect_with_replacement(self.node)
                            .map_or((None,false), |(rect,replaced)|(Some(rect),replaced)),
                    }
                } else {(None,false)};
                let placeholder_line_origin = if self.pseudo == Some(css::PseudoElement::Placeholder)
                    && matches!(property.as_ref(), "line-height" | "font") {
                    Some(session.computed_style_with_text(self.node,
                        fonts.as_ref().map(|font| font as &dyn lumen_html::paint::TextShaper))
                        .map_err(|error|OpError::new("InvalidStateError",format!("control line style failed: {error:?}")))?)
                } else { None };
                let context = css::computed_values::ComputedValueContext {
                    resolved,
                    border_box,
                    transform_reference_box: if needs_layout {session.layout_transform_reference_box(self.node,self.pseudo)
                        .map(|(rect,svg)|if svg {rect}else{style.css_transform_reference_box(rect)})}else{None},
                    percentage_basis: (needs_layout && used_margins.is_none()).then(|| match self.pseudo {Some(pseudo)=>session.pseudo_layout_percentage_basis(self.node,pseudo),None=>session.layout_percentage_basis(self.node)}).flatten(),
                    used_margins,
                    used_line_height: if resolved && matches!(self.pseudo,None|Some(css::PseudoElement::Placeholder)) && matches!(property.as_ref(), "line-height" | "font") {
                        fonts.as_ref().and_then(|text| {
                            let origin = placeholder_line_origin.as_ref().unwrap_or(&style);
                            if self.pseudo == Some(css::PseudoElement::Placeholder) {
                                lumen_html::layout::placeholder_used_line_height(session.document(), self.node, origin, text)
                            } else { lumen_html::layout::text_entry_used_line_height(session.document(), self.node, origin, text) }
                        })
                    } else {None},
                    replaced_element: self.pseudo.is_none() && replaced_element,
                    used_display: if needs_layout && self.pseudo.is_none() {session.layout_used_display(self.node)} else {None},
                    ..Default::default()
                };
                let grid_tracks = if resolved && matches!(property.as_ref(), "grid-template-columns" | "grid-template-rows" | "grid-template" | "grid" | "grid-lanes") { session.layout_grid_tracks(self.node) } else { None };
                let timeline_source=if resolved&&property.as_ref()=="transform"{session.used_transform_timeline_source(self.node,self.pseudo,&style)}else{None};
                (style, context, grid_tracks,timeline_source)
            };
            context.transform_timeline_source=timeline_source.as_deref();
            if let Some((columns, rows, column_names, row_names)) = &grid_tracks {
                context.grid_columns = (!columns.is_empty()).then_some(columns.as_ref());
                context.grid_rows = (!rows.is_empty()).then_some(rows.as_ref());
                context.grid_column_names = (!columns.is_empty()).then_some(column_names.as_ref());
                context.grid_row_names = (!rows.is_empty()).then_some(row_names.as_ref());
            }
            if property == "font-size-adjust" {
                if let Some(adjust) = style.font_spec().size_adjust.filter(|adjust| matches!(adjust.value, FontSizeAdjustValue::FromFont)) {
                    self.realm.font_loading.request_metric_font(style.font_spec());
                    context.primary_font_metric = self.realm.font_loading.primary_metric(style.font_spec(), adjust.metric);
                }
            }
            return Ok(style.computed_css_value(&property, context).unwrap_or_default());
        }
        Ok(self
            .declaration_value(name)?
            .map_or(String::new(), |(value, _)| {
                canonical_content_property(name, value)
            }))
    }
    fn resolved_style(&self, session: &mut lumen_html::session::RenderSession, fonts:Option<&super::canvas::CanvasFontSource>) -> Result<css::Style, lumen_html::layout::LayoutError> {
        let text=fonts.map(|fonts|fonts as &dyn lumen_html::paint::TextShaper);
        match self.pseudo {
            Some(pseudo) => session.computed_pseudo_style_with_text(self.node, pseudo,text),
            None => session.computed_style_with_text(self.node,text),
        }
    }
    fn computed_declarations_available(&self) -> OpResult<bool> {
        if self.invalid_pseudo { return Ok(false); }
        rendered_ancestry(&self.realm, self.node, false)
    }

    fn declaration_value(&self, name: &str) -> OpResult<Option<(String, bool)>> {
        Ok(self.read_declaration_block()?.value(name))
    }

    pub(crate) fn adopt_node(&mut self, realm: Rc<DomRealm>, node: NodeId) {
        self.realm = realm;
        self.node = node;
    }
}

/// Shared connected/rendered owner-chain eligibility. CSSOM checks embedding
/// containers; resource algorithms additionally require the element's own box.
pub(crate) fn rendered_ancestry(owner: &Rc<DomRealm>, node: NodeId, check_element: bool) -> OpResult<bool> {
    let mut realm = owner.clone();
    let mut node = node;
    let mut container = check_element;
        loop {
            let Some(context) = realm.browsing_context() else { return Ok(false); };
            if !browsing_context::is_active_document(&context, &realm) {
                return Ok(false);
            }
            {
                let fonts=super::canvas::initialized_realm_font_source(&realm)?;
                let mut session = realm.session.borrow_mut();
                let root = session.document().root();
                let mut current = node;
                while current != root {
                    if container && matches!(session.document().kind(current).map_err(dom_error)?, NodeKind::Element { .. }) {
                        let style = session.computed_style_with_text(current,fonts.as_ref().map(|fonts|fonts as &dyn lumen_html::paint::TextShaper)).map_err(|error| OpError::new("InvalidStateError", format!("computed style failed: {error:?}")))?;
                        if style.display == css::Display::None || (current == node && style.display == css::Display::Contents) {
                            return Ok(false);
                        }
                    }
                    let Some(parent) = session.document().flat_tree_parent(current).map_err(dom_error)? else { return Ok(false); };
                    current = parent;
                }
            }
            let Some((_, owner, frame)) = browsing_context::context_frame_element(&context) else {
                browsing_context::synchronize_frame_media_environment(owner)?;
                return Ok(true);
            };
            realm = owner;
            node = frame;
            container = true;
        }
}

pub(crate) fn computed_property_value(
    realm: &Rc<DomRealm>,
    node: NodeId,
    property: &str,
) -> OpResult<String> {
    DomStyle {
        realm: realm.clone(),
        node,
        computed: true,
        pseudo: None,
        invalid_pseudo: false,
        _owner: Value::Undefined,
    }
    .property_value(property, false)
}

/// One operation's canonical computed style and registration epoch. Iteration
/// reads every longhand, so font and rare query context are initialized once.
pub(crate) struct ComputedPropertyMapSnapshot {
    pub style: css::Style,
    pub registrations: Option<Arc<[css::registered_properties::RegisteredCustomProperty]>>,
    pub context: css::computed_values::ComputedValueContext<'static>,
}
pub(crate) fn computed_property_map_snapshot(realm: &Rc<DomRealm>, node: NodeId)
    -> OpResult<Option<ComputedPropertyMapSnapshot>> {
    let (realm,node)=realm.resolve_adopted_node(node);
    realm.synchronize_embedding_color_scheme()?;
    if !rendered_ancestry(&realm,node,false)? {return Ok(None);}
    let fonts=super::canvas::realm_font_source(&realm)?;
    let read=|session:&mut lumen_html::session::RenderSession|session
        .computed_style_with_text(node,Some(&fonts as &dyn lumen_html::paint::TextShaper))
        .map_err(|error|OpError::new("InvalidStateError",format!("computed style failed: {error:?}")));
    let mut style=read(&mut realm.session.borrow_mut())?;
    if style.has_query_container_dependencies()
        && (realm.layout_flusher.borrow().is_some() || realm.session.borrow().viewport_size().is_some()) {
        realm.flush_layout()?;
        style=read(&mut realm.session.borrow_mut())?;
    }
    let mut context=css::computed_values::ComputedValueContext::default();
    if let Some(adjust)=style.font_spec().size_adjust.filter(|adjust|
        matches!(adjust.value,FontSizeAdjustValue::FromFont)) {
        realm.font_loading.request_metric_font(style.font_spec());
        context.primary_font_metric=realm.font_loading.primary_metric(style.font_spec(),adjust.metric);
    }
    let registrations=realm.session.borrow().registered_custom_property_snapshot();
    Ok(Some(ComputedPropertyMapSnapshot {style,registrations,context}))
}

fn computed_font_property(property: &str) -> bool {
    static PROPERTIES: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    PROPERTIES.get_or_init(|| css::cssom_property_names().into_iter().filter(|name|
        css::supports_property_value(name, "1ch") || css::supports_property_value(name, "1ch 1ch") || *name == "font"
    ).collect()).contains(&property)
}

pub(super) fn css_error(error: css::CssError) -> OpError {
    OpError::new(
        "InvalidStateError",
        format!("CSS error at {}: {}", error.offset, error.message),
    )
}

pub(crate) fn canonical_content_property(name: &str, value: String) -> String {
    // CSS-wide keywords and deferred variable values use the shared declaration
    // parser's validated representation rather than generated-content grammar.
    css::serialize_cssom_property_value(name, &value).unwrap_or(value)
}

/// Install camel-case CSSStyleDeclaration aliases from the shared CSS
/// property registry. Accessors delegate through the receiver's CSSOM methods,
/// so stylesheet rule declarations and inline styles use the same path.
pub(crate) fn install_css_property_aliases(ctx: &mut Ctx) -> OpResult<()> {
    static ALIASES: std::sync::OnceLock<Vec<(&'static str, Option<String>, Option<String>)>> =
        std::sync::OnceLock::new();
    let aliases = ALIASES.get_or_init(|| {
        css::cssom_property_names()
            .into_iter()
            .filter(|property| !property.starts_with("--"))
            .map(|property| (property, camel_case_alias(property),
                property.starts_with("-webkit-").then(|| css_property_to_idl(property, true))))
            .collect()
    });
    let constructor = ctx.class_constructor::<DomStyle>();
    let prototype = ctx
        .get_member(&constructor, "prototype")
        .map_err(|_| OpError::new("Error", "CSSStyleDeclaration prototype unavailable"))?;
    for (property, camel, webkit) in aliases {
        define_css_property_accessor(ctx, &prototype, property, property);
        if let Some(camel) = camel {
            define_css_property_accessor(ctx, &prototype, camel, property);
        }
        if let Some(webkit) = webkit {
            define_css_property_accessor(ctx, &prototype, webkit, property);
        }
    }
    Ok(())
}

fn camel_case_alias(property: &str) -> Option<String> {
    let out = css_property_to_idl(property, false);
    (out != property).then_some(out)
}

// CSSOM's CSS-property-to-IDL algorithm, including the lowercase-first flag
// used for every supported -webkit- property. Names come only from the shared
// property registry; canonical declaration identity stays in DeclarationBlock.
fn css_property_to_idl(property: &str, lowercase_first: bool) -> String {
    let property = if lowercase_first { &property[1..] } else { property };
    let mut out = String::with_capacity(property.len());
    let mut uppercase_next = false;
    for c in property.chars() {
        if c == '-' { uppercase_next = true; }
        else if uppercase_next { uppercase_next = false; out.push(c.to_ascii_uppercase()); }
        else { out.push(c); }
    }
    out
}

fn define_css_property_accessor(
    ctx: &mut Ctx,
    prototype: &Value,
    name: &str,
    property: &'static str,
) {
    let getter = ctx.new_native_fn(
        "get",
        0,
        std::rc::Rc::new(move |ctx: &mut Ctx, this: Value, _args: &[Value]| {
            let method = ctx
                .get_member(&this, "getPropertyValue")
                .map_err(lumen::embed::abrupt_value)?;
            ctx.invoke(method, this, &[Value::str(property)])
        }),
    );
    let setter = ctx.new_native_fn(
        "set",
        1,
        std::rc::Rc::new(move |ctx: &mut Ctx, this: Value, args: &[Value]| {
            let method = ctx
                .get_member(&this, "setProperty")
                .map_err(lumen::embed::abrupt_value)?;
            let value = match args.first() {
                Some(Value::Null) => Value::str(""),
                Some(value) => value.clone(),
                None => Value::Undefined,
            };
            ctx.invoke(method, this, &[Value::str(property), value])?;
            Ok(Value::Undefined)
        }),
    );
    ctx.define_accessor_value(prototype, name, Some(getter), Some(setter), true);
}

impl DomStyle {
    fn read_declaration_block(&self) -> OpResult<Rc<css::DeclarationBlock>> {
        let mut session = self.realm.session.borrow_mut();
        if !matches!(session.document().kind(self.node).map_err(dom_error)?, NodeKind::Element { .. }) {
            return Err(OpError::new("TypeError", "style requires an element"));
        }
        session.document_mut().read_inline_cssom_style(self.node).map_err(css_error)?
            .ok_or_else(|| OpError::new("TypeError", "style requires an element"))
    }
    pub(crate) fn declaration_block(&self) -> OpResult<css::DeclarationBlock> {
        Ok(self.read_declaration_block()?.as_ref().clone())
    }
    fn raw(&self) -> OpResult<String> {
        self.read_declaration_block()?.serialize().map_err(css_error)
    }
    pub(crate) fn write_block(&self, block: css::DeclarationBlock) -> OpResult<()> {
        if self.computed {
            return Err(OpError::new(
                "NoModificationAllowedError",
                "computed style is read-only",
            ));
        }
        self.realm
            .session
            .borrow_mut()
            .document_mut()
            .set_inline_cssom_style(self.node, block)
            .map_err(dom_error)
    }
}

pub(crate) fn computed_property_names() -> &'static [&'static str] {
    static NAMES: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    NAMES.get_or_init(|| {
        let mut names: Vec<_> = css::cssom_property_names().into_iter()
            .filter(|name| !css::declaration_block::is_shorthand(name) && css::declaration_block::canonical_alias(name)==*name).collect();
        names.sort_unstable();
        names.dedup();
        names
    })
}

impl DomStyle {
    fn property_name(&self, index: usize) -> OpResult<Option<String>> {
        if self.computed {
            if !self.computed_declarations_available()? { return Ok(None); }
            let names = computed_property_names();
            if let Some(name) = names.get(index) { return Ok(Some((*name).to_owned())); }
            let fonts=super::canvas::initialized_realm_font_source(&self.realm)?;
            let style = self.resolved_style(&mut self.realm.session.borrow_mut(),fonts.as_ref())
                .map_err(|error| OpError::new("InvalidStateError", format!("computed style failed: {error:?}")))?;
            return Ok(style.custom_properties().iter().filter(|(_, value)| value.is_some())
                .nth(index - names.len()).map(|(name, _)| name.clone()));
        }
        Ok(self.read_declaration_block()?.names().nth(index).map(str::to_owned))
    }
}

#[lumen_bind::methods]
impl DomStyle {
    #[proto(len)]
    fn length(&self,ctx:&mut Ctx) -> OpResult<usize> {
        if self.computed {
            crate::animations::flush_css_transitions_for(ctx,&self.realm,Some(self.node))?;
            if !self.computed_declarations_available()? { return Ok(0); }
            let fonts=super::canvas::initialized_realm_font_source(&self.realm)?;
            let style = self.resolved_style(&mut self.realm.session.borrow_mut(),fonts.as_ref())
                .map_err(|error| OpError::new("InvalidStateError", format!("computed style failed: {error:?}")))?;
            return Ok(computed_property_names().len() + style.custom_properties().iter()
                .filter(|(_, value)| value.is_some()).count());
        }
        Ok(self.read_declaration_block()?.len())
    }

    #[method(coerce)]
    fn item(&self,ctx:&mut Ctx, index: usize) -> OpResult<String> {
        if self.computed {crate::animations::flush_css_transitions_for(ctx,&self.realm,Some(self.node))?;}
        Ok(self.property_name(index)?.unwrap_or_default())
    }

    #[proto(iter)]
    fn values(this: lumen_bind::This<Value>) -> crate::cssom::DomCssomCollectionIterator {
        crate::cssom::DomCssomCollectionIterator::new(this.0)
    }

    #[proto(getitem)]
    fn indexed(&self,ctx:&mut Ctx, index: usize) -> OpResult<Value> {
        if self.computed {crate::animations::flush_css_transitions_for(ctx,&self.realm,Some(self.node))?;}
        Ok(self.property_name(index)?.map_or(Value::Undefined, Value::from_string))
    }

    #[getter]
    fn css_text(&self) -> OpResult<String> {
        if self.computed {
            Ok(String::new())
        } else {
            self.raw()
        }
    }
    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_css_text(&self, value: &str) -> OpResult<()> {
        if self.computed {
            return Err(OpError::new("NoModificationAllowedError", "computed style is read-only"));
        }
        self.write_block(css::DeclarationBlock::parse(value).map_err(css_error)?)
    }
    #[method(coerce)]
    fn get_property_value(&self, ctx:&mut Ctx, name: &str) -> OpResult<String> {
        if self.computed {crate::animations::flush_css_transitions_for_property(ctx,&self.realm,Some(self.node),Some(name))?;}
        self.raw_property_value(name)
    }

    #[method(coerce)]
    fn get_property_priority(&self, name: &str) -> OpResult<String> {
        if self.computed {
            return Ok(String::new());
        }
        Ok(if self
            .declaration_value(name)?
            .is_some_and(|(_, important)| important)
        {
            "important"
        } else {
            ""
        }
        .into())
    }
    #[method(coerce, hint(js(ce_reactions)))]
    fn set_property(&self, name: &str, value: &str, priority: Option<&str>) -> OpResult<()> {
        if self.computed {
            return Err(OpError::new(
                "NoModificationAllowedError",
                "computed styles are read-only",
            ));
        }
        // CSSOM ignores invalid values and unsupported properties atomically.
        // An empty value removes the declaration even when priority is invalid.
        let priority = priority.unwrap_or("");
        if !value.is_empty() && !priority.is_empty() && !priority.eq_ignore_ascii_case("important")
        {
            return Ok(());
        }
        let mut block = self.declaration_block()?;
        let Ok(changed) = block.set(name, value, !priority.is_empty()) else {
            return Ok(());
        };
        if changed { self.write_block(block)?; }
        Ok(())
    }
    #[method(coerce, hint(js(ce_reactions)))]
    fn remove_property(&self, ctx:&mut Ctx, name: &str) -> OpResult<String> {
        let old = self.get_property_value(ctx,name)?;
        self.set_property(name, "", None)?;
        Ok(old)
    }
    #[getter]
    fn width(&self, ctx:&mut Ctx) -> OpResult<String> {
        self.get_property_value(ctx,"width")
    }
    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_width(&self, value: &str) -> OpResult<()> {
        self.set_property("width", value, None)
    }
    #[getter]
    fn height(&self, ctx:&mut Ctx) -> OpResult<String> {
        self.get_property_value(ctx,"height")
    }
    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_height(&self, value: &str) -> OpResult<()> {
        self.set_property("height", value, None)
    }
    #[getter]
    fn color(&self, ctx:&mut Ctx) -> OpResult<String> {
        self.get_property_value(ctx,"color")
    }
    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_color(&self, value: &str) -> OpResult<()> {
        self.set_property("color", value, None)
    }
    #[getter]
    fn background_color(&self, ctx:&mut Ctx) -> OpResult<String> {
        self.get_property_value(ctx,"background-color")
    }
    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_background_color(&self, value: &str) -> OpResult<()> {
        self.set_property("background-color", value, None)
    }
    #[getter]
    fn display(&self, ctx:&mut Ctx) -> OpResult<String> {
        self.get_property_value(ctx,"display")
    }
    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_display(&self, value: &str) -> OpResult<()> {
        self.set_property("display", value, None)
    }
    #[getter]
    fn content(&self, ctx:&mut Ctx) -> OpResult<String> {
        self.get_property_value(ctx,"content")
    }
    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_content(&self, value: &str) -> OpResult<()> {
        self.set_property("content", value, None)
    }
    #[getter]
    fn counter_reset(&self, ctx:&mut Ctx) -> OpResult<String> {
        self.get_property_value(ctx,"counter-reset")
    }
    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_counter_reset(&self, value: &str) -> OpResult<()> {
        self.set_property("counter-reset", value, None)
    }
    #[getter]
    fn counter_increment(&self, ctx:&mut Ctx) -> OpResult<String> {
        self.get_property_value(ctx,"counter-increment")
    }
    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_counter_increment(&self, value: &str) -> OpResult<()> {
        self.set_property("counter-increment", value, None)
    }
    #[getter]
    fn counter_set(&self, ctx:&mut Ctx) -> OpResult<String> {
        self.get_property_value(ctx,"counter-set")
    }
    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_counter_set(&self, value: &str) -> OpResult<()> {
        self.set_property("counter-set", value, None)
    }
    #[getter]
    fn quotes(&self, ctx:&mut Ctx) -> OpResult<String> {
        self.get_property_value(ctx,"quotes")
    }
    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_quotes(&self, value: &str) -> OpResult<()> {
        self.set_property("quotes", value, None)
    }
    #[getter]
    fn font_size(&self, ctx:&mut Ctx) -> OpResult<String> {
        self.get_property_value(ctx,"font-size")
    }
    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_font_size(&self, value: &str) -> OpResult<()> {
        self.set_property("font-size", value, None)
    }
    #[getter]
    fn font_size_adjust(&self, ctx:&mut Ctx) -> OpResult<String> {
        self.get_property_value(ctx,"font-size-adjust")
    }
    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_font_size_adjust(&self, value: &str) -> OpResult<()> {
        self.set_property("font-size-adjust", value, None)
    }
    #[getter]
    fn font_family(&self, ctx:&mut Ctx) -> OpResult<String> {
        self.get_property_value(ctx,"font-family")
    }
    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_font_family(&self, value: &str) -> OpResult<()> {
        self.set_property("font-family", value, None)
    }
    #[getter]
    fn font_weight(&self, ctx:&mut Ctx) -> OpResult<String> {
        self.get_property_value(ctx,"font-weight")
    }
    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_font_weight(&self, value: &str) -> OpResult<()> {
        self.set_property("font-weight", value, None)
    }
    #[getter]
    fn font_style(&self, ctx:&mut Ctx) -> OpResult<String> {
        self.get_property_value(ctx,"font-style")
    }
    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_font_style(&self, value: &str) -> OpResult<()> {
        self.set_property("font-style", value, None)
    }
    #[getter]
    fn font(&self, ctx:&mut Ctx) -> OpResult<String> {
        self.get_property_value(ctx,"font")
    }
    #[setter(coerce, hint(js(ce_reactions)))]
    fn set_font(&self, value: &str) -> OpResult<()> {
        self.set_property("font", value, None)
    }
}

pub(crate) struct ComputedStyleElement(DomNodeIdentity);
impl<'a> lumen_bind::FromArg<'a, lumen::embed::JsHost> for ComputedStyleElement {
    fn from_arg(cx: &'a lumen::embed::ArgCx<'_>, value: &'a Value, at: lumen_bind::Slot) -> Result<Self, Value> {
        cx.with_ctx(|ctx| ctx.with_instance::<DomElement, _>(value, |_| ()).map_err(|error| error.to_value(ctx)))?;
        <DomNodeIdentity as lumen_bind::FromArg<'a, lumen::embed::JsHost>>::from_arg(cx, value, at).map(Self)
    }
}

#[lumen_bind::op(name = "getComputedStyle", coerce)]
pub(crate) fn get_computed_style(ctx: &mut Ctx, element: ComputedStyleElement, pseudo: Option<String>) -> OpResult<DomStyle> {
    let (realm, id) = ctx.with_instance::<DomNode, _>(&element.0.value, |node| (node.realm.clone(), node.id))?;
    let pseudo = pseudo.unwrap_or_default();
    let parsed = match pseudo.as_str() {
        "::before" | ":before" => Some(css::PseudoElement::Before),
        "::after" | ":after" => Some(css::PseudoElement::After),
        "::marker" => Some(css::PseudoElement::Marker),
        "::before::marker" => Some(css::PseudoElement::BeforeMarker),
        "::after::marker" => Some(css::PseudoElement::AfterMarker),
        "::placeholder" => Some(css::PseudoElement::Placeholder),
        "::first-line" | ":first-line" => Some(css::PseudoElement::FirstLine),
        "::first-letter" | ":first-letter" => Some(css::PseudoElement::FirstLetter),
        _ => None,
    };
    if pseudo.starts_with("::part(") || pseudo.starts_with("::slotted(") {
        return Err(OpError::type_error("getComputedStyle does not accept part or slotted pseudo-elements"));
    }
    Ok(DomStyle {
        realm,
        node: id,
        computed: true,
        pseudo: parsed,
        invalid_pseudo: !pseudo.is_empty() && parsed.is_none(),
        _owner: element.0.value.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;

    struct NoText;
    impl lumen_html::paint::TextShaper for NoText {
        fn shape(&self, _: &str, _: f32) -> Result<lumen_html::paint::ShapedRun, ()> { Err(()) }
        fn ascent(&self, size: f32) -> f32 { size * 0.8 }
        fn line_height(&self, size: f32) -> f32 { size * 1.2 }
    }

    #[test]
    fn specification_cssom_computed_math_resolves_current_font_before_function_folding() {
        let mut engine=Engine::new();
        install(engine.ctx(),"<div id=parent style='font-size:10px'><div id=target style='animation-range:10% calc(70% + 10% * sign(100em - 1px));width:calc(20px + 10px * sign(1em - 1px))'></div></div>",64).unwrap();
        let result=engine.eval_value(r#"(()=>{
            const parent=document.getElementById('parent'),target=document.getElementById('target');
            const check=(range,width)=>{const style=getComputedStyle(target);if(style.animationRange!==range||style.width!==width)throw Error('computed math '+style.animationRange+' / '+style.width)};
            check('10% 80%','30px');
            parent.style.fontSize='0px';check('10% 60%','10px');
            parent.style.fontSize='20px';check('10% 80%','30px');
            target.style.animationRangeEnd='calc(100% - 100% + 1em)';
            if(getComputedStyle(target).animationRangeEnd!=='calc(0% + 20px)')throw Error('unresolved percentage type lost');
            target.style.animationRangeEnd='min(10%, 20%)';
            if(getComputedStyle(target).animationRangeEnd!=='min(10%, 20%)')throw Error('unresolved percentage comparison folded');
            return true;
        })()"#).unwrap().ok().expect("computed contextual math guard");
        assert!(matches!(result,Value::Bool(true)));
    }

    #[test]
    fn specification_generated_replacement_boxes_and_cssom_use_actual_pseudo_geometry() {
        let mut engine=Engine::new();
        let realm=install(engine.ctx(),r#"<!doctype html><style>
            video,canvas,iframe,object,div.host{display:block;width:100px;height:40px}
            video::before,canvas::before,iframe::before,object::before,div.host::before{content:'';display:block;width:10%;height:10px;background:lime}
            span.child{display:block;width:10%;height:10px;background:red}
        </style><video id=v controls><span id=vc class=child></span></video>
        <canvas id=c><span id=cc class=child></span></canvas>
        <iframe id=f></iframe>
        <object id=o><span id=oc class=child></span></object>
        <div id=d class=host><span id=dc class=child></span></div>"#,256).unwrap();
        realm.set_layout_flusher(Rc::new(|session|session.display_list(400,300,super::super::canvas::canvas_fallback_fonts())
            .map(|_|()).map_err(|error|format!("{error:?}"))));
        let result=engine.eval_value(r#"(() => {
            const expect=(ok,message)=>{if(!ok)throw Error(message)};
            const frameChild=document.createElement('span');frameChild.id='fc';frameChild.className='child';document.getElementById('f').append(frameChild);
            for(const [parent,child] of [['v','vc'],['c','cc'],['f','fc']]){
                const element=document.getElementById(parent),descendant=document.getElementById(child);
                expect(getComputedStyle(element,'::before').width==='10%','absent replaced pseudo uses computed percentage: '+parent);
                expect(getComputedStyle(descendant).width==='10%','replaced content suppresses actual descendant box: '+parent);
            }
            expect(getComputedStyle(document.getElementById('d'),'::before').width==='10px','real pseudo uses its own containing block, not originating parent: '+getComputedStyle(document.getElementById('d'),'::before').width);
            expect(getComputedStyle(document.getElementById('dc')).width==='10px','ordinary child geometry agrees with pseudo');
            expect(getComputedStyle(document.getElementById('o'),'::before').width==='10px','object fallback creates generated content');
            expect(getComputedStyle(document.getElementById('oc')).width==='10px','object fallback creates ordinary child');
            return true;
        })()"#).unwrap().unwrap_or_else(|error|panic!("replacement CSSOM guard: {}",engine.ctx().coerce_string(&error).map(|text|text.to_string()).unwrap_or_else(|_|"unprintable JavaScript exception".into())));
        assert!(matches!(result,Value::Bool(true)));
        realm.with_session(|session|{
            for (parent,child) in [("#v","#vc"),("#c","#cc"),("#f","#fc")] {
                let node=lumen_html::selector::query_selector(session.document(),session.document().root(),parent).unwrap().unwrap();
                let child=lumen_html::selector::query_selector(session.document(),session.document().root(),child).unwrap().unwrap();
                assert!(session.layout_rect(node).is_some(),"real replaced border box exists");
                assert!(session.layout_rect(child).is_none(),"fallback DOM content is not painted as ordinary boxes");
                assert!(session.pseudo_layout_rect(node,css::PseudoElement::Before).is_none(),"suppressed pseudo has no generated fragment");
                assert!(session.pseudo_layout_percentage_basis(node,css::PseudoElement::Before).is_none());
            }
            let object=lumen_html::selector::query_selector(session.document(),session.document().root(),"#o").unwrap().unwrap();
            let child=lumen_html::selector::query_selector(session.document(),session.document().root(),"#oc").unwrap().unwrap();
            assert!(session.layout_rect(child).is_some());
            assert_eq!(session.pseudo_layout_rect(object,css::PseudoElement::Before).unwrap().width,10.0);
            session.set_object_representation(object,lumen_html::object::Representation::Document).unwrap();
            session.display_list(400,300,super::super::canvas::canvas_fallback_fonts()).unwrap();
            assert!(session.layout_rect(object).is_some());
            assert!(session.layout_rect(child).is_none(),"actual navigable representation suppresses object fallback");
            assert!(session.pseudo_layout_rect(object,css::PseudoElement::Before).is_none());
            session.set_object_representation(object,lumen_html::object::Representation::Fallback).unwrap();
            let restored=session.display_list(400,300,super::super::canvas::canvas_fallback_fonts()).unwrap().clone();
            assert!(session.layout_rect(child).is_some());
            assert_eq!(session.pseudo_layout_rect(object,css::PseudoElement::Before).unwrap().width,10.0);
            let mut fresh=lumen_html::session::RenderSession::new(session.document().clone_document(true).unwrap());
            assert_eq!(restored,fresh.display_list(400,300,super::super::canvas::canvas_fallback_fonts()).unwrap().clone());
        });
    }

    // /html/rendering/non-replaced-elements/the-fieldset-and-legend-elements/fieldset-display.html
    // /html/rendering/non-replaced-elements/the-fieldset-and-legend-elements/legend-display.html
    #[test]
    fn specification_cssom_resolved_dimensions_follow_actual_host_used_formatting() {
        let mut engine=Engine::new();
        let realm=install(engine.ctx(),"<!doctype html><style>body{margin:0}fieldset{display:inline;margin:0;padding:0;border:2px solid;min-inline-size:0}legend{display:inline;padding:0;margin:0}</style><fieldset id=f><legend id=l></legend></fieldset><span id=ordinary></span>",96).unwrap();
        realm.set_layout_flusher(Rc::new(|session|session.display_list(300,200,crate::canvas::canvas_fallback_fonts())
            .map(|_|()).map_err(|error|format!("{error:?}"))));
        let answer=engine.eval_value(r#"(() => {
            const check=(ok,message)=>{if(!ok)throw Error(message)};
            const fieldset=document.getElementById('f'),legend=document.getElementById('l');
            const outer=getComputedStyle(fieldset),label=getComputedStyle(legend);
            check(outer.display==='inline' && label.display==='inline','host formatting preserves computed display');
            check(outer.width==='0px' && label.width==='0px','resolved content dimensions apply to real blockified boxes');
            check(fieldset.offsetWidth===4,'principal border geometry');
            check(getComputedStyle(document.getElementById('ordinary')).width==='auto','ordinary non-replaced inline retains computed auto width');
            fieldset.style.width='30px';legend.style.width='10px';
            check(outer.width==='30px' && label.width==='10px','live used dimensions');
            legend.style.display='none';check(label.display==='none' && label.width==='10px','no-box computed dimension fallback');
            legend.style.display='inline';check(label.width==='10px','restored selected legend used dimensions');
            fieldset.style.display='none';check(outer.width==='30px','hidden fieldset computes its authored dimension');
            fieldset.style.display='inline';check(outer.width==='30px','restored retained principal formatting');
            return true;
        })()"#).unwrap().unwrap_or_else(|error|panic!("host used CSSOM: {}",engine.ctx().coerce_string(&error).map(|text|text.to_string()).unwrap_or_else(|_|"unprintable JavaScript exception".into())));
        assert!(matches!(answer,Value::Bool(true)));
    }

    #[test]
    fn specification_computed_typographic_pseudo_uses_shared_cascade_and_webidl_conversion() {
        let mut engine = Engine::new();
        let _realm = install(engine.ctx(), "<!doctype html><style>p{color:blue}p::first-line{color:red;width:900px}p::first-letter{color:green;font-size:30px}</style><p id=target>letters</p>", 64).unwrap();
        let result = engine.eval_value(r#"(() => {
            const p = document.getElementById('target');
            function check(value, message) { if (!value) throw Error(message); }
            const line = getComputedStyle(p, '::first-line');
            check(line.color === 'rgb(255, 0, 0)', 'first-line cascade');
            check(getComputedStyle(p, ':first-letter').fontSize === '30px', 'legacy first-letter');
            check(getComputedStyle(p, null).color === 'rgb(0, 0, 255)', 'nullable pseudo argument');
            check(getComputedStyle(p, '::unknown').length === 0, 'unsupported pseudo empty declarations');
            let converted = 0;
            check(getComputedStyle(p, {toString() { ++converted; return '::first-line'; }}).color === line.color && converted === 1, 'owned argument conversion');
            const moved = document.createElement('div'); document.body.appendChild(moved);
            let adopted = false;
            const destination = document.implementation.createHTMLDocument('owner');
            getComputedStyle(moved, {toString() { destination.adoptNode(moved); adopted = moved.ownerDocument === destination; return '::first-line'; }});
            check(adopted, 'later conversion can migrate earlier element identity');
            let readonly = false; try { line.color = 'purple'; } catch(e) { readonly = e.name === 'NoModificationAllowedError'; }
            check(readonly, 'computed pseudo is readonly');
            p.style.color = 'black';
            check(line.color === 'rgb(255, 0, 0)', 'live pseudo declaration');
            return true;
        })()"#).unwrap().ok().expect("typed computed pseudo guard");
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn computed_margins_use_formatter_values_and_invalidate_with_live_boxes() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<style>body{margin:0}.container{display:flow-root;width:100px;padding:5px;box-sizing:border-box}.box{display:flow-root;width:40px;height:10px;margin:auto}</style><div class=container><div id=center class=box></div></div><div class=container style='direction:rtl'><div id=rtl class=box style='direction:ltr'></div></div><footer style='height:5px'></footer>", 128).unwrap();
        realm.set_layout_flusher(Rc::new(|session| {
            session.display_list(300, 200, &NoText).map(|_| ()).map_err(|error| format!("{error:?}"))
        }));
        let result = engine.eval_value(r#"(() => {
            function eq(a,b) { if(a!==b) throw Error(a+' !== '+b); }
            const center=document.getElementById('center'), style=getComputedStyle(center);
            eq(style.marginLeft,'25px'); eq(style.marginRight,'25px');
            eq(style.marginInlineStart,'25px');
            const rtl=getComputedStyle(document.getElementById('rtl'));
            eq(rtl.marginLeft,'25px'); eq(rtl.marginRight,'25px');
            document.querySelector('footer').style.height='7px';
            eq(style.marginLeft,'25px');
            center.style.width='20px'; eq(style.marginLeft,'35px'); eq(style.marginRight,'35px');
            center.style.position='relative'; center.style.left='10px'; eq(style.marginLeft,'35px');
            center.style.display='none'; eq(style.marginLeft,'auto'); eq(style.marginRight,'auto');
            center.style.display='flow-root'; eq(style.marginLeft,'35px');
            center.remove(); eq(style.marginLeft,''); eq(style.length,0);
            const detached=document.createElement('div'); detached.style.margin='auto';
            eq(getComputedStyle(detached).marginLeft,'');
            return true;
        })()"#).unwrap().unwrap_or_else(|error| panic!("used margin semantics threw: {}", engine.ctx().coerce_string(&error).map(|value| value.to_string()).unwrap_or_default()));
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn font_affecting_math_resolves_parent_query_metrics_before_size_overrides() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(),"<div id=container style='container-type:inline-size;width:100px'><div style='font-size:20cqw'><span id=target style='font-size:0;font-width:calc(100% + sign(1em - 18px)*5%);font-style:oblique calc(30deg + sign(1em - 18px)*5deg);font-feature-settings:&quot;liga&quot; calc(10 + sign(1em - 18px)*5)'></span></div></div>",128).unwrap();
        let calls = Rc::new(std::cell::Cell::new(0));
        let observed = calls.clone();
        realm.set_layout_flusher(Rc::new(move |session| {
            observed.set(observed.get()+1);
            session.display_list(800,600,&NoText).map(|_|()).map_err(|error|format!("{error:?}"))
        }));
        let result = engine.eval_value("globalThis.parentFontStyle=getComputedStyle(document.getElementById('target'));parentFontStyle.color==='rgb(0, 0, 0)'").unwrap().unwrap_or_else(|_|panic!("parent metric setup failed"));
        assert!(matches!(result,Value::Bool(true)));
        assert_eq!(calls.get(),0);
        let result = engine.eval_value(r#"(() => {
            const check=(width,angle,feature) => { if(parentFontStyle.fontSize!=='0px' || parentFontStyle.fontWidth!==width || parentFontStyle.fontStyle!==angle || parentFontStyle.fontFeatureSettings!==feature) throw Error(parentFontStyle.cssText+'|'+parentFontStyle.fontWidth+'|'+parentFontStyle.fontStyle+'|'+parentFontStyle.fontFeatureSettings); };
            check('105%','oblique 35deg','"liga" 15');
            document.getElementById('container').style.width='10px';
            check('95%','oblique 25deg','"liga" 5');
            return true;
        })()"#).unwrap().unwrap_or_else(|error|panic!("parent font metric resolution failed: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));
        assert!(matches!(result,Value::Bool(true)));
        assert_eq!(calls.get(),2);
    }

    #[test]
    fn computed_font_width_alias_flushes_only_required_query_context() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(),"<div id=container style='container-type:inline-size;width:100px'><div id=target><span id=child></span></div></div>",128).unwrap();
        let calls = Rc::new(std::cell::Cell::new(0));
        let observed = calls.clone();
        realm.set_layout_flusher(Rc::new(move |session| {
            observed.set(observed.get()+1);
            session.display_list(800,600,&NoText).map(|_|()).map_err(|error|format!("{error:?}"))
        }));
        let result = engine.eval_value(r#"(() => {
            const target=document.getElementById('target');
            target.style.fontStretch='calc(100% + sign(2cqw - 10px)*5%)';
            if(target.style.fontWidth!==target.style.fontStretch || target.style.length!==1 || target.style.item(0)!=='font-width') throw Error(target.style.cssText);
            globalThis.widthStyle=getComputedStyle(target);
            if(widthStyle.color!=='rgb(0, 0, 0)') throw Error(widthStyle.color);
            let canonical=false;for(let i=0;i<widthStyle.length;i++) {const name=widthStyle.item(i);if(name==='font-stretch')throw Error('legacy alias enumerated');if(name==='font-width')canonical=true;}
            if(!canonical)throw Error('missing canonical width');
            return true;
        })()"#).unwrap().unwrap_or_else(|_|panic!("width assignment failed"));
        assert!(matches!(result,Value::Bool(true)));
        assert_eq!(calls.get(),0);
        let result = engine.eval_value(r#"(() => {
            if(widthStyle.fontWidth!=='95%' || widthStyle.fontStretch!=='95%') throw Error(widthStyle.fontWidth+'|'+widthStyle.fontStretch);
            const child=getComputedStyle(document.getElementById('child'));
            if(child.fontWidth!=='95%') throw Error(child.fontWidth);
            document.getElementById('container').style.width='1000px';
            if(widthStyle.fontStretch!=='105%' || child.fontWidth!=='105%') throw Error(widthStyle.fontStretch+'|'+child.fontWidth);
            document.getElementById('target').style.font='16px serif';
            if(widthStyle.fontWidth!=='100%') throw Error('font reset '+widthStyle.fontWidth);
            return true;
        })()"#).unwrap().unwrap_or_else(|error|panic!("width query resolution failed: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));
        assert!(matches!(result,Value::Bool(true)));
        assert_eq!(calls.get(),2);
    }

    #[test]
    fn specification_unicode_bidi_cssom_live_scopes_preserve_selection_sources() {
        let mut engine=Engine::new();let realm=install(engine.ctx(),"<div id=target>a<span id=inner dir=rtl>אב</span>z</div><bdo id=override dir=rtl>abc</bdo>",64).unwrap();
        let font=lumen_html_text::FontFace::new(std::sync::Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        realm.set_layout_flusher(Rc::new(move|session|session.display_list(300,200,&font).map(|_|()).map_err(|error|format!("{error:?}"))));
        let result=engine.eval_value(r#"(()=>{
            const eq=(a,b)=>{if(a!==b)throw Error(a+' !== '+b)},target=document.getElementById('target'),inner=document.getElementById('inner'),bdo=document.getElementById('override');
            eq(getComputedStyle(inner).unicodeBidi,'isolate');eq(getComputedStyle(bdo).unicodeBidi,'isolate-override');
            target.style.unicodeBidi='plaintext';eq(target.style.unicodeBidi,'plaintext');eq(getComputedStyle(target).unicodeBidi,'plaintext');
            eq(target.innerText,'aאבz');eq(target.textContent,'aאבz');
            const selection=getSelection();selection.setBaseAndExtent(inner.firstChild,0,inner.firstChild,2);eq(selection.toString(),'אב');
            inner.style.unicodeBidi='normal';eq(getComputedStyle(inner).unicodeBidi,'normal');eq(target.innerText,'aאבz');
            inner.style.unicodeBidi='isolate';inner.style.all='initial';eq(getComputedStyle(inner).unicodeBidi,'isolate');
            inner.style.unicodeBidi='embed isolate';eq(inner.style.unicodeBidi,'isolate');return true;
        })()"#).unwrap().unwrap_or_else(|error|panic!("bidi source projection: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));assert!(matches!(result,Value::Bool(true)));
    }

    #[test]
    fn specification_modern_text_transforms_cssom_rendering_and_live_selection_share_sources() {
        let mut engine=Engine::new();let realm=install(engine.ctx(),"<div id=target>a ｧ</div><p id=math><span id=single>a</span><span id=multiple>ab</span></p>",64).unwrap();
        let font=lumen_html_text::FontFace::new(std::sync::Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        realm.set_layout_flusher(Rc::new(move |session|session.display_list(300,200,&font).map(|_|()).map_err(|error|format!("{error:?}"))));
        let result=engine.eval_value(r#"(() => {
            const eq=(a,b)=>{if(a!==b)throw Error(a+' !== '+b)},target=document.getElementById('target'),math=document.getElementById('math');
            target.style.textTransform='full-size-kana uppercase full-width';
            eq(target.style.textTransform,'uppercase full-width full-size-kana');eq(getComputedStyle(target).textTransform,target.style.textTransform);
            eq(target.innerText,'Ａ　ア');eq(target.textContent,'a ｧ');
            math.style.textTransform='math-auto';eq(math.innerText,'𝑎ab');
            const selection=getSelection(),one=document.getElementById('single').firstChild,many=document.getElementById('multiple').firstChild;
            selection.setBaseAndExtent(one,0,one,1);eq(selection.toString(),'𝑎');
            selection.setBaseAndExtent(many,0,many,1);eq(selection.toString(),'a');
            many.data='h';eq(math.innerText,'𝑎ℎ');
            target.style.textTransform='math-auto full-width';eq(target.style.textTransform,'uppercase full-width full-size-kana');
            target.style.all='initial';eq(target.style.textTransform,'uppercase full-width full-size-kana');eq(getComputedStyle(target).textTransform,'none');
            return true;
        })()"#).unwrap().unwrap_or_else(|error|panic!("modern transform projection: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));
        assert!(matches!(result,Value::Bool(true)));
    }

    #[test]
    fn text_transform_cssom_and_rendered_text_follow_live_language_and_inline_context() {
        let mut engine=Engine::new();
        let realm=install(engine.ctx(),"<main lang='tr' style='text-transform:uppercase'><span id=target>i</span></main><p id=word style='text-transform:capitalize'>a<span id=middle>b</span>c</p>",64).unwrap();
        let font=lumen_html_text::FontFace::new(std::sync::Arc::from(lumen_html_text::TEST_FONT_BYTES)).unwrap();
        realm.set_layout_flusher(Rc::new(move |session|session.display_list(300,200,&font).map(|_|()).map_err(|error|format!("{error:?}"))));
        let result=engine.eval_value(r#"(() => {
            const eq=(a,b)=>{if(a!==b)throw Error(a+' !== '+b)},target=document.getElementById('target'),main=target.parentElement,view=getComputedStyle(target);
            eq(view.textTransform,'uppercase');eq(target.innerText,'İ');eq(target.textContent,'i');
            main.lang='';eq(main.getAttribute('lang'),'');eq(target.innerText,'I');
            main.lang='nl';eq(main.getAttribute('lang'),'nl');main.style.textTransform='capitalize';target.firstChild.data='ijsland';eq(target.innerText,'IJsland');
            target.setAttributeNS('http://www.w3.org/XML/1998/namespace','xml:lang','tr');main.style.textTransform='uppercase';target.firstChild.data='i';eq(target.innerText,'İ');
            target.removeAttributeNS('http://www.w3.org/XML/1998/namespace','lang');eq(target.innerText,'I');
            const middle=document.getElementById('middle');eq(middle.innerText,'b');middle.previousSibling.data=' ';eq(middle.innerText,'B');
            target.style.textTransform='lowercase';eq(view.textTransform,'lowercase');eq(target.innerText,'i');eq(target.textContent,'i');
            target.style.textTransform='none uppercase';eq(target.style.textTransform,'lowercase');
            target.style.all='initial';eq(target.style.getPropertyValue('all'),'initial');eq(target.style.all,'initial');eq(view.textTransform,'none');
            return true;
        })()"#).unwrap().unwrap_or_else(|error|panic!("text transform semantics threw: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));
        assert!(matches!(result,Value::Bool(true)));
    }

    #[test]
    fn computed_feature_settings_flush_query_math_and_preserve_font_variant_independence() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<div id=container style='container-type:inline-size;width:100px'><div id=target style='font-feature-settings:normal'><span id=child></span></div></div>",128).unwrap();
        let calls = Rc::new(std::cell::Cell::new(0));
        let observed = calls.clone();
        realm.set_layout_flusher(Rc::new(move |session| {
            observed.set(observed.get()+1);
            session.display_list(800,600,&NoText).map(|_|()).map_err(|error|format!("{error:?}"))
        }));
        let result = engine.eval_value(r#"(() => {
            const target=document.getElementById('target');
            target.style.fontFeatureSettings='"liga" calc(10 + sign(2cqw - 10px)*5), "dlig" calc(20 + sign(2cqw - 10px)*5)';
            globalThis.featureStyle=getComputedStyle(target);
            if(featureStyle.color!=='rgb(0, 0, 0)') throw Error(featureStyle.color);
            return true;
        })()"#).unwrap().unwrap_or_else(|_|panic!("feature assignment failed"));
        assert!(matches!(result,Value::Bool(true)));
        assert_eq!(calls.get(),0);
        let result = engine.eval_value(r#"(() => {
            const child=getComputedStyle(document.getElementById('child'));
            if(child.fontFeatureSettings!=='"dlig" 15, "liga" 5') throw Error(child.fontFeatureSettings);
            document.getElementById('container').style.width='1000px';
            if(featureStyle.fontFeatureSettings!=='"dlig" 25, "liga" 15') throw Error(featureStyle.fontFeatureSettings);
            const target=document.getElementById('target');
            target.style.fontVariant='none';
            if(featureStyle.fontFeatureSettings!=='"dlig" 25, "liga" 15') throw Error('font-variant reset settings');
            target.style.font='16px serif';
            if(featureStyle.fontFeatureSettings!=='normal') throw Error('font failed to reset settings');
            return true;
        })()"#).unwrap().unwrap_or_else(|error|panic!("feature query resolution failed: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));
        assert!(matches!(result,Value::Bool(true)));
        assert_eq!(calls.get(),3);
    }

    #[test]
    fn specification_computed_font_backend_reuses_one_cache_after_lazy_initialization() {
        let mut engine=Engine::new();
        let realm=install(engine.ctx(),"<style>#target{font-size:10px;padding-left:2ch;color:red}#target::before{content:'';font-size:20px;padding-left:2ch}</style><div id=target></div>",128).unwrap();
        let check=|engine:&mut Engine,source:&str| {
            let result=engine.eval_value(source).unwrap().unwrap_or_else(|error|
                panic!("backend cache guard threw: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));
            assert!(matches!(result,Value::Bool(true)));
        };
        check(&mut engine,"globalThis.cachedStyle=getComputedStyle(document.getElementById('target'));cachedStyle.color==='rgb(255, 0, 0)'");
        assert!(!realm.font_loading.canvas_font_source_initialized.get(),"first color read must not initialize or load a font source");
        check(&mut engine,"globalThis.realMetric=parseFloat(cachedStyle.paddingLeft);realMetric>0");
        assert!(realm.font_loading.canvas_font_source_initialized.get());
        let before=realm.session.borrow().read_style_cache_stats();
        check(&mut engine,r#"(() => {
            for(let i=0;i<8;i++) {
                if(cachedStyle.color!=='rgb(255, 0, 0)' || parseFloat(cachedStyle.paddingLeft)!==realMetric) return false;
            }
            return true;
        })()"#);
        let after=realm.session.borrow().read_style_cache_stats();
        assert_eq!(after.computed_styles,before.computed_styles,"color/backend alternation must retain canonical computed styles");
        assert!(after.cache_hits>before.cache_hits,"alternating reads must hit the same style cache");
        check(&mut engine,r#"(() => {
            const pseudo=getComputedStyle(document.getElementById('target'),'::before');
            if(Math.abs(parseFloat(pseudo.paddingLeft)-realMetric*2)>0.001) return false;
            document.getElementById('target').style.fontSize='20px';
            return cachedStyle.color==='rgb(255, 0, 0)' && Math.abs(parseFloat(cachedStyle.paddingLeft)-realMetric*2)<0.001;
        })()"#);
    }

    #[test]
    fn computed_current_font_pair_lengths_follow_query_size_and_backend_cache_identity() {
        let mut engine=Engine::new();
        let realm=install(engine.ctx(),"<style>body{margin:0;font-size:0}#container{container-type:inline-size;width:200px}#target{font-size:10cqw;border-spacing:2em 3em;padding:2ex 3ch}</style><div id=container><table id=target></table></div>",128).unwrap();
        realm.set_layout_flusher(Rc::new(|session|session.display_list(800,600,super::super::canvas::canvas_fallback_fonts()).map(|_|()).map_err(|error|format!("{error:?}"))));
        let result=engine.eval_value(r#"(() => {
            const target=document.getElementById('target'), style=getComputedStyle(target);
            if(style.color!=='rgb(0, 0, 0)') throw Error('ordinary source read');
            if(style.borderSpacing!=='40px 60px') throw Error('pair '+style.borderSpacing);
            const before=parseFloat(style.paddingLeft);
            if(!(before>0)) throw Error('actual positive ch');
            document.getElementById('container').style.width='100px';
            if(style.borderSpacing!=='20px 30px') throw Error('mutated pair '+style.borderSpacing);
            if(Math.abs(parseFloat(style.paddingLeft)*2-before)>0.001) throw Error('backend metrics follow size');
            return true;
        })()"#).unwrap().unwrap_or_else(|error|panic!("current font pair guard failed: {}",engine.ctx().coerce_string(&error).map(|value|value.to_string()).unwrap_or_default()));
        assert!(matches!(result,Value::Bool(true)));
    }

    #[test]
    fn computed_font_size_and_line_height_flush_only_their_query_dependencies() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(),
            "<style>body{margin:0}#container{container-type:inline-size;width:200px}#target{font-size:10cqw;line-height:1.5;width:50cqw;color:red}#height{line-height:15cqw;letter-spacing:2cqw}</style><div id=container><div id=target><span id=child></span></div><div id=height></div></div>",
            128).unwrap();
        let calls = Rc::new(std::cell::Cell::new(0));
        let observed = calls.clone();
        realm.set_layout_flusher(Rc::new(move |session| {
            observed.set(observed.get() + 1);
            session.display_list(800, 600, &NoText).map(|_| ()).map_err(|error| format!("{error:?}"))
        }));
        let result = engine.eval_value(r#"(() => {
            globalThis.targetStyle=getComputedStyle(document.getElementById('target'));
            if(targetStyle.color!=='rgb(255, 0, 0)') throw Error(targetStyle.color);
            return true;
        })()"#).unwrap().unwrap_or_else(|_| panic!("color dependency guard threw"));
        assert!(matches!(result, Value::Bool(true)));
        assert_eq!(calls.get(), 0, "color reads must not flush unrelated query lengths");
        let result = engine.eval_value(r#"(() => {
            const child=getComputedStyle(document.getElementById('child'));
            if(child.fontSize!=='20px') throw Error('inherited size '+child.fontSize);
            if(targetStyle.fontSize!=='20px') throw Error('size '+targetStyle.fontSize);
            if(child.lineHeight!=='30px') throw Error('line height '+child.lineHeight);
            const height=getComputedStyle(document.getElementById('height'));
            if(height.lineHeight!=='30px') throw Error('query line height '+height.lineHeight);
            if(height.letterSpacing!=='4px') throw Error('query letter spacing '+height.letterSpacing);
            document.getElementById('container').style.width='100px';
            if(targetStyle.color!=='rgb(255, 0, 0)') throw Error(targetStyle.color);
            return true;
        })()"#).unwrap().unwrap_or_else(|error| panic!("query font values failed: {}", engine.ctx().coerce_string(&error).map(|value| value.to_string()).unwrap_or_default()));
        assert!(matches!(result, Value::Bool(true)));
        assert_eq!(calls.get(), 1);
        let result = engine.eval_value(r#"(() => {
            if(targetStyle.fontSize!=='10px') throw Error(targetStyle.fontSize);
            const height=getComputedStyle(document.getElementById('height'));
            if(height.lineHeight!=='15px') throw Error(height.lineHeight);
            if(height.letterSpacing!=='2px') throw Error(height.letterSpacing);
            return true;
        })()"#).unwrap().unwrap_or_else(|error| panic!("mutated query font values failed: {}", engine.ctx().coerce_string(&error).map(|value| value.to_string()).unwrap_or_default()));
        assert!(matches!(result, Value::Bool(true)));
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn computed_font_style_flushes_before_resolving_container_units() {
        let mut engine = Engine::new();
        let realm = install(
            engine.ctx(),
            "<style>body{margin:0}#container{container-type:inline-size;width:10px}#target{font-style:oblique calc(30deg + (sign(20cqw - 10px) * 5deg))}</style><div id=container><div id=target></div></div>",
            128,
        )
        .unwrap();
        realm.set_layout_flusher(Rc::new(|session| {
            session
                .display_list(300, 200, &NoText)
                .map(|_| ())
                .map_err(|error| format!("{error:?}"))
        }));
        let result = engine
            .eval_value(r#"(() => {
                const target=document.getElementById('target');
                const style=getComputedStyle(target);
                if(style.fontStyle!=='oblique 25deg') throw Error(style.fontStyle);
                document.getElementById('container').style.width='60px';
                if(style.fontStyle!=='oblique 35deg') throw Error(style.fontStyle);
                return true;
            })()"#)
            .unwrap()
            .unwrap_or_else(|error| {
                panic!(
                    "container-relative computed font style failed: {}",
                    engine
                        .ctx()
                        .coerce_string(&error)
                        .map(|value| value.to_string())
                        .unwrap_or_default()
                )
            });
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn computed_declarations_follow_flat_tree_and_rendered_browsing_contexts() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<div id=host><b id=light style='color:blue'></b></div><iframe id=frame></iframe>", 256).unwrap();
        let result = engine.eval_value(r#"(() => {
            const eq=(a,b,label)=>{if(a!==b) throw Error(label+': '+a+' !== '+b);};
            let rejected=false; try {getComputedStyle(document.createTextNode('x'));} catch(error) {rejected=error instanceof TypeError;}
            eq(rejected,true,'Element argument conversion');
            const host=document.getElementById('host'), light=document.getElementById('light'), style=getComputedStyle(light);
            const populated=style.length;
            if(!populated) throw Error('connected declarations');
            const shadow=host.attachShadow({mode:'closed'});
            eq(style.length,0,'unslotted length'); eq(style.item(0),'','unslotted item'); eq(style.color,'','unslotted value');
            const slot=document.createElement('slot'); shadow.append(slot);
            eq(style.length,populated,'assigned declarations');
            light.slot='named'; eq(style.length,0,'unmatched slot');
            slot.name='named'; eq(style.length,populated,'renamed slot');
            const fallback=document.createElement('i'); fallback.style.width='17px'; slot.append(fallback);
            const fallbackStyle=getComputedStyle(fallback); eq(fallbackStyle.length,0,'inactive fallback');
            light.slot='other'; eq(fallbackStyle.width,'17px','active fallback');
            host.remove(); eq(fallbackStyle.length,0,'detached shadow');
            document.body.append(host); eq(fallbackStyle.width,'17px','reattached shadow');
            const parsed=new DOMParser().parseFromString('<p>inert document</p>','text/html');
            eq(getComputedStyle(parsed.body).length,0,'document without browsing context');
            const frame=document.getElementById('frame'), body=frame.contentDocument.body;
            body.style.marginLeft='11px'; const frameStyle=getComputedStyle(body);
            eq(frameStyle.marginLeft,'11px','rendered container');
            frame.style.display='none'; eq(frameStyle.length,0,'hidden container'); eq(frameStyle.marginLeft,'','hidden container value');
            frame.style.display='block'; eq(frameStyle.marginLeft,'11px','restored container');
            return true;
        })()"#).unwrap().unwrap_or_else(|error| panic!("computed declaration eligibility threw: {}", engine.ctx().coerce_string(&error).map(|value| value.to_string()).unwrap_or_default()));
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn grid_lanes_computed_style_tracks_native_layout_and_cascade_resets() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<div style='display:grid-lanes;grid-template-columns:40px 60px;grid-lanes-direction:column fill-reverse track-reverse;grid-lanes-pack:dense;flow-tolerance:25%'></div>", 128).unwrap();
        let result = engine.eval_value(r#"(() => {
            const element=document.querySelector('div'), computed=getComputedStyle(element);
            function expect(actual, expected) { if(actual!==expected) throw new Error(actual+' !== '+expected); }
            expect(computed.display,'grid-lanes');
            expect(computed.gridLanesDirection,'column fill-reverse track-reverse');
            expect(computed.gridLanesPack,'dense');
            expect(computed.flowTolerance,'25%');
            element.style.cssText='display:grid;grid-lanes-direction:initial;grid-lanes-pack:initial;flow-tolerance:initial';
            expect(computed.display,'grid');
            expect(computed.gridLanesDirection,'normal');
            expect(computed.gridLanesPack,'normal');
            expect(computed.flowTolerance,'normal');
            return true;
        })()"#).unwrap().unwrap_or_else(|_| panic!("Grid Lanes computed CSS contract"));
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn inline_grid_computed_display_aliases_blockification_and_reset() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<div id='plain' style='display:inline grid-lanes'></div><section style='display:flex'><div id='item' style='display:inline-grid'></div></section><div id='float' style='display:inline grid-lanes;float:left'></div>",128).unwrap();
        let result = engine.eval_value(r#"(() => {
            function expect(actual, expected) { if(actual!==expected) throw new Error(actual+' !== '+expected); }
            const element=document.querySelector('#plain'), computed=getComputedStyle(element);
            expect(computed.display,'inline grid-lanes');
            element.style.display='inline-grid'; expect(computed.display,'inline-grid');
            element.style.display='grid inline'; expect(computed.display,'inline-grid');
            element.style.display='block grid-lanes'; expect(computed.display,'grid-lanes');
            element.style.display='initial'; expect(computed.display,'inline');
            expect(getComputedStyle(document.querySelector('#item')).display,'grid');
            expect(getComputedStyle(document.querySelector('#float')).display,'grid-lanes');
            return true;
        })()"#).unwrap().unwrap_or_else(|_|panic!("inline Grid computed display contract"));
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn computed_font_caps_follow_inheritance_shorthand_and_synthesis_policy() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main style='font-variant-caps:small-caps;font-synthesis-small-caps:none'><span>ßa</span></main>", 128).unwrap();
        let result = engine.eval_value(r#"(() => {
            const parent=document.querySelector('main'), element=parent.firstElementChild;
            const computed=getComputedStyle(element);
            function expect(actual, expected) { if(actual!==expected) throw new Error(actual+' !== '+expected); }
            expect(computed.fontVariantCaps,'small-caps');
            expect(computed.fontSynthesisSmallCaps,'none');
            element.style.font='italic 20px serif';
            expect(computed.fontVariantCaps,'normal');
            expect(computed.fontSynthesisSmallCaps,'none');
            element.style.font='small-caps bold 18px sans-serif';
            expect(computed.fontVariantCaps,'small-caps');
            expect(computed.fontWeight,'700');
            expect(computed.fontSynthesisSmallCaps,'none');
            element.style.fontVariantCaps='all-small-caps';
            element.style.fontSynthesisSmallCaps='auto';
            expect(computed.fontVariantCaps,'all-small-caps');
            expect(computed.fontSynthesisSmallCaps,'auto');
            element.style.fontVariantCaps='small-caps invalid';
            element.style.fontSynthesisSmallCaps='none invalid';
            expect(computed.fontVariantCaps,'all-small-caps');
            expect(computed.fontSynthesisSmallCaps,'auto');
            parent.style.fontVariantCaps='petite-caps';
            element.style.fontVariantCaps='unset';
            element.style.fontSynthesisSmallCaps='inherit';
            expect(computed.fontVariantCaps,'petite-caps');
            expect(computed.fontSynthesisSmallCaps,'none');
            element.style.fontVariantCaps='initial';
            element.style.fontSynthesisSmallCaps='initial';
            expect(computed.fontVariantCaps,'normal');
            expect(computed.fontSynthesisSmallCaps,'auto');
            return true;
        })()"#).unwrap().unwrap_or_else(|_| panic!("computed font caps cascade and synthesis policy"));
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn computed_alignment_keywords_preserve_normal_flex_and_live_shorthand_identity() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main><div></div></main>", 128).unwrap();
        let result = engine.eval_value(r#"(() => {
            const parent=document.querySelector('main'), element=parent.firstElementChild;
            const computed=getComputedStyle(element);
            function expect(actual, expected) { if(actual!==expected) throw new Error(actual+' !== '+expected); }
            expect(computed.alignItems,'normal'); expect(computed.justifyItems,'normal');
            expect(computed.alignContent,'normal'); expect(computed.justifyContent,'normal');
            expect(computed.placeItems,'normal'); expect(computed.placeSelf,'auto');
            element.style.cssText='place-items:flex-start normal;place-self:normal flex-end;align-content:safe flex-end;justify-content:unsafe flex-start';
            expect(computed.alignItems,'flex-start'); expect(computed.justifyItems,'normal');
            expect(computed.alignSelf,'normal'); expect(computed.justifySelf,'flex-end');
            expect(computed.placeItems,'flex-start normal'); expect(computed.placeSelf,'normal flex-end');
            expect(computed.alignContent,'safe flex-end'); expect(computed.justifyContent,'unsafe flex-start');
            element.style.alignItems='start'; element.style.justifyItems='stretch';
            expect(computed.placeItems,'start stretch');
            element.style.placeItems='flex-end'; expect(computed.placeItems,'flex-end');
            element.style.justifyItems='end'; expect(computed.placeItems,'flex-end end');
            parent.style.cssText='place-items:normal flex-start;place-self:flex-end normal;align-content:unsafe flex-start;justify-content:safe flex-end';
            element.style.cssText='place-items:inherit;place-self:inherit;align-content:inherit;justify-content:inherit';
            expect(computed.placeItems,'normal flex-start'); expect(computed.placeSelf,'flex-end normal');
            expect(computed.alignContent,'unsafe flex-start'); expect(computed.justifyContent,'safe flex-end');
            element.style.cssText='place-items:initial;place-self:unset;align-content:initial;justify-content:unset';
            expect(computed.placeItems,'normal'); expect(computed.placeSelf,'auto');
            expect(computed.alignContent,'normal'); expect(computed.justifyContent,'normal');
            return true;
        })()"#).unwrap().unwrap_or_else(|_|panic!("computed alignment keyword identity"));
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn grid_place_alignment_and_columns_computed_shorthands_follow_native_cascade() {
        let mut engine = Engine::new();
        install(
            engine.ctx(),
            "<div style='place-items:start center;place-self:end stretch;columns:3;align-content:safe end;justify-content:unsafe center'></div>",
            128,
        )
        .unwrap();
        let result = engine.eval_value(r#"(() => {
            const element = document.querySelector('div'), computed=getComputedStyle(element);
            function expect(actual, expected) { if(actual!==expected) throw new Error(actual+' !== '+expected); }
            expect(computed.getPropertyValue('align-items'),'start');
            expect(computed.getPropertyValue('justify-items'),'center');
            expect(computed.getPropertyValue('place-items'),'start center');
            expect(computed.getPropertyValue('place-self'),'end stretch');
            expect(computed.getPropertyValue('column-count'),'3');
            expect(computed.getPropertyValue('align-content'),'safe end');
            expect(computed.getPropertyValue('justify-content'),'unsafe center');
            element.style.cssText='place-items:center;place-self:initial;columns:auto';
            expect(computed.getPropertyValue('place-items'),'center');
            expect(computed.getPropertyValue('place-self'),'auto');
            expect(computed.getPropertyValue('column-count'),'auto');
            element.style.placeItems='end center';
            element.style.placeSelf='center start';
            element.style.columns='3';
            element.style.alignContent='safe center';
            element.style.justifyContent='unsafe end';
            expect(element.style.placeItems,'end center');
            expect(element.style.placeSelf,'center start');
            expect(element.style.columns,'3');
            expect(element.style.alignContent,'safe center');
            expect(element.style.justifyContent,'unsafe end');
            expect(computed.placeItems,'end center');
            expect(computed.placeSelf,'center start');
            expect(computed.columnCount,'3');
            expect(computed.alignContent,'safe center');
            expect(computed.justifyContent,'unsafe end');
            for(const [property,value] of [['place-items','end center'],['place-self','center start'],['columns','3'],['align-content','safe center'],['justify-content','unsafe end']]) {
                expect(CSS.supports(property,value),true);
            }
            expect(CSS.supports('place-items','start auto'),false);
            expect(CSS.supports('justify-content','safe space-between'),false);
            element.style.placeItems='start auto';
            expect(element.style.placeItems,'end center');
            return true;
        })()"#).unwrap().unwrap_or_else(|_|panic!("alignment shorthand computed cascade"));
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn computed_margin_sides_follow_live_native_style_and_animation_values() {
        let mut engine = Engine::new();
        install(
            engine.ctx(),
            "<div style='margin:1px 2px 3px 4px'></div>",
            128,
        )
        .unwrap();
        let result = engine.eval_value(r#"(() => {
            const element=document.querySelector('div'), style=getComputedStyle(element);
            function expect(actual, expected, label) {
                if (actual!==expected) throw new Error(label+': '+actual+' !== '+expected);
            }
            expect(style.marginTop,'1px','top');
            expect(style.getPropertyValue('margin-right'),'2px','right');
            expect(style.marginBottom,'3px','bottom');
            expect(style.marginLeft,'4px','left');
            element.style.marginLeft='-7.5px';
            expect(style.marginLeft,'-7.5px','live left');
            const animation=element.animate({marginLeft:['0px','300px']},{duration:1000,fill:'both'});
            animation.currentTime=500;
            expect(style.marginLeft,'150px','native animation interpolation');
            animation.cancel();
            expect(style.marginLeft,'-7.5px','underlying left restored');
            element.style.cssText='display:none;font-size:20px;margin:1em auto';
            expect(style.marginTop,'20px','shared font-relative computed length');
            expect(style.marginRight,'auto','unboxed computed automatic margin');
            expect(style.marginLeft,'auto','automatic margin preserved');
            return true;
        })()"#).unwrap().unwrap_or_else(|error| {
            let message=match engine.ctx().member_get(&error,"message") {
                Ok(Value::Str(message)) => message.to_string(),
                _ => "non-string script exception".into(),
            };
            panic!("computed margin contract: {message}");
        });
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn counter_cssom_canonicalizes_defaults_and_keeps_computed_values_live() {
        let mut runtime = lumen_runtime::Runtime::new();
        let engine = runtime.engine();
        install(engine.ctx(), "<main><div></div></main>", 128).unwrap();
        let result = engine.eval_value(r#"(() => {
            const element=document.querySelector('main'), style=element.style;
            function expect(actual, expected, label) {
                if (actual!==expected) throw new Error(label+': '+actual+' !== '+expected);
            }
            const computed=getComputedStyle(element);
            expect(computed.counterReset,'none','initial reset');
            expect(computed.counterIncrement,'none','initial increment');
            expect(computed.counterSet,'none','initial set');
            style.counterReset='chapter section -2';
            style.counterIncrement='chapter section 2';
            style.counterSet='chapter 9 section';
            expect(style.counterReset,'chapter 0 section -2','reset defaults');
            expect(style.counterIncrement,'chapter 1 section 2','increment defaults');
            expect(style.counterSet,'chapter 9 section 0','set defaults');
            expect(computed.counterReset,style.counterReset,'computed reset');
            expect(computed.counterIncrement,style.counterIncrement,'computed increment');
            expect(computed.counterSet,style.counterSet,'computed set');
            expect(getComputedStyle(element.firstElementChild).counterReset,'none','counter reset not inherited');
            style.setProperty('counter-increment','chapter 3','important');
            expect(style.getPropertyPriority('counter-increment'),'important','priority survives canonicalization');
            expect(computed.counterIncrement,'chapter 3','live computed increment');
            style.counterIncrement='none 2';
            expect(style.counterIncrement,'chapter 3','invalid value keeps prior declaration');
            expect(style.removeProperty('counter-reset'),'chapter 0 section -2','removed canonical value');
            expect(computed.counterReset,'none','removal updates computed reset');
            return true;
        })()"#).unwrap().unwrap_or_else(|error| {
            let message=match engine.ctx().member_get(&error,"message") {
                Ok(Value::Str(message)) => message.to_string(),
                _ => "non-string script exception".into(),
            };
            panic!("counter CSSOM contract: {message}");
        });
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn generated_content_cssom_reflects_canonical_values_and_live_computed_styles() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main style='display:none'></main>", 128).unwrap();
        let result = engine
            .eval_value(
                r#"(() => {
            const el=document.querySelector('main'), style=el.style;
            function expect(value, wanted, label) {
                if (value!==wanted) throw new Error(label+': '+value+' !== '+wanted);
            }
            style.content="open-quote 'hello' counter(chapter, DECIMAL) / 'alt'";
            const content='open-quote "hello" counter(chapter) / "alt"';
            expect(style.content,content,'content IDL');
            expect(style.getPropertyValue('content'),content,'content declaration');
            style.quotes="'outer' 'close' 'inner' 'end'";
            expect(style.quotes,'"outer" "close" "inner" "end"','quotes IDL');
            const computed=getComputedStyle(el);
            expect(computed.content,content,'computed content');
            expect(computed.quotes,style.quotes,'computed quotes');
            style.content='counter(';
            expect(style.content,content,'invalid content preserves old value');
            style.content="'changed'";
            expect(computed.content,'"changed"','live computed content');
            style.quotes='none';
            expect(computed.quotes,'none','live computed quotes');
            return true;
        })()"#,
            )
            .unwrap()
            .unwrap_or_else(|error| {
                let message = match engine.ctx().member_get(&error, "message") {
                    Ok(Value::Str(message)) => message.to_string(),
                    _ => "non-string script exception".into(),
                };
                panic!("generated-content CSSOM regression: {message}");
            });
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn inline_styles_and_token_lists_use_only_null_namespace_attributes() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<main></main>", 128).unwrap();
        let result = engine.eval_value(r#"(() => {
            const ns = 'https://example.test/attributes';
            for (const el of [document.createElement('div'), document.createElementNS('http://www.w3.org/2000/svg', 'g')]) {
                el.setAttributeNS(ns, 'style', 'color: red');
                el.setAttributeNS(null, 'style', 'color: green');
                el.setAttributeNS(ns, 'style', 'color: blue');
                if (el.style.color !== 'green') return false;
                el.style.color = 'purple';
                if (el.getAttributeNS(ns, 'style') !== 'color: blue' || !el.getAttributeNS(null, 'style').includes('purple')) return false;
                el.setAttributeNS(ns, 'class', 'foreign');
                el.classList.add('native');
                if (el.classList.contains('foreign') || !el.classList.contains('native') || el.getAttributeNS(ns, 'class') !== 'foreign') return false;
                document.querySelector('main').append(el);
                if (document.getElementsByClassName('foreign').length !== 0) return false;
                const observer = new MutationObserver(() => {});
                observer.observe(el, {attributes: true});
                if (el.classList.toggle('native', true) !== true || el.classList.toggle('absent', false) !== false || observer.takeRecords().length !== 0) return false;
                el.classList.toggle('native', false);
                const records = observer.takeRecords();
                if (records.length !== 1 || records[0].attributeNamespace !== null) return false;
                observer.disconnect();
            }
            return document.getElementsByClassName('native').length === 0;
        })()"#).unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn cssom_setters_ignore_invalid_values_and_priorities_atomically() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<div></div>", 64).unwrap();
        let result = engine
            .eval_value(
                r#"
            const s = document.querySelector('div').style;
            s.setProperty('width', '10px', 'important');
            s.setProperty('width', 'bad');
            s.setProperty('width', '20px', 'urgent');
            s.setProperty('width', '30px !important');
            s.setProperty('width', 'var(not-custom)');
            const preserved = s.width === '10px' && s.getPropertyPriority('width') === 'important';
            s.setProperty('width', '', 'urgent');
            s.setProperty('width;bad', '');
            s.setProperty('height', 'var(--missing, var(--fallback, 12px))');
            preserved && s.width === '' && s.height === 'var(--missing, var(--fallback, 12px))'
        "#,
            )
            .unwrap()
            .ok()
            .unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn css_text_filters_invalid_declarations_and_preserves_priority() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<div></div>", 64).unwrap();
        let result = engine.eval_value(r#"
            const s = document.querySelector('div').style;
            s.cssText = 'width:10px! /**/ important /*tail*/;width:20px;height:bad;unknown:yes;--Case:red;--case:blue;color:rgb(1,2,3)';
            const valid = s.width === '10px' && s.height === '' && s.getPropertyValue('unknown') === '' &&
                s.getPropertyValue('--Case') === 'red' && s.getPropertyValue('--case') === 'blue';
            s.cssText = 'height:12px;width:calc(';
            valid && s.height === '12px' && s.width === ''
        "#).unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn computed_overflow_keeps_axis_values_and_attachment_layers() {
        let mut engine = Engine::new();
        install(
            engine.ctx(),
            "<div style='overflow:clip hidden;background-attachment:fixed,local'></div>",
            64,
        )
        .unwrap();
        let result = engine.eval_value(r#"
            const s = getComputedStyle(document.querySelector('div'));
            s.getPropertyValue('overflow-x') === 'hidden' && s.getPropertyValue('overflow-y') === 'hidden' &&
            s.getPropertyValue('overflow') === 'hidden' && s.getPropertyValue('background-attachment') === 'fixed, local'
        "#).unwrap().ok().unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn computed_border_radius_serializes_elliptical_values_and_font_units() {
        let mut engine = Engine::new();
        install(
            engine.ctx(),
            "<div style='font-size:16px;border-radius:calc(10px + 25%) 1em 25% 25px / calc(20px + 25%) 1em 25% 25px'></div>",
            64,
        )
        .unwrap();
        let result = engine
            .eval_value(
                r#"getComputedStyle(document.querySelector('div')).getPropertyValue('border-radius') ===
                    'calc(25% + 10px) 16px 25% 25px / calc(25% + 20px) 16px 25% 25px'"#,
            )
            .unwrap()
            .ok()
            .unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn computed_physical_border_radius_longhands_serialize_each_corner() {
        let mut engine = Engine::new();
        install(
            engine.ctx(),
            "<div style='font-size:20px;border-top-left-radius:1em 25%;border-top-right-radius:10%;border-bottom-right-radius:2rem 10px;border-bottom-left-radius:calc(10px + 5%)'></div>",
            64,
        )
        .unwrap();
        let result = engine
            .eval_value(
                r#"(() => {
                    const s = getComputedStyle(document.querySelector('div'));
                    return s.getPropertyValue('border-top-left-radius') === '20px 25%' &&
                        s.getPropertyValue('border-top-right-radius') === '10%' &&
                        s.getPropertyValue('border-bottom-right-radius') === '32px 10px' &&
                        s.getPropertyValue('border-bottom-left-radius') === 'calc(5% + 10px)';
                })()"#,
            )
            .unwrap()
            .ok()
            .unwrap();
        assert!(matches!(result, Value::Bool(true)));
    }
    #[test]
    fn computed_values_native_insets_and_grid_projection_follow_live_boxes_and_rules() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<style>#relative{position:relative;left:20%} .grid{grid-template:[head] \"a a\" auto [tail] / 1em 2fr}</style><div style='width:200px;height:100px'><div id=relative style='width:10px;height:10px'></div><div id=static style='top:10%;left:25%'></div></div><div class=grid></div>", 128).unwrap();
        realm.set_layout_flusher(Rc::new(|session| {
            session.display_list(300, 200, &NoText).map(|_| ()).map_err(|error| format!("{error:?}"))
        }));
        let result = engine.eval_value(r#"(() => {
            function eq(a,b) { if (a !== b) throw Error(a + ' !== ' + b); }
            const relative = document.getElementById('relative'), staticBox = document.getElementById('static');
            relative.getBoundingClientRect();
            const computed = getComputedStyle(relative);
            eq(computed.left, '40px'); eq(computed.right, '-40px'); eq(computed.top, '0px');
            eq(getComputedStyle(staticBox).top, '10%'); eq(getComputedStyle(staticBox).left, '25%');
            relative.style.display = 'none'; eq(computed.left, '20%'); eq(computed.right, 'auto');
            const detached = document.createElement('div'); detached.style.cssText = 'position:relative;left:30%';
            eq(getComputedStyle(detached).left, '30%'); eq(getComputedStyle(detached).right, 'auto');
            const rule = document.styleSheets[0].cssRules[1];
            eq(rule.style.gridTemplateRows, '[head] auto [tail]'); eq(rule.style.gridTemplateColumns, '1em 2fr');
            const element = document.querySelector('.grid'); element.style.cssText = 'grid:auto-flow 1em / 20px;font-size:20px';
            eq(element.style.gridAutoRows, '1em'); eq(getComputedStyle(element).gridAutoRows, '20px');
            element.style.gridTemplate = 'none'; eq(element.style.gridAutoRows, '1em');
            return true;
        })()"#).unwrap();
        let result = result.unwrap_or_else(|error| panic!("inset and grid native semantics threw: {}", engine.ctx().coerce_string(&error).map(|value| value.to_string()).unwrap_or_default()));
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn computed_values_native_live_case_registry_and_shared_iterator() {
        let mut engine = Engine::new();
        install(engine.ctx(), "<div style='font:italic bold 20px/1.5 serif;border:5px solid red;border-left-style:hidden;row-gap:normal;--Case:UP;--case:down'></div>", 128).unwrap();
        let result = engine.eval_value(r#"(() => {
            const element = document.querySelector('div'), computed = getComputedStyle(element);
            function eq(actual, expected) { if (actual !== expected) throw Error(actual + ' !== ' + expected); }
            eq(computed.getPropertyValue('FONT-FAMILY'), 'serif');
            eq(computed.lineHeight, '30px'); eq(computed.borderLeftWidth, '0px');
            eq(computed.getPropertyValue('--Case'), 'UP'); eq(computed.getPropertyValue('--case'), 'down');
            eq(computed.rowGap, 'normal');
            for (const name of computed) { if (!computed.getPropertyValue(name)) throw Error('missing computed value for ' + name); }
            const names = [...element.style]; if (!names.includes('font-size') || names.includes('font')) throw Error('specified iteration');
            element.style.borderLeftStyle = 'dotted'; eq(computed.borderLeftWidth, '5px');
            element.style.fontSize = '10px'; eq(computed.lineHeight, '15px');
            let threw = false; try { computed.color = 'blue'; } catch (e) { threw = e.name === 'NoModificationAllowedError'; } eq(threw, true);
            return true;
        })()"#).unwrap().ok().expect("computed native values");
        assert!(matches!(result, Value::Bool(true)));
    }

    #[test]
    fn specification_appearance_native_primitive_children_cssom_and_activation() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), r#"<!doctype html><style>
            input{display:block;width:100px;height:40px;border:0;padding:0}
            input::before{content:'';display:block;width:10%;height:10px;background:lime}
            input span{display:block;width:10%;height:10px;background:red}
        </style><input id=native type=checkbox checked><input id=primitive type=checkbox style='appearance:none'>
        <input id=radio type=radio style='-webkit-appearance:none'><input id=text value=AB style='appearance:none'>
        <div id=plain style='appearance:checkbox'></div>"#,256).unwrap();
        realm.set_layout_flusher(Rc::new(|session|session.display_list(400,300,super::super::canvas::canvas_fallback_fonts())
            .map(|_|()).map_err(|error|format!("{error:?}"))));
        let result = engine.eval_value(r#"(() => {
            const expect=(value,message)=>{if(!value)throw Error(message)};
            for(const id of ['native','primitive','radio','text']){
                const input=document.getElementById(id),child=document.createElement('span');child.id=id+'Child';input.append(child);
            }
            const native=document.getElementById('native'),primitive=document.getElementById('primitive'),radio=document.getElementById('radio'),text=document.getElementById('text');
            expect(getComputedStyle(native).appearance==='auto','real UA appearance');
            expect(getComputedStyle(primitive).appearance==='none'&&getComputedStyle(radio).webkitAppearance==='none','native alias computed values');
            for(const input of [primitive,radio]){
                expect(getComputedStyle(input,'::before').width==='10px','primitive check/radio actual generated box: '+getComputedStyle(input,'::before').width);
                expect(getComputedStyle(input.firstChild).width==='10px','primitive ordinary child');
            }
            for(const input of [native,text]){
                expect(getComputedStyle(input,'::before').width==='10%','native or semantic text replacement suppresses generated box');
                expect(getComputedStyle(input.firstChild).width==='10%','replacement suppresses ordinary children');
            }
            primitive.style.display='inline';
            expect(getComputedStyle(primitive).width==='100px','primitive Inline width uses computed CSS, not a replaced used width');
            primitive.style.display='block';
            let changes=0;primitive.addEventListener('change',()=>++changes);primitive.click();
            expect(primitive.checked&&changes===1,'primitive presentation preserves native activation');
            expect(!document.getElementById('plain').matches(':enabled,:disabled'),'compat keyword cannot create control semantics');
            primitive.style.appearance='checkbox';
            expect(getComputedStyle(primitive).appearance==='checkbox','compat-auto keeps keyword identity');
            expect(getComputedStyle(primitive,'::before').width==='10%','changing appearance restores replacement lifecycle');
            primitive.style.setProperty('-webkit-appearance','none');
            expect(getComputedStyle(primitive,'::before').width==='10px','alias modifies same presentation state');
            primitive.style.appearance='bogus';expect(primitive.style.appearance==='none','invalid assignment is atomic');
            const child=document.createElement('input');primitive.append(child);
            expect(getComputedStyle(child).appearance==='auto','appearance is not inherited');
            return true;
        })()"#).unwrap().unwrap_or_else(|error|panic!("appearance native lifecycle guard: {}",engine.ctx().coerce_string(&error).map(|text|text.to_string()).unwrap_or_else(|_|"unprintable JavaScript exception".into())));
        assert!(matches!(result,Value::Bool(true)));
        // The final appearance read above is deliberately non-geometric;
        // render the appended child before querying completed box metadata.
        realm.flush_layout().unwrap();
        realm.with_session(|session| {
            let target=lumen_html::selector::query_selector(session.document(),session.document().root(),"#primitive").unwrap().unwrap();
            let child=lumen_html::selector::query_selector(session.document(),session.document().root(),"#primitiveChild").unwrap().unwrap();
            assert!(session.layout_rect(child).is_some());
            assert_eq!(session.pseudo_layout_rect(target,css::PseudoElement::Before).unwrap().width,10.0);
            let cached=session.display_list(400,300,super::super::canvas::canvas_fallback_fonts()).unwrap().clone();
            let mut fresh=lumen_html::session::RenderSession::new(session.document().clone_document(true).unwrap());
            assert_eq!(cached,fresh.display_list(400,300,super::super::canvas::canvas_fallback_fonts()).unwrap().clone());
        });
    }

    #[test]
    fn specification_appearance_live_values_and_checkedness_invalidate_pixels_without_attributes() {
        let mut engine = Engine::new();
        let source = r#"<!doctype html><style>body{margin:0}
            input,textarea,select{display:block;width:100px;height:20px;border:0;padding:0;margin:0;color:black;background:white}
        </style><input id=range type=range value=0 min=0 max=100><input id=color type=color value='#ff0000'>
        <input id=check type=checkbox><input id=text value=old><textarea id=area>old</textarea>
        <select id=choice><option value=a>A</option><option value=b>B</option></select>
        <select id=list multiple size=2><option>A</option><option>B</option></select>"#;
        let realm = install(engine.ctx(), source,128).unwrap();
        let fonts=super::super::canvas::canvas_fallback_fonts();
        realm.set_layout_flusher(Rc::new(|session|session.display_list(300,200,super::super::canvas::canvas_fallback_fonts())
            .map(|_|()).map_err(|error|format!("{error:?}"))));
        let before=realm.with_session(|session|session.display_list(300,200,fonts).unwrap().clone());
        let result=engine.eval_value(r#"(() => {
            const e=id=>document.getElementById(id);
            e('range').value='100';e('color').value='#00ff00';e('check').checked=true;
            e('text').value='updated';e('area').value='updated';e('choice').value='b';
            if(e('range').getAttribute('value')!=='0'||e('color').getAttribute('value')!=='#ff0000'||e('text').getAttribute('value')!=='old'||e('area').textContent!=='old'||e('check').hasAttribute('checked'))throw Error('live state rewrote authored defaults');
            if(e('choice').selectedIndex!==1||e('choice').options[1].selected!==true)throw Error('live selectedness');
            return true;
        })()"#).unwrap().ok().expect("live input value guard");
        assert!(matches!(result,Value::Bool(true)));
        realm.with_session(|session| {
            let after=session.display_list(300,200,fonts).unwrap().clone();
            assert_ne!(before,after,"live state must invalidate the completed display list without a DOM mutation");
            let find=|id|lumen_html::selector::get_element_by_id(session.document(),session.document().root(),id).unwrap().unwrap();
            let range=session.layout_rect(find("range")).unwrap();
            let color=session.layout_rect(find("color")).unwrap();
            let check=session.layout_rect(find("check")).unwrap();
            let image=lumen_html_image::render_with_font(&after,300,200,1.0,true,fonts).unwrap();
            let pixel=|rect:lumen_html::paint::Rect| {
                let x=(rect.x+rect.width*0.5) as usize;let y=(rect.y+rect.height*0.5) as usize;
                &image.pixels[(y*300+x)*4..(y*300+x)*4+4]
            };
            let before_image=lumen_html_image::render_with_font(&before,300,200,1.0,true,fonts).unwrap();
            let left_x=(range.x+8.0) as usize;let right_x=(range.x+range.width-8.0) as usize;
            let y=(range.y+range.height*0.5) as usize;
            assert_eq!(&before_image.pixels[(y*300+left_x)*4..(y*300+left_x)*4+4],&[0,0,0,255]);
            assert_eq!(&image.pixels[(y*300+right_x)*4..(y*300+right_x)*4+4],&[0,0,0,255]);
            assert_ne!(&image.pixels[(y*300+left_x)*4..(y*300+left_x)*4+4],&[0,0,0,255]);
            assert_eq!(pixel(color),&[0,255,0,255]);

            assert_eq!(pixel(check),&[0,0,0,255]);
            let text=find("text");let area=find("area");
            assert_eq!(session.control_text_runs(text).map(|run|run.range.end).max(),Some(7));
            assert_eq!(session.control_text_runs(area).map(|run|run.range.end).max(),Some(7));
            let frame=session.frame_id();let revision=session.paint_revision();
            assert_eq!(session.display_list(300,200,fonts).unwrap(),&after);
            assert_eq!(session.frame_id(),frame);assert_eq!(session.paint_revision(),revision);
            let reference=source.replace("type=range value=0", "type=range value=100")
                .replace("value='#ff0000'", "value='#00ff00'")
                .replace("type=checkbox>", "type=checkbox checked>")
                .replace("id=text value=old", "id=text value=updated")
                .replace("<textarea id=area>old", "<textarea id=area>updated")
                .replace("<option value=b>", "<option value=b selected>");
            let mut fresh_session=lumen_html::session::RenderSession::new(lumen_html::html::parse(&reference,128).unwrap());
            let fresh=fresh_session.display_list(300,200,fonts).unwrap().clone();
            assert_eq!(fresh,after,"fresh layout and retained live-state layout must agree");
            assert_eq!(lumen_html_image::render_with_font(&fresh,300,200,1.0,true,fonts).unwrap().pixels,image.pixels);
        });
    }

    #[test]
    fn specification_appearance_legacy_alias_cssom_inline_rule_and_computed_identity() {
        let mut engine=Engine::new();
        let realm=install(engine.ctx(),"<!doctype html><style>#target{-webkit-appearance:none}</style><input id=target>",128).unwrap();
        realm.set_layout_flusher(Rc::new(|session|session.display_list(300,200,&NoText).map(|_|()).map_err(|error|format!("{error:?}"))));
        let result=engine.eval_value(r#"(() => {
            const expect=(yes,message)=>{if(!yes)throw Error(message)};
            const target=document.getElementById('target'),style=target.style;
            for(const name of ['appearance','-webkit-appearance','WebkitAppearance','webkitAppearance']){
                style.cssText='';style[name]='none';
                expect(style.length===1&&style.item(0)==='appearance'&&style.cssText==='appearance: none;','canonical declaration '+name);
                for(const alias of ['appearance','-webkit-appearance','WebkitAppearance','webkitAppearance'])expect(style[alias]==='none','IDL alias '+alias);
                expect(style.getPropertyValue('-webkit-appearance')==='none'&&style.getPropertyValue('appearance')==='none','shared CSS name');
                expect(style.removeProperty('-webkit-appearance')==='none'&&style.length===0,'shared removal');
            }
            for(const value of ['initial','inherit','unset','revert','revert-layer']){
                style.setProperty('-webkit-appearance',value,'important');
                expect(style.appearance===value&&style.getPropertyPriority('appearance')==='important','wide/priority shared slot '+value);
                expect(style.removeProperty('appearance')===value,'canonical removal '+value);
            }
            style.cssText='appearance: auto!important;-webkit-appearance:none';
            expect(style.webkitAppearance==='auto'&&style.length===1,'source cascade across aliases');
            style.webkitAppearance=null;expect(style.length===0,'LegacyNullToEmptyString removal');
            style.setProperty('webkitAppearance','none');style.setProperty('WebkitAppearance','none');
            expect(style.length===0,'CSS methods do not accept IDL names');
            const rule=document.styleSheets[0].cssRules[0];
            expect(rule.style.cssText==='appearance: none;'&&rule.style.webkitAppearance==='none','rule serialization');
            rule.style.WebkitAppearance='auto';expect(rule.style.getPropertyValue('appearance')==='auto','rule live alias mutation');
            const computed=getComputedStyle(target);
            expect(computed.appearance==='auto'&&computed.webkitAppearance==='auto'&&computed.WebkitAppearance==='auto','computed IDL aliases');
            let count=0;for(let i=0;i<computed.length;i++){if(computed.item(i)==='appearance')count++;expect(computed.item(i)!=='-webkit-appearance','computed canonical enumeration')}
            expect(count===1,'computed one canonical identity');
            let readonly=false;try{computed.webkitAppearance='none'}catch(error){readonly=error.name==='NoModificationAllowedError'}
            expect(readonly,'computed alias preserves readonly brand path');
            return true;
        })()"#).unwrap().ok().expect("legacy CSSOM alias guard");
        assert!(matches!(result,Value::Bool(true)));
    }
    #[test]
    fn specification_cssom_animation_controls_canonical_roundtrip_and_resolved_timeline_context() {
        let mut engine=Engine::new();
        install(engine.ctx(),"<!doctype html><div id=target></div>",128).unwrap();
        let result=engine.eval_value(r#"(() => {
            const expect=(yes,message)=>{if(!yes)throw Error(message)};
            const target=document.getElementById('target'),style=target.style;
            for(const [raw,value] of [['NONE','none'],['"something"','something'],['"multi word"','multi\\ word'],['"NoNe"','"NoNe"']]){
                style.animationName=raw;expect(style.animationName===value,'specified name '+raw+' -> '+style.animationName);
                expect(getComputedStyle(target).animationName===value,'computed name '+raw);
            }
            style.animationName='retained';
            const names=new CSSStyleSheet();
            for(const raw of ['""', "''", '"\\\n"']) {
                style.animationName=raw;
                expect(style.animationName==='retained'&&!CSS.supports('animation-name',raw),'empty decoded name rejected '+JSON.stringify(raw));
                let rejected=false;
                try{names.insertRule('@keyframes '+raw+' {from{opacity:0}}')}catch(error){rejected=error.name==='SyntaxError'}
                expect(rejected&&names.cssRules.length===0,'empty keyframes name rejected '+JSON.stringify(raw));
            }
            for(const raw of ['"none"','"initial"','"inherit"','"unset"','"revert"','"revert-layer"','"default"']) {
                expect(CSS.supports('animation-name',raw),'quoted reserved name valid '+raw);
                names.insertRule('@keyframes '+raw+' {from{opacity:0}}',names.cssRules.length);
            }
            for(const [property,raw,canonical] of [['animationDirection','REVERSE','reverse'],['animationFillMode','BOTH','both'],['animationPlayState','PAUSED','paused'],['animationComposition','ADD','add']]){
                style[property]=raw;expect(style[property]===canonical&&getComputedStyle(target)[property]===canonical,'canonical keyword '+property);
            }
            style.cssText='animation:1s';
expect(style.animation==='1s'&&getComputedStyle(target).animation==='1s','minimal shorthand');
            style.cssText='animation-delay-start:1s';expect(getComputedStyle(target).animation==='0s 1s','duration before delay');
            style.cssText='animation-duration:auto,auto';
            expect(style.animationDuration==='auto, auto'&&getComputedStyle(target).animationDuration==='0s, 0s','computed auto preserved, resolved document duration');
            for(const timeline of ['auto, auto','none','--named','scroll()','view()']){
                style.animationTimeline=timeline;expect(getComputedStyle(target).animationDuration==='auto, auto','exact timeline compatibility '+timeline);
            }
            style.cssText='font-size:24px;animation-range-start:calc(1em + 10%);animation-range-end:120%';
            expect(getComputedStyle(target).animationRangeStart==='calc(10% + 24px)','contextual range');
            expect(style.animationRangeEnd==='120%'&&getComputedStyle(target).animationRangeEnd==='120%','exact percentage precision');
            style.animationRangeStart='0%';expect(getComputedStyle(target).animationRangeStart==='0%','percentage zero retains type');
            style.animation='2s ease --timeline ease';
            const roundtrip=style.cssText;style.cssText=roundtrip;
            expect(style.animationName==='ease'&&style.animationTimeline==='--timeline','ambiguous name/timeline roundtrip');
            style.animationRangeEnd='exit 70%';expect(style.animation===''&&style.animationRangeEnd==='exit 70%','noninitial reset-only endpoint blocks shorthand');
            style.animation='spin 1s';expect(style.animationRangeStart==='normal'&&style.animationRangeEnd==='normal','shorthand resets endpoints');
            return true;
        })()"#).unwrap().ok().expect("animation control CSSOM guard");
        assert!(matches!(result,Value::Bool(true)));
    }

    #[test]
    fn specification_font_relative_units_native_controls_dynamic_root_and_computed_context() {
        let mut engine=Engine::new();
        let realm=install(engine.ctx(),r#"<!doctype html><style>
            html{font-size:20px;line-height:2}body{margin:0}
            textarea,input{appearance:none;display:block;padding:0;border:0;width:100px;box-sizing:content-box}
            textarea{font-size:10px;line-height:3;height:2lh}
            input{font-size:20px;line-height:1px;height:2lh}
            #rootLine{height:2rlh;width:1rcap}
        </style><textarea id=area>value</textarea><input id=control><div id=rootLine></div>"#,128).unwrap();
        let fonts=crate::canvas::realm_font_source(&realm).unwrap();
        realm.set_layout_flusher(Rc::new(move|session|session.display_list(300,400,&fonts)
            .map(|_|()).map_err(|error|format!("{error:?}"))));
        let result=engine.eval_value(r#"(() => {
            const area=document.getElementById('area'),control=document.getElementById('control'),rootLine=document.getElementById('rootLine');
            if(getComputedStyle(area).height!=='60px')throw new Error('textarea 2lh did not use computed font/line-height');
            if(getComputedStyle(control).height!=='2px')throw new Error('control used line-height clamp affected lh');
            if(getComputedStyle(rootLine).height!=='80px')throw new Error('rlh lost actual root line-height');
            if(rootLine.computedStyleMap().get('height').value!==80)throw new Error('computed font unit did not become absolute');
            document.documentElement.style.fontSize='30px';
            if(getComputedStyle(rootLine).height!=='120px')throw new Error('root font mutation did not invalidate rlh');
            area.style.lineHeight='4';
            if(getComputedStyle(area).height!=='80px')throw new Error('line-height mutation did not invalidate lh');
            area.style.height='min(2lh, 90px)';
            if(getComputedStyle(area).height!=='80px')throw new Error('lh failed canonical nonlinear numeric builder');
            return true;
        })()"#).unwrap().unwrap_or_else(|error|panic!("font-unit native guard: {}",engine.ctx().coerce_string(&error).map(|text|text.to_string()).unwrap_or_default()));
        assert!(matches!(result,Value::Bool(true)));
        realm.flush_layout().unwrap();
        let session=realm.session.borrow();
        let area=lumen_html::selector::get_element_by_id(session.document(),session.document().root(),"area").unwrap().unwrap();
        assert_eq!(session.layout_rect(area).unwrap().height,80.0);
    }

    #[test]
    fn specification_text_entry_native_line_height_preserves_cssom_and_uses_font_metrics() {
        let mut engine = Engine::new();
        let realm = install(engine.ctx(), "<!doctype html><style>html,body{margin:0}input{display:block;width:100px;height:60px;padding:0;border:0;font-size:20px;line-height:1px}</style><input id=control style=appearance:none>",128).unwrap();
        let fonts = crate::canvas::realm_font_source(&realm).unwrap();
        realm.set_layout_flusher(Rc::new(move |session| {
            session.display_list(300,200,&fonts).map(|_|()).map_err(|error|format!("{error:?}"))
        }));
        let answer = engine.eval_value(r#"(() => {
            const control = document.getElementById("control");
            const used = getComputedStyle(control).lineHeight;
            if (used === "1px" || used === "normal" || !used.endsWith("px")) throw new Error("resolved line-height did not expose the actual used length");
            if (getComputedStyle(control).width !== "100px") throw new Error("actual control box was not rendered");
            const computed = control.computedStyleMap();
            const line = computed.get('line-height');
            if (line.value !== 1 || line.unit !== 'px') throw new Error('computed map exposed the used line-height clamp');
            control.style.lineHeight = '2';
            const number = computed.get('line-height');
            if (number.value !== 2 || number.unit !== 'number') throw new Error('computed unitless line-height lost its type');
            control.style.width = '50%';
            const width = computed.get('width');
            if (width.value !== 50 || width.unit !== 'percent') throw new Error('computed map exposed used box width');
            if (getComputedStyle(control).width !== '150px') throw new Error('resolved box width did not use its containing block');
            control.style.width = '100px';
            control.style.lineHeight = '1px';
            control.style.transform = 'translate(10px, 20px) scale(2)';
            const transform = computed.get('transform');
            if (transform.length !== 2 || !(transform[0] instanceof CSSTranslate) || !(transform[1] instanceof CSSScale)) throw new Error('computed transform list was flattened to its resolved matrix');
            if (!getComputedStyle(control).transform.startsWith('matrix(')) throw new Error('resolved transform must expose its actual matrix');
            control.style.transform = 'none';
            getComputedStyle(control).width;
            return true;
        })()"#).unwrap().ok().expect("native CSSOM reads succeed");
        assert!(matches!(answer,Value::Bool(true)));
        let session = realm.session.borrow();
        let control = lumen_html::selector::get_element_by_id(session.document(),session.document().root(),"control").unwrap().unwrap();
        let run = session.control_text_runs(control).next().expect("real control has caret geometry");
        let measured = run.height;
        drop(session);
        let fonts = crate::canvas::realm_font_source(&realm).unwrap();
        let computed = realm.session.borrow_mut().computed_style_with_text(control,Some(&fonts)).unwrap();
        assert_eq!(computed.computed_css_value("line-height",Default::default()).unwrap(),"1px","actual computed carrier remains authored");
        let used = lumen_html::layout::text_entry_used_line_height(realm.session.borrow().document(),control,&computed,&fonts).unwrap();
        assert_eq!(measured,used,"CSSOM and actual caret geometry share the installed font metric");
    }

    #[test]
    fn specification_placeholder_native_cssom_and_dirty_value_share_actual_paint() {
        let mut engine=Engine::new();
        let realm=install(engine.ctx(),r#"<!doctype html><style>
            html,body{margin:0}input{display:block;width:120px;height:40px;padding:0;border:0;
                appearance:none;font-size:10px;line-height:20px;color:blue;background:lime}
            input::placeholder{color:red;font-size:30px;background-color:inherit;
                line-height:0;vertical-align:100px;direction:rtl;writing-mode:vertical-rl}
        </style><input id=control value=authored placeholder=Hint>"#,128).unwrap();
        let fonts=crate::canvas::realm_font_source(&realm).unwrap();
        realm.set_layout_flusher(Rc::new(move |session|session.display_list(240,160,&fonts)
            .map(|_|()).map_err(|error|format!("{error:?}"))));
        let answer=engine.eval_value(r#"(() => {
            const input=document.getElementById('control');
            const pseudo=getComputedStyle(input,'::placeholder');
            if(pseudo.color!=='rgb(255, 0, 0)'||pseudo.fontSize!=='30px'||pseudo.backgroundColor!=='rgb(0, 255, 0)')throw new Error('real placeholder cascade/inheritance');
            if(pseudo.lineHeight!=='20px'||pseudo.writingMode!=='horizontal-tb'||pseudo.direction!=='ltr')throw new Error('excluded inline properties changed the placeholder context');
            input.style.lineHeight='2';
            if(getComputedStyle(input,'::placeholder').lineHeight!==getComputedStyle(input).lineHeight)throw new Error('placeholder font must not rebase the control unitless line-height');
            input.value='';
            if(!input.matches(':placeholder-shown')||input.getAttribute('value')!=='authored')throw new Error('dirty value must show placeholder without attribute reflection');
            return true;
        })()"#).unwrap().unwrap_or_else(|reason|panic!("placeholder native guard: {}",engine.ctx().coerce_string(&reason).map(|text|text.to_string()).unwrap_or_else(|_|"unprintable thrown value".into())));
        assert!(matches!(answer,Value::Bool(true)));
        for (line_height,value,red) in [("2","",true),("2","live",false),("2","",true),
            ("normal","",true),("normal","live",false),("normal","",true)] {
            engine.eval_value(&format!("document.getElementById('control').style.lineHeight='{line_height}'")).unwrap().ok().expect("actual originating line-height update");
            let source=format!("document.getElementById('control').value='{value}'");
            engine.eval_value(&source).unwrap().ok().expect("real dirty-value update");
            realm.flush_layout().unwrap();
            let session=realm.session.borrow();
            let control=lumen_html::selector::get_element_by_id(session.document(),session.document().root(),"control").unwrap().unwrap();
            assert!(session.control_text_runs(control).any(|run|run.placeholder==red),"live value selects actual editable/placeholder source");
            drop(session);
            let fonts=crate::canvas::realm_font_source(&realm).unwrap();
            let mut session=realm.session.borrow_mut();
            let origin=session.computed_style_with_text(control,Some(&fonts)).unwrap();
            let expected=lumen_html::layout::text_entry_used_line_height(session.document(),control,&origin,&fonts).unwrap();
            assert!(session.control_text_runs(control).any(|run|run.placeholder==red&&run.height==expected),
                "Number/normal line geometry must use the actual originating control metric, not the placeholder font");
            assert_eq!(session.document().get_attribute_ns_ref(control,None,"value").unwrap(),Some("authored"));
            drop(session);
            let fonts=crate::canvas::realm_font_source(&realm).unwrap();
            let mut session=realm.session.borrow_mut();
            let cached=session.display_list(240,160,&fonts).unwrap().clone();
            assert!(cached.0.iter().any(|command|matches!(command,lumen_html::paint::Command::GlyphRun{color,..}
                if *color==if red{lumen_html::paint::Rgba{r:255,g:0,b:0,a:255}}else{lumen_html::paint::Rgba{r:0,g:0,b:255,a:255}})));
            let image=lumen_html_image::render_with_font(&cached,240,160,1.0,true,&fonts).unwrap();
            assert!(image.pixels.chunks_exact(4).any(|pixel|if red {pixel[0]>0&&pixel[2]==0}else{pixel[2]>0&&pixel[0]==0}),
                "actual font raster carries the current placeholder/value color");
        }
    }

    #[test]
    fn specification_placeholder_native_textarea_uses_real_origin_lines_and_dirty_values() {
        let mut engine=Engine::new();
        let realm=install(engine.ctx(),r#"<!doctype html><style>
            html,body{margin:0}textarea{appearance:none;display:block;width:120px;height:60px;
                border:0;padding:0;font-size:10px;line-height:2;color:blue}
            textarea::placeholder{font-size:30px;line-height:100px;color:red}
        </style><textarea id=control placeholder=Hint>authored</textarea>"#,128).unwrap();
        let fonts=crate::canvas::realm_font_source(&realm).unwrap();
        realm.set_layout_flusher(Rc::new(move |session|session.display_list(240,160,&fonts)
            .map(|_|()).map_err(|error|format!("{error:?}"))));
        for line_height in ["2","normal"] {
            for value in ["","live",""] {
                let source=format!(r#"(() => {{
                    const input=document.getElementById('control');
                    input.style.lineHeight='{line_height}';input.value='{value}';
                    const origin=getComputedStyle(input),hint=getComputedStyle(input,'::placeholder');
                    if(hint.fontSize!=='30px'||hint.lineHeight!==origin.lineHeight)throw new Error('textarea hint rebased the originating line metric');
                    return input.textContent==='authored'&&input.value==='{value}';
                }})()"#);
                let answer=engine.eval_value(&source).unwrap().unwrap_or_else(|reason|panic!("textarea placeholder: {}",
                    engine.ctx().coerce_string(&reason).map(|text|text.to_string()).unwrap_or_default()));
                assert!(matches!(answer,Value::Bool(true)),"dirty value never reflects into authored textarea children");
                realm.flush_layout().unwrap();
                let fonts=crate::canvas::realm_font_source(&realm).unwrap();
                let mut session=realm.session.borrow_mut();
                let control=lumen_html::selector::get_element_by_id(session.document(),session.document().root(),"control").unwrap().unwrap();
                let origin=session.computed_style_with_text(control,Some(&fonts)).unwrap();
                let expected=lumen_html::layout::placeholder_used_line_height(session.document(),control,&origin,&fonts).unwrap();
                assert!(session.control_text_runs(control).any(|run|run.placeholder==value.is_empty()&&run.height==expected),
                    "real native textarea uses its own Number/normal line metric for both value and hint");
            }
        }
    }

}
