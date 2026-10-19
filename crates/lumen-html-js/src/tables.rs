//! Typed HTML table interfaces using shared tree plans and ordinary DOM mutation.
use super::*;
use lumen_bind::This;
use lumen_html::tables::{self, CollectionKind};

pub(crate) fn node_at(realm: &Rc<DomRealm>, id: NodeId) -> DomNode {
    DomNode { base: DomEventTarget::node(realm, id), realm: realm.clone(), id, collections: RefCell::new(HashMap::new()) }
}

fn create(node: &DomNode, ctx: &mut Ctx, local: &str) -> OpResult<Value> {
    let owner = node.realm.session.borrow().document().node_document(node.id).map_err(dom_error)?;
    let document = DomDocument { base: node_at(&node.realm, owner), realm: node.realm.clone(), fonts: RefCell::new(None) };
    document.create_element_ns(ctx, Some("http://www.w3.org/1999/xhtml"), local, None)
}

fn remove(node: &DomNode, ctx: &mut Ctx, id: NodeId) -> OpResult<()> {
    let parent = node.realm.session.borrow().document().parent(id).map_err(dom_error)?;
    if let Some(parent) = parent { node_at(&node.realm, parent).remove_child(ctx, &node_at(&node.realm, id))?; }
    Ok(())
}

fn collection(node: &DomNode, ctx: &mut Ctx, owner: Value, key: &'static str, kind: CollectionKind) -> Value {
    if let Some(value) = node.collections.borrow().get(key).and_then(WeakValue::upgrade) { return value; }
    let value = DomHtmlCollection::create(ctx, DomNodeList::table(node.realm.clone(), node.id, kind, owner));
    node.collections.borrow_mut().insert(key.to_owned(), ctx.weak_value(&value).expect("table collection"));
    value
}

fn first(node: &DomNode, ctx: &mut Ctx, tag: &str) -> OpResult<Value> {
    let id = tables::first_named(node.realm.session.borrow().document(), node.id, tag).map_err(dom_error)?;
    Ok(node.realm.wrap_option(ctx, id))
}

fn head_before(node: &DomNode) -> OpResult<Option<NodeId>> {
    let session = node.realm.session.borrow();
    let document = session.document();
    let mut child = document.first_child(node.id).map_err(dom_error)?;
    while let Some(id) = child {
        if matches!(document.kind(id).map_err(dom_error)?, NodeKind::Element { .. })
            && !matches!(lumen_html::forms::html_element_local_name(document, id), Some("caption" | "colgroup")) { return Ok(Some(id)); }
        child = document.next_sibling(id).map_err(dom_error)?;
    }
    Ok(None)
}

fn insert_special(node: &DomNode, ctx: &mut Ctx, tag: &str, value: Value) -> OpResult<()> {
    let before = match tag {
        "caption" => node.realm.session.borrow().document().first_child(node.id).map_err(dom_error)?,
        "thead" => head_before(node)?,
        _ => None,
    };
    let before = node.realm.wrap_option(ctx, before);
    insert_dom_node(ctx, &node.realm, node.id, value, before)?;
    Ok(())
}

fn create_special(node: &DomNode, ctx: &mut Ctx, tag: &str) -> OpResult<Value> {
    if let Some(id) = tables::first_named(node.realm.session.borrow().document(), node.id, tag).map_err(dom_error)? { return Ok(node.realm.wrap(ctx, id)); }
    let value = create(node, ctx, tag)?;
    insert_special(node, ctx, tag, value.clone())?;
    Ok(value)
}

fn delete_special(node: &DomNode, ctx: &mut Ctx, tag: &str) -> OpResult<()> {
    let id = tables::first_named(node.realm.session.borrow().document(), node.id, tag).map_err(dom_error)?;
    if let Some(id) = id { remove(node, ctx, id)?; }
    Ok(())
}

fn set_special(node: &DomNode, ctx: &mut Ctx, tag: &str, replacement: Option<Value>) -> OpResult<()> {
    if let Some(value) = replacement.as_ref() {
        if tag == "caption" {
            ctx.with_instance::<DomHtmlTableCaptionElement, _>(value, |_| ())?;
        } else {
            ctx.with_instance::<DomHtmlTableSectionElement, _>(value, |_| ())?;
        }
        let valid = ctx.with_instance::<DomNode, _>(value, |replacement| {
            if tag == "caption" {
                lumen_html::forms::html_element_local_name(replacement.realm.session.borrow().document(), replacement.id) == Some("caption")
            } else {
                lumen_html::forms::html_element_local_name(replacement.realm.session.borrow().document(), replacement.id) == Some(tag)
            }
        })?;
        if !valid { return Err(OpError::new("HierarchyRequestError", "table child has the wrong local name")); }
    }
    // Keep the wrapper, releasing its native projection before adoption.
    delete_special(node, ctx, tag)?;
    if let Some(value) = replacement { insert_special(node, ctx, tag, value)?; }
    Ok(())
}

fn insert_member(node: &DomNode, ctx: &mut Ctx, kind: CollectionKind, index: i32) -> OpResult<Value> {
    let plan = tables::insertion(node.realm.session.borrow().document(), node.id, kind, index).map_err(dom_error)?;
    node.realm.prepare_allocation(ctx, if plan.create_body { 2 } else { 1 })?;
    let member = create(node, ctx, if kind == CollectionKind::Cells { "td" } else { "tr" })?;
    if plan.create_body {
        let body = create(node, ctx, "tbody")?;
        let body_id = ctx.with_instance::<DomNode, _>(&body, |node| node.id).map_err(|_| OpError::type_error("tbody is not a node"))?;
        insert_dom_node(ctx, &node.realm, body_id, member.clone(), Value::Null)?;
        insert_dom_node(ctx, &node.realm, node.id, body, Value::Null)?;
    } else {
        let before = node.realm.wrap_option(ctx, plan.before);
        insert_dom_node(ctx, &node.realm, plan.parent, member.clone(), before)?;
    }
    Ok(member)
}

fn delete_member(node: &DomNode, ctx: &mut Ctx, kind: CollectionKind, index: i32) -> OpResult<()> {
    let id = tables::deletion(node.realm.session.borrow().document(), node.id, kind, index).map_err(dom_error)?;
    if let Some(id) = id { remove(node, ctx, id)?; }
    Ok(())
}

macro_rules! table_interface {
    ($ty:ident, $interface:literal, {$($members:tt)*}) => {
        #[lumen_bind::class(name = $interface, extends = DomHtmlElement, hint(js(webidl)))]
        pub(crate) struct $ty { pub(crate) base: DomHtmlElement }
        #[lumen_bind::methods]
        impl $ty {
            #[constructor]
            fn new(ctx: &mut Ctx, this: This<Value>) -> OpResult<crate::custom_elements::HtmlElementCtor> {
                crate::custom_elements::construct_customized_interface(ctx, this.0, $interface)
            }
            $($members)*
        }
    };
}
table_interface!(DomHtmlTableElement,"HTMLTableElement",{
    #[getter]
    fn rows(&self, ctx:&mut Ctx, this:This<Value>) -> Value { collection(&self.base.base.base,ctx,this.0,"table:TableRows",CollectionKind::TableRows) }
    #[method(name = "insertRow", coerce, hint(js(ce_reactions)))]
    fn insert_member(&self,ctx:&mut Ctx,#[default(-1)] index:i32)->OpResult<Value>{insert_member(&self.base.base.base,ctx,CollectionKind::TableRows,index)}
    #[method(name = "deleteRow", coerce, hint(js(ce_reactions)))]
    fn delete_member(&self,ctx:&mut Ctx,index:i32)->OpResult<()>{delete_member(&self.base.base.base,ctx,CollectionKind::TableRows,index)}

    #[getter(name = "caption")] fn caption(&self,ctx:&mut Ctx)->OpResult<Value>{first(&self.base.base.base,ctx,"caption")}
    #[setter(name = "caption", hint(js(ce_reactions)))] fn set_caption(&self,ctx:&mut Ctx,value:Value)->OpResult<()>{set_special(&self.base.base.base,ctx,"caption",if matches!(value,Value::Null|Value::Undefined){None}else{Some(value)})}
    #[method(name = "createCaption", hint(js(ce_reactions)))] fn create_caption(&self,ctx:&mut Ctx)->OpResult<Value>{create_special(&self.base.base.base,ctx,"caption")}
    #[method(name = "deleteCaption", hint(js(ce_reactions)))] fn delete_caption(&self,ctx:&mut Ctx)->OpResult<()>{delete_special(&self.base.base.base,ctx,"caption")}

    #[getter(name = "tHead")] fn t_head(&self,ctx:&mut Ctx)->OpResult<Value>{first(&self.base.base.base,ctx,"thead")}
    #[setter(name = "tHead", hint(js(ce_reactions)))] fn set_t_head(&self,ctx:&mut Ctx,value:Value)->OpResult<()>{set_special(&self.base.base.base,ctx,"thead",if matches!(value,Value::Null|Value::Undefined){None}else{Some(value)})}
    #[method(name = "createTHead", hint(js(ce_reactions)))] fn create_t_head(&self,ctx:&mut Ctx)->OpResult<Value>{create_special(&self.base.base.base,ctx,"thead")}
    #[method(name = "deleteTHead", hint(js(ce_reactions)))] fn delete_t_head(&self,ctx:&mut Ctx)->OpResult<()>{delete_special(&self.base.base.base,ctx,"thead")}

    #[getter(name = "tFoot")] fn t_foot(&self,ctx:&mut Ctx)->OpResult<Value>{first(&self.base.base.base,ctx,"tfoot")}
    #[setter(name = "tFoot", hint(js(ce_reactions)))] fn set_t_foot(&self,ctx:&mut Ctx,value:Value)->OpResult<()>{set_special(&self.base.base.base,ctx,"tfoot",if matches!(value,Value::Null|Value::Undefined){None}else{Some(value)})}
    #[method(name = "createTFoot", hint(js(ce_reactions)))] fn create_t_foot(&self,ctx:&mut Ctx)->OpResult<Value>{create_special(&self.base.base.base,ctx,"tfoot")}
    #[method(name = "deleteTFoot", hint(js(ce_reactions)))] fn delete_t_foot(&self,ctx:&mut Ctx)->OpResult<()>{delete_special(&self.base.base.base,ctx,"tfoot")}

    #[getter(name="tBodies")] fn bodies(&self,ctx:&mut Ctx,this:This<Value>)->Value{collection(&self.base.base.base,ctx,this.0,"table:Bodies",CollectionKind::Bodies)}
    #[method(name="createTBody", hint(js(ce_reactions)))] fn create_body(&self,ctx:&mut Ctx)->OpResult<Value>{
        let node=&self.base.base.base;
        let before={let session=node.realm.session.borrow();let document=session.document();let count=tables::count(document,node.id,CollectionKind::Bodies).map_err(dom_error)?;match tables::item(document,node.id,CollectionKind::Bodies,count.saturating_sub(1)).map_err(dom_error)?{Some(body)=>document.next_sibling(body).map_err(dom_error)?,None=>None}};
        let value=create(node,ctx,"tbody")?;let before=node.realm.wrap_option(ctx,before);
        insert_dom_node(ctx,&node.realm,node.id,value.clone(),before)?;Ok(value)
    }

    #[getter(name = "align")]
    fn align(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("align")?.unwrap_or_default()) }
    #[setter(name = "align", coerce, hint(js(ce_reactions)))]
    fn set_align(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("align", value) }

    #[getter(name = "border")]
    fn border(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("border")?.unwrap_or_default()) }
    #[setter(name = "border", coerce, hint(js(ce_reactions)))]
    fn set_border(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("border", value) }

    #[getter(name = "frame")]
    fn frame(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("frame")?.unwrap_or_default()) }
    #[setter(name = "frame", coerce, hint(js(ce_reactions)))]
    fn set_frame(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("frame", value) }

    #[getter(name = "rules")]
    fn rules(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("rules")?.unwrap_or_default()) }
    #[setter(name = "rules", coerce, hint(js(ce_reactions)))]
    fn set_rules(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("rules", value) }

    #[getter(name = "summary")]
    fn summary(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("summary")?.unwrap_or_default()) }
    #[setter(name = "summary", coerce, hint(js(ce_reactions)))]
    fn set_summary(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("summary", value) }

    #[getter(name = "width")]
    fn width(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("width")?.unwrap_or_default()) }
    #[setter(name = "width", coerce, hint(js(ce_reactions)))]
    fn set_width(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("width", value) }

    #[getter(name = "bgColor")]
    fn bg_color(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("bgcolor")?.unwrap_or_default()) }
    #[setter(name = "bgColor", coerce, hint(js(ce_reactions)))]
    fn set_bg_color(&self, value: lumen_host::webidl::LegacyNullToEmptyString<'_>) -> OpResult<()> { self.base.base.base.set_attribute_core("bgcolor", &value.0) }

    #[getter(name = "cellPadding")]
    fn cell_padding(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("cellpadding")?.unwrap_or_default()) }
    #[setter(name = "cellPadding", coerce, hint(js(ce_reactions)))]
    fn set_cell_padding(&self, value: lumen_host::webidl::LegacyNullToEmptyString<'_>) -> OpResult<()> { self.base.base.base.set_attribute_core("cellpadding", &value.0) }

    #[getter(name = "cellSpacing")]
    fn cell_spacing(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("cellspacing")?.unwrap_or_default()) }
    #[setter(name = "cellSpacing", coerce, hint(js(ce_reactions)))]
    fn set_cell_spacing(&self, value: lumen_host::webidl::LegacyNullToEmptyString<'_>) -> OpResult<()> { self.base.base.base.set_attribute_core("cellspacing", &value.0) }
});
table_interface!(DomHtmlTableSectionElement,"HTMLTableSectionElement",{
    #[getter]
    fn rows(&self, ctx:&mut Ctx, this:This<Value>) -> Value { collection(&self.base.base.base,ctx,this.0,"table:SectionRows",CollectionKind::SectionRows) }
    #[method(name = "insertRow", coerce, hint(js(ce_reactions)))]
    fn insert_member(&self,ctx:&mut Ctx,#[default(-1)] index:i32)->OpResult<Value>{insert_member(&self.base.base.base,ctx,CollectionKind::SectionRows,index)}
    #[method(name = "deleteRow", coerce, hint(js(ce_reactions)))]
    fn delete_member(&self,ctx:&mut Ctx,index:i32)->OpResult<()>{delete_member(&self.base.base.base,ctx,CollectionKind::SectionRows,index)}

    #[getter(name = "align")]
    fn align(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("align")?.unwrap_or_default()) }
    #[setter(name = "align", coerce, hint(js(ce_reactions)))]
    fn set_align(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("align", value) }

    #[getter(name = "ch")]
    fn ch(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("char")?.unwrap_or_default()) }
    #[setter(name = "ch", coerce, hint(js(ce_reactions)))]
    fn set_ch(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("char", value) }

    #[getter(name = "chOff")]
    fn ch_off(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("charoff")?.unwrap_or_default()) }
    #[setter(name = "chOff", coerce, hint(js(ce_reactions)))]
    fn set_ch_off(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("charoff", value) }

    #[getter(name = "vAlign")]
    fn v_align(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("valign")?.unwrap_or_default()) }
    #[setter(name = "vAlign", coerce, hint(js(ce_reactions)))]
    fn set_v_align(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("valign", value) }
});
table_interface!(DomHtmlTableRowElement,"HTMLTableRowElement",{

    #[getter]
    fn cells(&self, ctx:&mut Ctx, this:This<Value>) -> Value { collection(&self.base.base.base,ctx,this.0,"table:Cells",CollectionKind::Cells) }
    #[method(name = "insertCell", coerce, hint(js(ce_reactions)))]
    fn insert_member(&self,ctx:&mut Ctx,#[default(-1)] index:i32)->OpResult<Value>{insert_member(&self.base.base.base,ctx,CollectionKind::Cells,index)}
    #[method(name = "deleteCell", coerce, hint(js(ce_reactions)))]
    fn delete_member(&self,ctx:&mut Ctx,index:i32)->OpResult<()>{delete_member(&self.base.base.base,ctx,CollectionKind::Cells,index)}

    #[getter(name="rowIndex")] fn row_index(&self)->OpResult<i32>{let node=&self.base.base.base;tables::row_index(node.realm.session.borrow().document(),node.id,false).map_err(dom_error)}
    #[getter(name="sectionRowIndex")] fn section_row_index(&self)->OpResult<i32>{let node=&self.base.base.base;tables::row_index(node.realm.session.borrow().document(),node.id,true).map_err(dom_error)}

    #[getter(name = "align")]
    fn align(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("align")?.unwrap_or_default()) }
    #[setter(name = "align", coerce, hint(js(ce_reactions)))]
    fn set_align(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("align", value) }

    #[getter(name = "ch")]
    fn ch(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("char")?.unwrap_or_default()) }
    #[setter(name = "ch", coerce, hint(js(ce_reactions)))]
    fn set_ch(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("char", value) }

    #[getter(name = "chOff")]
    fn ch_off(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("charoff")?.unwrap_or_default()) }
    #[setter(name = "chOff", coerce, hint(js(ce_reactions)))]
    fn set_ch_off(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("charoff", value) }

    #[getter(name = "vAlign")]
    fn v_align(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("valign")?.unwrap_or_default()) }
    #[setter(name = "vAlign", coerce, hint(js(ce_reactions)))]
    fn set_v_align(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("valign", value) }

    #[getter(name = "bgColor")]
    fn bg_color(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("bgcolor")?.unwrap_or_default()) }
    #[setter(name = "bgColor", coerce, hint(js(ce_reactions)))]
    fn set_bg_color(&self, value: lumen_host::webidl::LegacyNullToEmptyString<'_>) -> OpResult<()> { self.base.base.base.set_attribute_core("bgcolor", &value.0) }

});
table_interface!(DomHtmlTableCellElement,"HTMLTableCellElement",{
    #[getter(name = "align")]
    fn align(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("align")?.unwrap_or_default()) }
    #[setter(name = "align", coerce, hint(js(ce_reactions)))]
    fn set_align(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("align", value) }

    #[getter(name = "ch")]
    fn ch(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("char")?.unwrap_or_default()) }
    #[setter(name = "ch", coerce, hint(js(ce_reactions)))]
    fn set_ch(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("char", value) }

    #[getter(name = "chOff")]
    fn ch_off(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("charoff")?.unwrap_or_default()) }
    #[setter(name = "chOff", coerce, hint(js(ce_reactions)))]
    fn set_ch_off(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("charoff", value) }

    #[getter(name = "vAlign")]
    fn v_align(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("valign")?.unwrap_or_default()) }
    #[setter(name = "vAlign", coerce, hint(js(ce_reactions)))]
    fn set_v_align(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("valign", value) }

    #[getter(name = "bgColor")]
    fn bg_color(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("bgcolor")?.unwrap_or_default()) }
    #[setter(name = "bgColor", coerce, hint(js(ce_reactions)))]
    fn set_bg_color(&self, value: lumen_host::webidl::LegacyNullToEmptyString<'_>) -> OpResult<()> { self.base.base.base.set_attribute_core("bgcolor", &value.0) }

    #[getter(name = "axis")]
    fn axis(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("axis")?.unwrap_or_default()) }
    #[setter(name = "axis", coerce, hint(js(ce_reactions)))]
    fn set_axis(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("axis", value) }

    #[getter(name = "height")]
    fn height(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("height")?.unwrap_or_default()) }
    #[setter(name = "height", coerce, hint(js(ce_reactions)))]
    fn set_height(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("height", value) }

    #[getter(name = "width")]
    fn width(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("width")?.unwrap_or_default()) }
    #[setter(name = "width", coerce, hint(js(ce_reactions)))]
    fn set_width(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("width", value) }

    #[getter(name = "headers")]
    fn headers(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("headers")?.unwrap_or_default()) }
    #[setter(name = "headers", coerce, hint(js(ce_reactions)))]
    fn set_headers(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("headers", value) }

    #[getter(name = "abbr")]
    fn abbr(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("abbr")?.unwrap_or_default()) }
    #[setter(name = "abbr", coerce, hint(js(ce_reactions)))]
    fn set_abbr(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("abbr", value) }

    #[getter(name="cellIndex")] fn cell_index(&self)->OpResult<i32>{let node=&self.base.base.base;let session=node.realm.session.borrow();let document=session.document();match document.parent(node.id).map_err(dom_error)?{Some(parent) if lumen_html::forms::html_element_local_name(document,parent)==Some("tr")=>tables::index(document,parent,CollectionKind::Cells,node.id).map_err(dom_error),_=>Ok(-1)}}
    #[getter] fn scope(&self)->OpResult<String>{let value=self.base.base.base.get_null_attribute("scope")?.unwrap_or_default().to_ascii_lowercase();Ok(if matches!(value.as_str(),"row"|"col"|"rowgroup"|"colgroup"){value}else{String::new()})}
    #[setter(coerce,hint(js(ce_reactions)))] fn set_scope(&self,value:&str)->OpResult<()>{self.base.base.base.set_attribute_core("scope",value)}
    #[getter(name="noWrap")] fn no_wrap(&self)->OpResult<bool>{self.base.base.base.has_null_attribute("nowrap")}
    #[setter(name="noWrap",coerce,hint(js(ce_reactions)))] fn set_no_wrap(&self,value:bool)->OpResult<()>{if value { self.base.base.base.set_attribute_core("nowrap","") } else { self.base.base.base.remove_attribute_core("nowrap") }}

    #[getter(name="colSpan")] fn col_span(&self)->OpResult<u32>{let node=&self.base.base.base;tables::reflected_span(node.realm.session.borrow().document(),node.id,"colspan",1,1000).map_err(dom_error)}
    #[setter(name="colSpan",coerce,hint(js(ce_reactions)))] fn set_col_span(&self,value:u32)->OpResult<()>{self.base.base.base.set_attribute_core("colspan",&lumen_html::forms::reflected_unsigned_long_setter_value(value,1).to_string())}

    #[getter(name="rowSpan")] fn row_span(&self)->OpResult<u32>{let node=&self.base.base.base;tables::reflected_span(node.realm.session.borrow().document(),node.id,"rowspan",0,65534).map_err(dom_error)}
    #[setter(name="rowSpan",coerce,hint(js(ce_reactions)))] fn set_row_span(&self,value:u32)->OpResult<()>{self.base.base.base.set_attribute_core("rowspan",&lumen_html::forms::reflected_unsigned_long_setter_value(value,1).to_string())}
});
table_interface!(DomHtmlTableCaptionElement,"HTMLTableCaptionElement",{
    #[getter(name = "align")]
    fn align(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("align")?.unwrap_or_default()) }
    #[setter(name = "align", coerce, hint(js(ce_reactions)))]
    fn set_align(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("align", value) }
});
table_interface!(DomHtmlTableColElement,"HTMLTableColElement",{
    #[getter(name = "align")]
    fn align(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("align")?.unwrap_or_default()) }
    #[setter(name = "align", coerce, hint(js(ce_reactions)))]
    fn set_align(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("align", value) }

    #[getter(name = "ch")]
    fn ch(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("char")?.unwrap_or_default()) }
    #[setter(name = "ch", coerce, hint(js(ce_reactions)))]
    fn set_ch(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("char", value) }

    #[getter(name = "chOff")]
    fn ch_off(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("charoff")?.unwrap_or_default()) }
    #[setter(name = "chOff", coerce, hint(js(ce_reactions)))]
    fn set_ch_off(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("charoff", value) }

    #[getter(name = "vAlign")]
    fn v_align(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("valign")?.unwrap_or_default()) }
    #[setter(name = "vAlign", coerce, hint(js(ce_reactions)))]
    fn set_v_align(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("valign", value) }

    #[getter(name = "width")]
    fn width(&self) -> OpResult<String> { Ok(self.base.base.base.get_null_attribute("width")?.unwrap_or_default()) }
    #[setter(name = "width", coerce, hint(js(ce_reactions)))]
    fn set_width(&self, value: &str) -> OpResult<()> { self.base.base.base.set_attribute_core("width", value) }

    #[getter(name="span")] fn span(&self)->OpResult<u32>{let node=&self.base.base.base;tables::reflected_span(node.realm.session.borrow().document(),node.id,"span",1,1000).map_err(dom_error)}
    #[setter(name="span",coerce,hint(js(ce_reactions)))] fn set_span(&self,value:u32)->OpResult<()>{self.base.base.base.set_attribute_core("span",&lumen_html::forms::reflected_unsigned_long_setter_value(value,1).to_string())}
});

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn specification_window_table_reflection_raw_unsigned_values_reactions_and_adoption() {
        check(r#"(() => {
            const check=(condition,message)=>{if(!condition)throw new Error(message)};
            const destination=document.implementation.createHTMLDocument('destination');
            for(const [tag,property,attribute,minimum,maximum] of [
                ['col','span','span',1,1000],['colgroup','span','span',1,1000],
                ['td','colSpan','colspan',1,1000],['th','colSpan','colspan',1,1000],
                ['td','rowSpan','rowspan',0,65534],['th','rowSpan','rowspan',0,65534]
            ]) {
                const element=document.createElement(tag);
                check(element[property]===1,'missing default '+tag+property);
                for(const [input,raw,value] of [[0,'0',minimum],[maximum+1,String(maximum+1),maximum],
                    [2147483647,'2147483647',maximum],[2147483648,'1',1],[-1,'1',1],
                    [4294967296,'0',minimum],[1.9,'1',1],['2.9','2',2],[true,'1',1],
                    [null,'0',minimum],[undefined,'0',minimum],[NaN,'0',minimum],[Infinity,'0',minimum]]) {
                    element[property]=input;
                    check(element.getAttribute(attribute)===raw && element[property]===value,'raw setter/getter '+tag+property+' '+input);
                }
                element.setAttribute(attribute,'9'.repeat(400));check(element[property]===maximum,'unbounded valid positive content');
                element.setAttribute(attribute,'-1');check(element[property]===1,'invalid content default');
                element.removeAttribute(attribute);element.setAttributeNS('urn:foreign','foreign:'+attribute,'9');
                check(element[property]===1,'null namespace reflection');
                destination.adoptNode(element);element[property]=maximum+1;
                check(element.ownerDocument===destination && element.getAttribute(attribute)===String(maximum+1) && element[property]===maximum,'adopted reflection');
            }
            const log=[];
            customElements.define('x-reflected-span',class extends HTMLTableCellElement {
                static get observedAttributes(){return ['colspan','rowspan']}
                attributeChangedCallback(name,oldValue,newValue){log.push([name,oldValue,newValue,this[name==='colspan'?'colSpan':'rowSpan']])}
            },{extends:'td'});
            const cell=document.createElement('td',{is:'x-reflected-span'});
            cell.colSpan={valueOf(){log.push('conversion');return 1001}};log.push('after');
            check(JSON.stringify(log)==='["conversion",["colspan",null,"1001",1000],"after"]','Web IDL conversion precedes mutation and synchronous reactions');
            destination.adoptNode(cell);log.length=0;cell.rowSpan=65535;
            check(cell.ownerDocument===destination && JSON.stringify(log)==='[["rowspan",null,"65535",65534]]','adopted CE getter and reaction');
            let rejected=false;try{cell.colSpan=Symbol()}catch(error){rejected=error instanceof TypeError}
            check(rejected && cell.getAttribute('colspan')==='1001','failed conversion does not mutate');
            rejected=false;try{cell.rowSpan=1n}catch(error){rejected=error instanceof TypeError}
            check(rejected && cell.getAttribute('rowspan')==='65535','BigInt conversion failure preserves the attribute');
            return true;
        })()"#);
    }
    #[test]
    fn specification_window_table_cross_owner_caption_setter_releases_native_projection() {
        check(r#"(() => {
            const donor=document.implementation.createHTMLDocument('donor');
            const source=donor.createElement('table'),caption=source.createCaption();
            caption.id='caption';caption.appendChild(donor.createElement('b'));
            const table=document.createElement('table');document.body.appendChild(table);
            table.createCaption();table.caption=caption;
            if(table.caption!==caption||caption.ownerDocument!==document||caption.firstChild.ownerDocument!==document||source.caption!==null)throw Error('caption adoption lost identity or descendants');
            const row=source.insertRow(),rows=source.rows;document.adoptNode(source);
            if(source.rows!==rows||rows[0]!==row||row.ownerDocument!==document)throw Error('collection adoption lost actual brand');
            source.insertRow();if(rows.length!==2)throw Error('adopted collection stopped being live');
            return true;
        })()"#);
    }
    fn check(source: &str) {
        let mut engine = lumen::Engine::new();
        let _realm = crate::install(engine.ctx(), "<!doctype html><body></body>", 512).unwrap();
        let result = engine.eval_value(source).unwrap().unwrap_or_else(|exception| {
            let message = engine.ctx().member_get(&exception, "stack").ok()
                .and_then(|value| engine.ctx().coerce_string(&value).ok()).map(|value| value.to_string()).unwrap_or_default();
            panic!("table guard threw: {message}");
        });
        assert!(matches!(result, Value::Bool(true)), "table guard did not return true");
    }

    #[test]
    fn specification_window_table_live_collection_retains_actual_detached_owner_through_gc() {
        let mut engine = lumen::Engine::new();
        let _realm = crate::install(engine.ctx(), "<!doctype html><body></body>", 128).unwrap();
        let row = engine.eval_value(r#"(() => {
            const other=document.implementation.createHTMLDocument('retained');
            const table=other.createElement('table');
            const row=table.insertRow();row.id='retained-row';
            globalThis.savedTableRows=table.rows;
            return row;
        })()"#).unwrap().ok().expect("create actual table member");
        let weak_row=engine.ctx().weak_value(&row).expect("table row object");
        drop(row);
        engine.collect_garbage();
        let row=weak_row.upgrade().expect("live collection retains actual member identity");
        let global=engine.ctx().global_object();
        engine.ctx().member_set(&global,"savedRowIdentity",row).ok().expect("publish retained identity comparison");
        let result=engine.eval_value(r#"(() => {
            const rows=savedTableRows,row=rows[0];
            if(row!==savedRowIdentity||rows.namedItem('retained-row')!==row)throw new Error('collection member identity lost');
            const table=row.parentNode.parentNode;
            const next=table.insertRow();
            if(rows.length!==2||rows[1]!==next||next.ownerDocument!==row.ownerDocument)throw new Error('retained detached owner lost');
            return true;
        })()"#).unwrap();
        assert!(matches!(result, Ok(Value::Bool(true))), "collection lost owner or native identity after GC");
    }

    #[test]
    fn specification_window_table_membership_order_mutations_and_typed_setter_validation() {
        check(r#"(() => {
            const check=(condition,message)=>{if(!condition)throw new Error(message);};
            const t=document.createElement('table');document.body.appendChild(t);
            const rows=t.rows,bodies=t.tBodies;
            check(rows===t.rows&&bodies===t.tBodies,'live SameObject collections');
            const foot=t.createTFoot(),fr=foot.insertRow();fr.id='footer';
            const body=t.createTBody(),br=body.insertRow();br.id='body';
            const direct=document.createElement('tr');direct.id='direct';t.appendChild(direct);
            const head=t.createTHead(),hr=head.insertRow();hr.id='header';
            check(Array.from(rows).map(r=>r.id).join(',')==='header,body,direct,footer','normative header/body/footer order');
            check(rows.namedItem('footer')===fr&&fr.rowIndex===3&&fr.sectionRowIndex===0,'indices and named access');
            const appended=t.insertRow(-1);check(appended.parentNode===foot&&appended.rowIndex===4,'append uses last ordered row parent');
            const inserted=t.insertRow(1);check(inserted.parentNode===body&&body.rows[0]===inserted,'insertion follows indexed parent');
            let threw=false;try{t.tHead=body;}catch(e){threw=e.name==='HierarchyRequestError';}
            check(threw&&t.tHead===head&&head.parentNode===t,'validate before deleting head');
            threw=false;try{t.caption=body;}catch(e){threw=e instanceof TypeError;}
            check(threw&&t.caption===null,'nominal caption conversion');
            t.insertBefore(document.createTextNode('prefix'),t.firstChild);
            const caption=t.createCaption();check(t.firstChild===caption,'caption before first node');
            t.caption=caption;check(t.firstChild===caption,'same caption reinserted');
            t.deleteTHead();check(hr.rowIndex===-1&&hr.sectionRowIndex===0&&rows.length===5,'live collection after section removal');
            const foreign=document.createElementNS('urn:other','tr');body.appendChild(foreign);
            check(body.rows.length===2,'namespace filtering');
            const empty=document.createElement('table');empty.deleteRow(-1);
            const first=empty.insertRow();check(first.parentNode===empty.tBodies[0]&&empty.tBodies.length===1,'empty table body creation');
            let bad=false;try{empty.insertRow(2);}catch(e){bad=e.name==='IndexSizeError';}
            check(bad&&empty.rows.length===1,'index admission before mutation');
            return true;
        })()"#);
    }

    #[test]
    fn specification_window_table_indices_use_webidl_long_defaults_and_conversion() {
        check(r#"(() => {
            const check=(ok,message)=>{if(!ok)throw Error(message);};
            for(const name of ['table','thead','tbody','tfoot','tr']) {
                const owner=document.createElement(name), cells=name==='tr';
                const insert=cells?'insertCell':'insertRow', remove=cells?'deleteCell':'deleteRow';
                const members=cells?owner.cells:owner.rows;
                check(owner[insert].length===0&&owner[remove].length===1,'IDL operation lengths');
                const first=owner[insert](), last=owner[insert](undefined);
                check(members[0]===first&&members[1]===last,'optional undefined uses minus-one default');
                const front=owner[insert](null);
                check(members[0]===front,'non-nullable long converts null to zero');
                let calls=0;
                const middle=owner[insert]({valueOf(){calls++;return '4294967297.9';}});
                check(calls===1&&members[1]===middle,'ToInt32 truncates and wraps once before mutation');
                owner[remove]('1.9');
                check(members.length===3&&members[1]===first,'string index conversion');
                owner[remove](undefined);
                check(members.length===2&&members[0]===first,'required undefined converts to zero');
                for(const value of [1n,Symbol('index')]) {
                    let failed=false;try{owner[insert](value);}catch(e){failed=e instanceof TypeError;}
                    check(failed&&members.length===2,'conversion failure precedes insertion');
                }
                let failed=false;try{owner[remove]();}catch(e){failed=e instanceof TypeError;}
                check(failed&&members.length===2,'required delete index stays required');
                owner[remove](Infinity);
                check(members.length===1&&members[0]===last,'nonfinite long becomes zero');
            }
            return true;
        })()"#);
    }

    #[test]
    fn specification_window_table_cells_ranges_legacy_reflection_and_owner_document() {
        check(r#"(() => {
            const check=(condition,message)=>{if(!condition)throw new Error(message);};
            const inert=document.implementation.createHTMLDocument('other');
            const t=inert.createElement('table'),r=t.insertRow(),cells=r.cells;
            const a=r.insertCell(),b=r.insertCell(0);
            check(a.ownerDocument===inert&&b.ownerDocument===inert&&cells===r.cells,'factory associated document and live identity');
            check(b.cellIndex===0&&a.cellIndex===1&&r.sectionRowIndex===0,'cell and section indices');
            a.colSpan=0;check(a.colSpan===1&&a.getAttribute('colspan')==='0','raw zero setter and clamped getter');
            a.colSpan=-1;check(a.colSpan===1&&a.getAttribute('colspan')==='1','unsigned conversion then setter default');
            a.rowSpan=0;check(a.rowSpan===0,'rowspan zero sentinel');
            a.setAttribute('rowspan','  +65535suffix');check(a.rowSpan===65534,'prefix parse and maximum clamp');
            a.setAttribute('colspan','9'.repeat(400));check(a.colSpan===1000,'overflow remains valid positive integer');
            a.setAttribute('colspan','-2');check(a.colSpan===1,'negative default');
            a.scope='ROWGROUP';check(a.scope==='rowgroup','scope known-value reflection');
            a.scope='invalid';check(a.scope==='','scope invalid-value default');
            a.bgColor=null;check(a.getAttribute('bgcolor')==='','legacy null-to-empty');
            t.cellPadding=undefined;check(t.getAttribute('cellpadding')==='undefined','legacy undefined string');
            a.noWrap=true;check(a.hasAttribute('nowrap'),'boolean presence');a.noWrap=false;
            check(!a.hasAttribute('nowrap'),'boolean removal');
            r.deleteCell(-1);check(cells.length===1&&a.cellIndex===-1,'deleted cell live collection');
            r.deleteCell(-1);r.deleteCell(-1);check(cells.length===0,'empty minus-one deletion');
            const other=document.implementation.createHTMLDocument('destination');
            other.adoptNode(t);other.body.appendChild(t);
            const newCell=r.insertCell();
            check(cells.length===1&&cells[0]===newCell&&newCell.ownerDocument===other,'live collection follows actual adoption');
            return true;
        })()"#);
    }
}
