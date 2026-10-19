//! HTML event-handler content attributes backed by the native event-target list.
use super::*;
use lumen::embed::{JsFunction, Value};

pub(crate) enum Compilation {
    Inactive,
    Failed,
    Compiled(JsFunction),
}

pub(crate) fn node_handler_get<T:lumen_bind::Class>(ctx:&mut Ctx,receiver:&Value,event:&str,lenient:bool)->OpResult<Value> {
    if let Err(error)=ctx.with_instance::<T,_>(receiver,|_|()) {
        return if lenient {Ok(Value::Undefined)} else {Err(error)};
    }
    let (realm,node)=ctx.with_instance::<DomNode,_>(receiver,|node|node.realm.resolve_adopted_node(node.id))?;
    element_handler_value(ctx,&realm,node,receiver,event)
}

pub(crate) fn node_handler_set<T:lumen_bind::Class>(ctx:&mut Ctx,receiver:&Value,event:&str,callback:events::EventHandler,lenient:bool)->OpResult<()> {
    if let Err(error)=ctx.with_instance::<T,_>(receiver,|_|()) {
        return if lenient {Ok(())} else {Err(error)};
    }
    let (realm,node)=ctx.with_instance::<DomNode,_>(receiver,|node|node.realm.resolve_adopted_node(node.id))?;
    set_element_handler(ctx,&realm,node,receiver,event,callback);
    Ok(())
}

fn active_handler_document(realm:&DomRealm,node:NodeId)->bool {
    if !realm.has_browsing_context || realm.lifecycle.destroyed.get() {return false;}
    let session=realm.session.borrow();let document=session.document();
    if document.node_document(node).ok()!=Some(document.root()) {return false;}
    drop(session);
    realm.browsing_context().is_none_or(|context|context.document().is_none()
        || browsing_context::is_active_document(&context,realm))
}

fn handler_bit(event_type:&str)->Option<u128> {
    HANDLERS.iter().position(|(_,event)|*event==event_type).map(|index|1u128<<index)
}

// Null is still an initialized HTML handler-map value. Keep this sparse
// source-initialization state independently of active listeners and wrappers.
pub(crate) fn mark_idl_handler(realm:&DomRealm,node:NodeId,event_type:&str) {
    // A parser constructor may set an IDL handler before its token attributes
    // exist. Those later attribute additions still activate their own source.
    let session=realm.session.borrow();let document=session.document();
    let present=matches!(document.kind(node),Ok(NodeKind::Element{attributes,..}) if
        attributes.iter().enumerate().any(|(index,(name,_))|document.attribute_namespace_uri_at(node,index).is_none()
            && handler_attribute(name.as_str(),realm.is_html_document).is_some_and(|(_,event)|event==event_type)));
    if !present{return;}
    mark_content_handler(realm,node,event_type);
}

fn mark_content_handler(realm:&DomRealm,node:NodeId,event_type:&str) {
    if let Some(bit)=handler_bit(event_type) {
        *realm.initialized_content_handlers.borrow_mut().entry(node).or_default()|=bit;
    }
}

pub(crate) fn compile(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    source: &events::RawContentHandler,
) -> Compilation {
    if !realm.has_browsing_context || realm.lifecycle.sandboxed_automatic_features.get() {
        return Compilation::Inactive;
    }
    if !source.window_target {
        let session=realm.session.borrow();let document=session.document();
        if document.node_document(source.node).ok()!=Some(document.root()) {
            return Compilation::Inactive;
        }
    }
    let Some(handle)=realm.relevant_host_realm(ctx) else {return Compilation::Inactive;};
    ctx.with_host_realm(&handle,|ctx|compile_in_realm(ctx,realm,source))
        .unwrap_or(Compilation::Inactive)
}

fn compile_in_realm(ctx:&mut Ctx,realm:&Rc<DomRealm>,source:&events::RawContentHandler)->Compilation {
    let scopes = if source.window_target {
        Vec::new()
    } else {
        let document = realm.document_value(ctx);
        let form_owner = {
            let session = realm.session.borrow();
            lumen_html::forms::form_owner(session.document(), source.node)
        };
        let mut scopes = vec![document];
        if let Some(form) = form_owner {
            scopes.push(realm.wrap(ctx, form));
        }
        scopes.push(realm.wrap(ctx, source.node));
        scopes
    };

    match ctx.compile_event_handler(
        &source.name,
        &source.body,
        &scopes,
        source.window_target && source.name == "onerror",
    ) {
        Ok(function) => match JsFunction::from_value(function) {
            Some(function) => Compilation::Compiled(function),
            None => Compilation::Failed,
        },
        Err(exception) => {
            DomRealm::report_exception(ctx, lumen::embed::abrupt_value(exception));
            Compilation::Failed
        }
    }
}

pub(crate) fn attribute_changed(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    namespace_uri: Option<&str>,
    qualified_name: &str,
    value: Option<&str>,
) -> OpResult<()> {
    if namespace_uri.is_some() {
        return Ok(());
    }

    let event = handler_attribute(qualified_name, realm.is_html_document);
    let Some((attribute_name, event_type)) = event else {
        return Ok(());
    };
    let window_target = is_window_reflecting_body_attribute(realm, node, attribute_name);
    if WINDOW_HANDLERS.contains(&attribute_name) && !window_target { return Ok(()); }
    if !matches!(
        realm.session.borrow().document().kind(node),
        Ok(NodeKind::Element { .. })
    ) {
        return Ok(());
    }
    mark_content_handler(realm,node,event_type);
    if window_target && !active_handler_document(realm,node) {return Ok(());}
    if let Some(body)=value {
        if !realm.prepare_handler_csp(ctx,node,body)? {return Ok(());}
    }

    let (target, owner) = if window_target {
        let Some(data) = realm.window_target.borrow().as_ref().cloned() else {
            return Ok(());
        };
        let Some(owner) = realm
            .window_wrapper
            .borrow()
            .as_ref()
            .and_then(WeakValue::upgrade)
        else {
            return Ok(());
        };
        (events::DomEventTarget::from_data(data), owner)
    } else {
        (
            events::DomEventTarget::node(realm, node),
            realm.wrap(ctx, node),
        )
    };
    let handler = value.map(|body| events::RawContentHandler {
        node,
        name: attribute_name.to_owned(),
        body: body.to_owned(),
        window_target,
        location: realm.base_url(),
    });
    target.set_content_handler(ctx, &owner, event_type, handler);
    Ok(())
}

pub(crate) fn window_handler_value(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    event_type: &str,
) -> OpResult<Value> {
    if realm.lifecycle.destroyed.get() { return Ok(Value::Null); }
    let Some(data) = realm.window_target.borrow().as_ref().cloned() else {
        return Ok(Value::Null);
    };
    let Some(owner) = realm
        .window_wrapper
        .borrow()
        .as_ref()
        .and_then(WeakValue::upgrade)
    else {
        return Ok(Value::Null);
    };
    events::DomEventTarget::from_data(data).handler_value(ctx, &owner, event_type)
}

pub(crate) fn set_window_handler(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    event_type: &str,
    callback: crate::events::EventHandler,
) {
    if realm.lifecycle.destroyed.get() { return; }
    let Some(data) = realm.window_target.borrow().as_ref().cloned() else {
        return;
    };
    let Some(owner) = realm
        .window_wrapper
        .borrow()
        .as_ref()
        .and_then(WeakValue::upgrade)
    else {
        return;
    };
    events::DomEventTarget::from_data(data).set_event_handler(ctx, &owner, event_type, callback);
}

pub(crate) fn element_handler_value(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    owner: &Value,
    event_type: &str,
) -> OpResult<Value> {
    let attribute=HANDLERS.iter().find(|(_,event)|*event==event_type).map_or("",|(name,_)|*name);
    if !attribute.is_empty() && is_window_reflecting_body_attribute(realm, node, attribute) {
        if active_handler_document(realm,node) {window_handler_value(ctx, realm, event_type)} else {Ok(Value::Null)}
    } else {
        events::DomEventTarget::node(realm, node).handler_value(ctx, owner, event_type)
    }
}

pub(crate) fn set_element_handler(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    node: NodeId,
    owner: &Value,
    event_type: &str,
    callback: crate::events::EventHandler,
) {
    let attribute=HANDLERS.iter().find(|(_,event)|*event==event_type).map_or("",|(name,_)|*name);
    if !attribute.is_empty() && is_window_reflecting_body_attribute(realm, node, attribute) {
        if active_handler_document(realm,node) {set_window_handler(ctx, realm, event_type, callback);}
    } else {
        events::DomEventTarget::node(realm, node).set_event_handler(ctx, owner, event_type, callback);
    }
}

pub(crate) fn initialize_document(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    let root = realm.session.borrow().document().root();
    initialize_subtree(ctx, realm, root)
}

pub(crate) fn queue_inserted_handlers(document: &lumen_html::Document,
    mutation: &lumen_html::observe::ObservedMutation, html: bool, pending: &RefCell<Vec<NodeId>>) {
    for node in mutation.kind.added_nodes() {
        let Ok(NodeKind::Element { attributes, .. }) = document.kind(node) else { continue; };
        if attributes.iter().enumerate().any(|(index, (name, _))|
            document.attribute_namespace_uri_at(node, index).is_none()
                && handler_attribute(name.as_str(), html).is_some()) {
            let mut pending = pending.borrow_mut();
            if !pending.contains(&node) { pending.push(node); }
        }
    }
}

pub(crate) fn initialize_subtree(
    ctx: &mut Ctx,
    realm: &Rc<DomRealm>,
    root: NodeId,
) -> OpResult<()> {
    let graph = template_graph::ArenaGraph::new(realm)?;
    let nodes = lumen_html::graph::identity_nodes(&graph, root).map_err(dom_error)?;
    let handlers = {
        let mut handlers = Vec::new();
        for node in nodes {
            let owner = graph.owner(node)?;
            let session = owner.session.borrow();
            let document = session.document();
            if let NodeKind::Element { attributes, .. } = document.kind(node).map_err(dom_error)? {
                for (index, (name, value)) in attributes.iter().enumerate() {
                    if document.attribute_namespace_uri_at(node, index).is_none()
                        && handler_attribute(name.as_str(), owner.is_html_document).is_some()
                    {
                        handlers.push((owner.clone(), node, name.to_string(), value.clone()));
                    }
                }
            }
        }
        handlers
    };

    for (realm, node, name, body) in handlers {
        let Some((_, event_type)) = handler_attribute(&name, realm.is_html_document) else {
            continue;
        };
        let initialized=handler_bit(event_type).is_some_and(|bit|
            realm.initialized_content_handlers.borrow().get(&node).is_some_and(|state|*state&bit!=0));
        if !initialized {
            attribute_changed(ctx, &realm, node, None, &name, Some(&body))?;
        }
    }
    Ok(())
}

// HTML §8.1.8.2: one typed handler registry drives IDL members, content
// applicability and sparse initialization. Existing UI/Pointer/Touch extensions
// share the same handler semantics without a separate binding/name table.
macro_rules! handler_bindings {
($d:tt; strict[$(($getter:ident,$setter:ident,$idl:literal,$event:literal)),* $(,)?]; lenient[$(($lg:ident,$ls:ident,$li:literal,$le:literal)),* $(,)?]; window[$(($wg:ident,$ws:ident,$wi:literal,$we:literal)),* $(,)?])=>{
const HANDLERS:&[(&str,&str)]=&[$(($idl,$event)),*,$(($li,$le)),*,$(($wi,$we)),*];
const WINDOW_HANDLERS:&[&str]=&[$($wi),*];
pub(crate) const WINDOW_IDL_HANDLERS:&[&str]=&[$($idl),*,$($li),*,$($wi),*];
macro_rules! bind_node_handlers {($d ty:ident {$d($d body:tt)*})=>{#[lumen_bind::methods] impl $d ty {$(#[getter(name=$idl)] fn $getter(&self,ctx:&mut lumen::embed::Ctx,this:lumen_bind::This<lumen::embed::Value>)->lumen::embed::OpResult<lumen::embed::Value> {crate::event_content_handlers::node_handler_get::<$d ty>(ctx,&this.0,$event,false)}
#[setter(name=$idl)] fn $setter(&self,ctx:&mut lumen::embed::Ctx,this:lumen_bind::This<lumen::embed::Value>,callback:crate::events::EventHandler)->lumen::embed::OpResult<()> {crate::event_content_handlers::node_handler_set::<$d ty>(ctx,&this.0,$event,callback,false)})*
$(#[getter(name=$li)] fn $lg(ctx:&mut lumen::embed::Ctx,this:lumen_bind::This<lumen::embed::Value>)->lumen::embed::OpResult<lumen::embed::Value> {crate::event_content_handlers::node_handler_get::<$d ty>(ctx,&this.0,$le,true)}
#[setter(name=$li)] fn $ls(ctx:&mut lumen::embed::Ctx,this:lumen_bind::This<lumen::embed::Value>,callback:crate::events::EventHandler)->lumen::embed::OpResult<()> {crate::event_content_handlers::node_handler_set::<$d ty>(ctx,&this.0,$le,callback,true)})*
$d($d body)*}}}
pub(crate) use bind_node_handlers;
macro_rules! bind_namespace_handlers {($d ty:ident {$d($d body:tt)*})=>{crate::focus::bind_focus_mixin! {$d ty {$(#[getter(name=$idl)] fn $getter(&self,ctx:&mut lumen::embed::Ctx,this:lumen_bind::This<lumen::embed::Value>)->lumen::embed::OpResult<lumen::embed::Value> {crate::event_content_handlers::node_handler_get::<$d ty>(ctx,&this.0,$event,false)}
#[setter(name=$idl)] fn $setter(&self,ctx:&mut lumen::embed::Ctx,this:lumen_bind::This<lumen::embed::Value>,callback:crate::events::EventHandler)->lumen::embed::OpResult<()> {crate::event_content_handlers::node_handler_set::<$d ty>(ctx,&this.0,$event,callback,false)})*
$(#[getter(name=$li)] fn $lg(ctx:&mut lumen::embed::Ctx,this:lumen_bind::This<lumen::embed::Value>)->lumen::embed::OpResult<lumen::embed::Value> {crate::event_content_handlers::node_handler_get::<$d ty>(ctx,&this.0,$le,true)}
#[setter(name=$li)] fn $ls(ctx:&mut lumen::embed::Ctx,this:lumen_bind::This<lumen::embed::Value>,callback:crate::events::EventHandler)->lumen::embed::OpResult<()> {crate::event_content_handlers::node_handler_set::<$d ty>(ctx,&this.0,$le,callback,true)})*
$d($d body)*}}}}
pub(crate) use bind_namespace_handlers;
macro_rules! bind_body_handlers {($d ty:ident {$d($d body:tt)*})=>{#[lumen_bind::methods] impl $d ty {$(#[getter(name=$wi)] fn $wg(&self,ctx:&mut lumen::embed::Ctx,this:lumen_bind::This<lumen::embed::Value>)->lumen::embed::OpResult<lumen::embed::Value> {crate::event_content_handlers::node_handler_get::<$d ty>(ctx,&this.0,$we,false)}
#[setter(name=$wi)] fn $ws(&self,ctx:&mut lumen::embed::Ctx,this:lumen_bind::This<lumen::embed::Value>,callback:crate::events::EventHandler)->lumen::embed::OpResult<()> {crate::event_content_handlers::node_handler_set::<$d ty>(ctx,&this.0,$we,callback,false)})*
$d($d body)*}}}
pub(crate) use bind_body_handlers;
macro_rules! bind_declared_html_handlers {
    ($d ty:ident [window_handlers] {$d($d body:tt)*})=>{crate::event_content_handlers::bind_body_handlers! {$d ty {$d($d body)*}}};
    ($d ty:ident [] {$d($d body:tt)*})=>{#[lumen_bind::methods] impl $d ty {$d($d body)*}};
}
pub(crate) use bind_declared_html_handlers;
macro_rules! bind_window_handlers {($d($d body:tt)*)=>{#[lumen_bind::methods] impl DomWindow {
$(#[getter(name=$idl)] fn $getter(&self,ctx:&mut lumen::embed::Ctx,this:lumen_bind::This<lumen::embed::Value>)->lumen::embed::OpResult<lumen::embed::Value> {crate::window_globals::handler_get(ctx,&this.0,$event,false)}
#[setter(name=$idl)] fn $setter(&self,ctx:&mut lumen::embed::Ctx,this:lumen_bind::This<lumen::embed::Value>,callback:crate::events::EventHandler)->lumen::embed::OpResult<()> {crate::window_globals::handler_set(ctx,&this.0,$event,callback,false)})*
$(#[getter(name=$li)] fn $lg(ctx:&mut lumen::embed::Ctx,this:lumen_bind::This<lumen::embed::Value>)->lumen::embed::OpResult<lumen::embed::Value> {crate::window_globals::handler_get(ctx,&this.0,$le,true)}
#[setter(name=$li)] fn $ls(ctx:&mut lumen::embed::Ctx,this:lumen_bind::This<lumen::embed::Value>,callback:crate::events::EventHandler)->lumen::embed::OpResult<()> {crate::window_globals::handler_set(ctx,&this.0,$le,callback,true)})*
$(#[getter(name=$wi)] fn $wg(&self,ctx:&mut lumen::embed::Ctx,this:lumen_bind::This<lumen::embed::Value>)->lumen::embed::OpResult<lumen::embed::Value> {crate::window_globals::handler_get(ctx,&this.0,$we,false)}
#[setter(name=$wi)] fn $ws(&self,ctx:&mut lumen::embed::Ctx,this:lumen_bind::This<lumen::embed::Value>,callback:crate::events::EventHandler)->lumen::embed::OpResult<()> {crate::window_globals::handler_set(ctx,&this.0,$we,callback,false)})*
$d($d body)*}}}
pub(crate) use bind_window_handlers;
};
}
handler_bindings! {$; strict[
(onabort,set_onabort,"onabort","abort"),
(onauxclick,set_onauxclick,"onauxclick","auxclick"),
(onbeforeinput,set_onbeforeinput,"onbeforeinput","beforeinput"),
(onbeforematch,set_onbeforematch,"onbeforematch","beforematch"),
(onbeforetoggle,set_onbeforetoggle,"onbeforetoggle","beforetoggle"),
(onblur,set_onblur,"onblur","blur"),
(oncancel,set_oncancel,"oncancel","cancel"),
(oncanplay,set_oncanplay,"oncanplay","canplay"),
(oncanplaythrough,set_oncanplaythrough,"oncanplaythrough","canplaythrough"),
(onchange,set_onchange,"onchange","change"),
(onclick,set_onclick,"onclick","click"),
(onclose,set_onclose,"onclose","close"),
(oncommand,set_oncommand,"oncommand","command"),
(oncontextlost,set_oncontextlost,"oncontextlost","contextlost"),
(oncontextmenu,set_oncontextmenu,"oncontextmenu","contextmenu"),
(oncontextrestored,set_oncontextrestored,"oncontextrestored","contextrestored"),
(oncopy,set_oncopy,"oncopy","copy"),
(oncuechange,set_oncuechange,"oncuechange","cuechange"),
(oncut,set_oncut,"oncut","cut"),
(ondblclick,set_ondblclick,"ondblclick","dblclick"),
(ondrag,set_ondrag,"ondrag","drag"),
(ondragend,set_ondragend,"ondragend","dragend"),
(ondragenter,set_ondragenter,"ondragenter","dragenter"),
(ondragleave,set_ondragleave,"ondragleave","dragleave"),
(ondragover,set_ondragover,"ondragover","dragover"),
(ondragstart,set_ondragstart,"ondragstart","dragstart"),
(ondrop,set_ondrop,"ondrop","drop"),
(ondurationchange,set_ondurationchange,"ondurationchange","durationchange"),
(onemptied,set_onemptied,"onemptied","emptied"),
(onended,set_onended,"onended","ended"),
(onerror,set_onerror,"onerror","error"),
(onfocus,set_onfocus,"onfocus","focus"),
(onformdata,set_onformdata,"onformdata","formdata"),
(oninput,set_oninput,"oninput","input"),
(oninvalid,set_oninvalid,"oninvalid","invalid"),
(onkeydown,set_onkeydown,"onkeydown","keydown"),
(onkeypress,set_onkeypress,"onkeypress","keypress"),
(onkeyup,set_onkeyup,"onkeyup","keyup"),
(onload,set_onload,"onload","load"),
(onloadeddata,set_onloadeddata,"onloadeddata","loadeddata"),
(onloadedmetadata,set_onloadedmetadata,"onloadedmetadata","loadedmetadata"),
(onloadstart,set_onloadstart,"onloadstart","loadstart"),
(onmousedown,set_onmousedown,"onmousedown","mousedown"),
(onmousemove,set_onmousemove,"onmousemove","mousemove"),
(onmouseout,set_onmouseout,"onmouseout","mouseout"),
(onmouseover,set_onmouseover,"onmouseover","mouseover"),
(onmouseup,set_onmouseup,"onmouseup","mouseup"),
(onpaste,set_onpaste,"onpaste","paste"),
(onpause,set_onpause,"onpause","pause"),
(onplay,set_onplay,"onplay","play"),
(onplaying,set_onplaying,"onplaying","playing"),
(onprogress,set_onprogress,"onprogress","progress"),
(onratechange,set_onratechange,"onratechange","ratechange"),
(onreset,set_onreset,"onreset","reset"),
(onresize,set_onresize,"onresize","resize"),
(onscroll,set_onscroll,"onscroll","scroll"),
(onscrollend,set_onscrollend,"onscrollend","scrollend"),
(onsecuritypolicyviolation,set_onsecuritypolicyviolation,"onsecuritypolicyviolation","securitypolicyviolation"),
(onseeked,set_onseeked,"onseeked","seeked"),
(onseeking,set_onseeking,"onseeking","seeking"),
(onselect,set_onselect,"onselect","select"),
(onslotchange,set_onslotchange,"onslotchange","slotchange"),
(onstalled,set_onstalled,"onstalled","stalled"),
(onsubmit,set_onsubmit,"onsubmit","submit"),
(onsuspend,set_onsuspend,"onsuspend","suspend"),
(ontimeupdate,set_ontimeupdate,"ontimeupdate","timeupdate"),
(ontoggle,set_ontoggle,"ontoggle","toggle"),
(onvolumechange,set_onvolumechange,"onvolumechange","volumechange"),
(onwaiting,set_onwaiting,"onwaiting","waiting"),
(onanimationstart,set_onanimationstart,"onanimationstart","animationstart"),
(onanimationiteration,set_onanimationiteration,"onanimationiteration","animationiteration"),
(onanimationend,set_onanimationend,"onanimationend","animationend"),
(onanimationcancel,set_onanimationcancel,"onanimationcancel","animationcancel"),
(ontransitionrun,set_ontransitionrun,"ontransitionrun","transitionrun"),
(ontransitionstart,set_ontransitionstart,"ontransitionstart","transitionstart"),
(ontransitionend,set_ontransitionend,"ontransitionend","transitionend"),
(ontransitioncancel,set_ontransitioncancel,"ontransitioncancel","transitioncancel"),
(onwebkitanimationend,set_onwebkitanimationend,"onwebkitanimationend","webkitAnimationEnd"),
(onwebkitanimationiteration,set_onwebkitanimationiteration,"onwebkitanimationiteration","webkitAnimationIteration"),
(onwebkitanimationstart,set_onwebkitanimationstart,"onwebkitanimationstart","webkitAnimationStart"),
(onwebkittransitionend,set_onwebkittransitionend,"onwebkittransitionend","webkitTransitionEnd"),
(onwheel,set_onwheel,"onwheel","wheel"),
(onfocusin,set_onfocusin,"onfocusin","focusin"),
(onfocusout,set_onfocusout,"onfocusout","focusout"),
(onpointerdown,set_onpointerdown,"onpointerdown","pointerdown"),
(onpointerup,set_onpointerup,"onpointerup","pointerup"),
(onpointermove,set_onpointermove,"onpointermove","pointermove"),
(onpointercancel,set_onpointercancel,"onpointercancel","pointercancel"),
(oncompositionstart,set_oncompositionstart,"oncompositionstart","compositionstart"),
(oncompositionupdate,set_oncompositionupdate,"oncompositionupdate","compositionupdate"),
(oncompositionend,set_oncompositionend,"oncompositionend","compositionend"),
(ontouchstart,set_ontouchstart,"ontouchstart","touchstart"),
(ontouchend,set_ontouchend,"ontouchend","touchend"),
(ontouchmove,set_ontouchmove,"ontouchmove","touchmove"),
(ontouchcancel,set_ontouchcancel,"ontouchcancel","touchcancel")
];lenient[
(onmouseenter,set_onmouseenter,"onmouseenter","mouseenter"),
(onmouseleave,set_onmouseleave,"onmouseleave","mouseleave")
];window[
(onafterprint,set_onafterprint,"onafterprint","afterprint"),
(onbeforeprint,set_onbeforeprint,"onbeforeprint","beforeprint"),
(onbeforeunload,set_onbeforeunload,"onbeforeunload","beforeunload"),
(onhashchange,set_onhashchange,"onhashchange","hashchange"),
(onlanguagechange,set_onlanguagechange,"onlanguagechange","languagechange"),
(onmessage,set_onmessage,"onmessage","message"),
(onmessageerror,set_onmessageerror,"onmessageerror","messageerror"),
(onoffline,set_onoffline,"onoffline","offline"),
(ononline,set_ononline,"ononline","online"),
(onpagehide,set_onpagehide,"onpagehide","pagehide"),
(onpagereveal,set_onpagereveal,"onpagereveal","pagereveal"),
(onpageshow,set_onpageshow,"onpageshow","pageshow"),
(onpageswap,set_onpageswap,"onpageswap","pageswap"),
(onpopstate,set_onpopstate,"onpopstate","popstate"),
(onrejectionhandled,set_onrejectionhandled,"onrejectionhandled","rejectionhandled"),
(onstorage,set_onstorage,"onstorage","storage"),
(onunhandledrejection,set_onunhandledrejection,"onunhandledrejection","unhandledrejection"),
(onunload,set_onunload,"onunload","unload")
]}
const _:()=assert!(HANDLERS.len()<=u128::BITS as usize);

fn handler_attribute(name: &str, html_document: bool) -> Option<(&'static str, &'static str)> {
    HANDLERS.iter().copied().find(|(attribute, _)| {
        if html_document {
            name.eq_ignore_ascii_case(attribute)
        } else {
            name == *attribute
        }
    })
}

fn is_window_reflecting_body_attribute(realm: &DomRealm, node: NodeId, attribute: &str) -> bool {
    if !WINDOW_HANDLERS.contains(&attribute) && !matches!(attribute, "onblur" | "onerror" | "onfocus" | "onload" | "onresize" | "onscroll") {
        return false;
    }
    let document = realm.session.borrow();
    matches!(
        document.document().kind(node),
        Ok(NodeKind::Element {
            namespace: Namespace::Html,
            name,
            ..
        }) if if realm.is_html_document {
            name.as_str().eq_ignore_ascii_case("body")
                || name.as_str().eq_ignore_ascii_case("frameset")
        } else {
            matches!(name.as_str(), "body" | "frameset")
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_runtime::Runtime;

    fn evaluate(page: &str, source: &str) -> Value {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        crate::install(engine.ctx(), page, 256).expect("install document");
        match engine.eval_value(source).expect("valid event-handler test") {
            Ok(value) => value,
            Err(error) => match engine.describe_throw(error) {
                lumen::Completion::Throw { name, message } => panic!("{name}: {message}"),
                _ => unreachable!("describe_throw returns a throw completion"),
            },
        }
    }

    #[test]
    fn specification_handler_compilation_after_live_blank_frame_adoption_uses_actual_document_realm() {
        let value=evaluate("<body></body>",r#"
            const require=(value,label)=>{if(!value)throw new Error(label)};
            const first=document.createElement('iframe'),second=document.createElement('iframe');
            document.body.append(first,second);
            const target=second.contentDocument.body;
            target.setAttribute('onclick','return this.ownerDocument');
            first.contentDocument.documentElement.appendChild(target);
            const getter=Object.getOwnPropertyDescriptor(HTMLElement.prototype,'onclick').get;
            const callback=getter.call(target);
            require(callback!==null&&callback.constructor===first.contentWindow.Function,'actual initial-blank document realm');
            require(callback.call(target)===first.contentDocument,'current element scope');
            let errors=0;first.contentWindow.addEventListener('error',event=>{
                require(event.error instanceof first.contentWindow.SyntaxError,'compile error origin');
                errors++;event.preventDefault();
            });
            target.setAttribute('onmousedown','1 *-* 2');
            require(errors===0,'deferred compilation');
            const invalid=Object.getOwnPropertyDescriptor(HTMLElement.prototype,'onmousedown').get.call(target);
            require(invalid===null&&errors===1,'one error event in adopted document');
            first.remove();second.remove();true
        "#);
        assert!(matches!(value,Value::Bool(true)));
    }

    #[test]
    fn specification_global_handler_registry_places_members_and_preserves_receiver_and_target_rules() {
        let globals=HANDLERS.iter().filter(|(name,_)|!WINDOW_HANDLERS.contains(name))
            .map(|(name,_)|format!("'{name}'")).collect::<Vec<_>>().join(",");
        let windows=WINDOW_HANDLERS.iter().map(|name|format!("'{name}'")).collect::<Vec<_>>().join(",");
        let source=format!(r#"
            const require=(value,label)=>{{if(!value)throw new Error(label)}};
            for(const name of [{globals}]) {{
                require(Object.hasOwn(window,name),'own global '+name);
                for(const prototype of [HTMLElement.prototype,SVGElement.prototype,Document.prototype])
                    require(Object.hasOwn(prototype,name),'mixin prototype '+name);
                require(!Object.hasOwn(Element.prototype,name),'generic Element '+name);
                for(const target of [window,document,document.createElement('meta'),document.createElementNS('http://www.w3.org/2000/svg','svg')]) {{
                    require(target[name]===null,'initial '+name);
                    const object={{}};target[name]=object;require(target[name]===object,'object identity '+name);
                    target[name]=undefined;require(target[name]===null,'clear '+name);
                }}
                const descriptor=Object.getOwnPropertyDescriptor(window,name);
                require(descriptor.configurable&&descriptor.enumerable,'global descriptor '+name);
            }}
            for(const name of [{windows}]) {{
                require(Object.hasOwn(HTMLBodyElement.prototype,name),'body window mixin '+name);
                require(Object.hasOwn(HTMLFrameSetElement.prototype,name),'frameset window mixin '+name);
                require(!Object.hasOwn(HTMLElement.prototype,name),'window-only applicability '+name);
            }}
            for(const name of ['onblur','onerror','onfocus','onload','onresize','onscroll',...[{windows}]]) {{
                const body=document.createElement('body'),frameset=document.createElement('frameset'),object={{}};
                body[name]=object;require(window[name]===object&&frameset[name]===object,'disconnected reflection '+name);
                const inert=document.implementation.createHTMLDocument('inert').body;
                inert[name]=object;require(inert[name]===null&&window[name]===object,'inactive target '+name);
                window[name]=null;
            }}
            for(const name of ['onmouseenter','onmouseleave']) {{
                const descriptor=Object.getOwnPropertyDescriptor(HTMLElement.prototype,name);
                require(descriptor.get.call({{}})===undefined,'lenient getter '+name);
                require(descriptor.set.call({{}},()=>{{}})===undefined,'lenient setter '+name);
            }}
            const strict=Object.getOwnPropertyDescriptor(HTMLElement.prototype,'onabort');
            let rejected=false;try{{strict.get.call({{}})}}catch(error){{rejected=error instanceof TypeError}}
            require(rejected,'strict receiver');
            const element=document.createElement('meta');
            for(const [attribute,type] of [['onwebkitanimationend','webkitAnimationEnd'],['onwebkitanimationiteration','webkitAnimationIteration'],['onwebkitanimationstart','webkitAnimationStart'],['onwebkittransitionend','webkitTransitionEnd']]) {{
                element.webkitRan=false;element.setAttribute(attribute,'this.webkitRan=true');
                element.dispatchEvent(new Event(type));require(element.webkitRan===true,'legacy event type mapping '+attribute);
                const callback=()=>{{element.webkitIdlRan=true}};
                element.webkitIdlRan=false;element[attribute]=callback;element.dispatchEvent(new Event(type));
                require(element[attribute]===callback&&element.webkitIdlRan===true,'legacy IDL event type mapping '+attribute);
            }}
            element.setAttribute('onbeforematch','this.matched=true');
            element.dispatchEvent(new Event('beforematch'));require(element.matched===true,'content handler registry');
            true
        "#);
        assert!(matches!(evaluate("<body></body>",&source),Value::Bool(true)));
    }

    #[test]
    fn specification_raw_handler_compilation_respects_captured_sandbox_and_logical_document_without_blocking_idl() {
        let mut runtime=Runtime::new();let engine=runtime.engine();
        let realm=crate::install(engine.ctx(),"<body><button id=target></button></body>",128).unwrap();
        // This is the same captured restriction that child-frame installation
        // sets from its active sandbox flags, rather than a live attribute read.
        realm.lifecycle.sandboxed_automatic_features.set(true);
        let value=engine.eval_value(r#"
            const require=(value,label)=>{if(!value)throw new Error(label)};
            const target=document.getElementById('target');
            target.setAttribute('onabort','this.rawRan=true');
            require(target.onabort===null,'sandboxed source does not compile');
            target.dispatchEvent(new Event('abort'));require(!target.rawRan,'sandboxed source does not run');
            target.onabort=()=>{target.idlRan=true};target.dispatchEvent(new Event('abort'));
            require(target.idlRan===true,'author callback is independent of compilation restrictions');
            const inert=document.implementation.createHTMLDocument('inert');
            inert.body.setAttribute('onresize','window.resurrected=true');
            require(inert.body.onresize===null,'inactive body source target');
            const object={};window.onresize=object;document.adoptNode(inert.body);
            require(window.onresize===object,'adoption does not reactivate an ignored body source');
            true
        "#).expect("valid sandbox-handler guard").unwrap_or_else(|error|match engine.describe_throw(error) {
            lumen::Completion::Throw{name,message}=>panic!("sandbox handler guard: {name}: {message}"),
            _=>unreachable!("describe_throw returns a throw completion"),
        });
        assert!(matches!(value,Value::Bool(true)));
    }

    #[test]
    fn specification_event_handler_lazy_compilation_uses_adopted_document_realm_and_preserves_compiled_origin() {
        let mut runtime=Runtime::new();let engine=runtime.engine();
        crate::install(engine.ctx(),"<title>parent-title</title><body></body>",128).unwrap();
        let child=engine.ctx().create_host_realm();
        let (_realm,foreign)=engine.ctx().with_host_realm(&child,|ctx| {
            let realm=crate::install(ctx,"<title>child-title</title><button id=raw onclick='return document.title'></button><button id=compiled onclick='return document.title'></button>",128).unwrap();
            let global=ctx.global_object();
            let foreign=ctx.eval_in_realm(&global,"({raw:document.getElementById('raw'),compiled:document.getElementById('compiled'),saved:document.getElementById('compiled').onclick,prototype:Function.prototype})").ok().expect("foreign handler records");
            (realm,foreign)
        }).ok().expect("child event-handler realm");
        let global=engine.ctx().global_object();engine.ctx().set_member(&global,"foreignHandlers",foreign).ok().expect("share actual foreign nodes");
        let value=engine.eval_value(r#"
            const require=(value,label)=>{if(!value)throw new Error(label)};
            document.adoptNode(foreignHandlers.raw);document.adoptNode(foreignHandlers.compiled);
            const raw=foreignHandlers.raw.onclick;
            require(Object.getPrototypeOf(raw)===Function.prototype,'raw adopted source uses current document realm');
            require(raw.call(foreignHandlers.raw)==='parent-title','raw source uses current document scope');
            require(foreignHandlers.compiled.onclick===foreignHandlers.saved,'compiled origin callback identity is stable');
            require(Object.getPrototypeOf(foreignHandlers.saved)===foreignHandlers.prototype,'compiled original intrinsics retained');
            require(foreignHandlers.saved.call(foreignHandlers.compiled)==='child-title','compiled original document scope retained');
            true
        "#).expect("valid foreign handler source");
        match value {Ok(Value::Bool(true))=>(),Err(error)=>match engine.describe_throw(error){
            lumen::Completion::Throw{name,message}=>panic!("{name}: {message}"),_=>unreachable!(),
        },_=>panic!("foreign handler assertion result")}
    }

    #[test]
    fn specification_event_handler_null_reinsert_adoption_and_equal_attribute_updates_preserve_state() {
        let value=evaluate("<button id=target onclick='globalThis.contentCalls++'></button>",r#"
            globalThis.contentCalls=0;
            const check=(value,label)=>{if(!value)throw new Error(label)};
            const target=document.getElementById('target');
            const source=target.getAttribute('onclick');
            const first=target.onclick;
            check(typeof first==='function','initial content is lazy callable source');
            target.onclick=null;
            for(let i=0;i<3;i++){target.remove();document.body.appendChild(target);check(target.onclick===null,'reinsertion cannot replay unchanged source')}
            target.setAttribute('onclick',source);
            const second=target.onclick;
            check(typeof second==='function'&&second!==first,'equal source setter reactivates');
            target.setAttribute('onclick',source);
            check(target.onclick!==second,'each actual equal-valued setter installs new raw source');
            target.dispatchEvent(new Event('click'));
            check(contentCalls===1,'one active callback');
            const body=document.body;body.setAttribute('onload','globalThis.contentCalls+=10');
            window.onload=null;body.remove();document.documentElement.appendChild(body);
            check(window.onload===null,'body reinsertion cannot resurrect a Window handler');
            body.setAttribute('onload',body.getAttribute('onload'));
            check(typeof window.onload==='function','equal body content assignment reactivates the Window target');
            const inert=document.implementation.createHTMLDocument('inert');
            const compiled=target.onclick;
            inert.adoptNode(target);
            check(target.onclick===compiled,'compiled callback identity survives adoption');
            document.adoptNode(target);document.body.appendChild(target);
            check(target.onclick===compiled,'compiled callback survives round-trip adoption');
            target.onclick=null;
            inert.adoptNode(target);document.adoptNode(target);document.body.appendChild(target);
            check(target.onclick===null,'null state survives identity migration');
            const pending=inert.createElement('button');
            pending.setAttribute('onclick','this.dataset.owner = document.title');
            check(pending.onclick===null,'inactive raw source stays pending');
            document.title='active-owner';document.adoptNode(pending);document.body.appendChild(pending);
            check(typeof pending.onclick==='function','pending source compiles after active adoption');
            pending.dispatchEvent(new Event('click'));
            check(pending.dataset.owner==='active-owner','compilation uses current node document scope');
            const xml=new DOMParser().parseFromString('<button xmlns="http://www.w3.org/1999/xhtml" onclick="this.dataset.xml = document.title" ONCLICK="throw new Error(&quot;wrong case&quot;)"/>','application/xhtml+xml');
            const xmlButton=xml.documentElement;
            check(xmlButton.onclick===null,'XML document has no browsing context');
            document.adoptNode(xmlButton);document.body.appendChild(xmlButton);
            xmlButton.dispatchEvent(new Event('click'));
            check(xmlButton.dataset.xml==='active-owner','XML null-namespace names preserve case and pending scope');
            const erased=document.createElement('button');erased.setAttribute('onclick','globalThis.contentCalls+=100');
            document.body.appendChild(erased);document.open();document.write('<body></body>');document.close();
            check(erased.onclick===null,'document.open erases handler state');
            document.body.appendChild(erased);
            check(erased.onclick===null,'erased retained node is not reactivated by insertion');
            true
        "#);
        assert!(matches!(value,Value::Bool(true)));
    }

    #[test]
    fn specification_event_handler_parser_attribute_birth_replaces_constructor_idl_state() {
        let mut runtime=Runtime::new();let engine=runtime.engine();
        let realm=crate::install(engine.ctx(),"<body></body>",64).unwrap();
        let value=engine.eval_value("globalThis.parserHandler=document.createElement('button'); parserHandler.onclick=()=>false; parserHandler").expect("valid construction source").ok().expect("constructed handler element");
        let node=engine.ctx().with_instance::<DomNode,_>(&value,|node|node.id).ok().expect("actual native identity");
        realm.session.borrow_mut().document_mut().set_attribute(node,"onclick","return document.title").unwrap();
        initialize_subtree(engine.ctx(),&realm,node).unwrap();
        let value=engine.eval_value("document.title='token-source'; parserHandler.onclick.call(parserHandler)==='token-source'").expect("valid token source assertion").ok().expect("token source assertion");
        assert!(matches!(value,Value::Bool(true)),"a later parser token attribute is an actual source activation, even after constructor IDL assignment");
    }

    #[test]
    fn specification_event_handler_nullable_objects_preserve_identity_and_never_coerce() {
        let value = evaluate("<button id=target onclick='globalThis.handlerCalls++'></button>", r#"
            globalThis.handlerCalls=0;globalThis.handlerOrder=[];
            const target=document.getElementById('target');
            const check=(value,message)=>{if(!value)throw new Error(message)};
            let inspected=0;
            const object={get handleEvent(){inspected++;throw new Error('callback interface lookup')},
                get valueOf(){inspected++;throw new Error('coercion')},
                get toString(){inspected++;throw new Error('coercion')}};
            const before=()=>{handlerCalls+=10;handlerOrder.push('before')};
            const after=()=>{handlerCalls+=100;handlerOrder.push('after')};
            target.addEventListener('click',before);
            target.onclick=object;
            target.addEventListener('click',after);
            check(target.onclick===object,'non-callable object identity');
            target.dispatchEvent(new Event('click',{cancelable:true}));
            check(handlerCalls===110&&inspected===0,'legacy objects do nothing without handleEvent or coercion');
            handlerOrder=[];target.onclick=()=>{handlerCalls++;handlerOrder.push('handler');return false};
            check(!target.dispatchEvent(new Event('click',{cancelable:true})),'callable false return cancels');
            check(handlerCalls===221&&handlerOrder.join(',')==='handler,before,after','replacement retains the activated listener position');
            for(const primitive of [undefined,null,false,0,NaN,'source',Symbol('handler'),1n]){
                target.onclick=primitive;
                window.onerror=primitive;
                document.oninput=primitive;
                check(target.onclick===null&&window.onerror===null&&document.oninput===null,'primitive callback attributes clear');
            }
            window.onerror=object;document.oninput=object;
            check(window.onerror===object&&document.oninput===object,'Window and Document use the shared nullable conversion');
            const media=matchMedia('(width:100px)');media.onchange=object;
            check(media.onchange===object,'media callback attribute preserves objects');
            media.dispatchEvent(new Event('change'));
            const fonts=document.fonts;fonts.onloadingdone=object;
            check(fonts.onloadingdone===object,'font callback attribute preserves objects');
            fonts.dispatchEvent(new Event('loadingdone'));
            check(inspected===0,'non-callable legacy object must never become an EventListener callback interface');
            handlerOrder=[];target.setAttribute('onclick',"globalThis.handlerCalls+=1000;handlerOrder.push('content')");
            check(typeof target.onclick==='function','a content attribute can replace the IDL-cleared handler');
            target.dispatchEvent(new Event('click'));
            check(handlerCalls===1331&&handlerOrder.join(',')==='before,after,content','reactivation appends the new handler after existing listeners');
            true
        "#);
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn specification_attribute_setters_convert_omitted_values_without_relaxing_operations() {
        let value=evaluate("<button id=target></button>",r#"
            const target=document.getElementById('target');
            const check=(value,label)=>{if(!value)throw new Error(label)};
            const descriptor=(object,name)=>{
                for(let prototype=object;prototype;prototype=Object.getPrototypeOf(prototype)){
                    const entry=Object.getOwnPropertyDescriptor(prototype,name);
                    if(entry)return entry;
                }
                throw new Error('missing actual native attribute '+name);
            };
            target.onclick=()=>false;
            const onclick=descriptor(target,'onclick');
            check(onclick.set.length===1&&onclick.get.length===0,'IDL accessor lengths');
            onclick.set.call(target);
            check(target.onclick===null,'omitted handler setter converts undefined');
            descriptor(target,'id').set.call(target);
            check(target.id==='undefined','omitted string setter converts undefined');
            const col=document.createElement('col');
            descriptor(col,'span').set.call(col);
            check(col.getAttribute('span')==='0'&&col.span===1,'omitted unsigned setter converts then reflects');
            let failed=false;
            try{target.addEventListener()}catch(error){failed=error instanceof TypeError}
            check(failed,'ordinary required callback operation keeps its arity check');
            failed=false;
            try{target.setAttribute()}catch(error){failed=error instanceof TypeError}
            check(failed,'ordinary required string operation keeps its arity check');
            true
        "#);
        assert!(matches!(value,Value::Bool(true)));
    }

    #[test]
    fn volumechange_handler_uses_native_slot_receiver_and_queued_event_order() {
        let mut runtime = Runtime::new();
        let engine = runtime.engine();
        let realm = crate::install(engine.ctx(),
            r#"<audio id="audio" onvolumechange="calls.push('content')"></audio>"#, 64).unwrap();
        let evaluate = |engine: &mut lumen::Engine, source: &str| {
            engine.eval_value(source).expect("valid media-handler script")
                .ok().expect("media-handler script threw")
        };
        evaluate(engine, r#"
            var calls = [], receiverCorrect = false;
            const audio = document.getElementById('audio');
            const content = audio.onvolumechange;
            audio.addEventListener('volumechange', () => calls.push('listener'));
            audio.onvolumechange = function(event) {
                calls.push('idl');
                receiverCorrect = this === audio && event.currentTarget === audio && event.target === audio;
            };
            audio.muted = true;
            Promise.resolve().then(() => calls.push('micro'));
            if (calls.length) throw new Error('media handler fired synchronously');
        "#);
        while engine.run_one_job() {}
        assert!(matches!(evaluate(engine, "calls.join(',')==='micro'"), Value::Bool(true)));
        assert_eq!(realm.queue_media_tasks(engine.ctx()).unwrap(), 1);
        assert!(scheduling::run_tasks(engine, 64).is_empty());
        assert!(matches!(evaluate(engine,
            "receiverCorrect && calls.join(',')==='micro,idl,listener'"), Value::Bool(true)));
        evaluate(engine, "calls=[];audio.onvolumechange=null;audio.muted=false");
        assert_eq!(realm.queue_media_tasks(engine.ctx()).unwrap(), 1);
        assert!(scheduling::run_tasks(engine, 64).is_empty());
        assert!(matches!(evaluate(engine,
            "audio.onvolumechange===null && calls.join(',')==='listener'"), Value::Bool(true)));
        evaluate(engine, "calls=[];audio.setAttribute('onvolumechange',\"calls.push('attribute')\");audio.muted=true");
        assert_eq!(realm.queue_media_tasks(engine.ctx()).unwrap(), 1);
        assert!(scheduling::run_tasks(engine, 64).is_empty());
        assert!(matches!(evaluate(engine,
            "calls.join(',')==='listener,attribute'"), Value::Bool(true)));
    }

    #[test]
    fn content_handlers_compile_with_form_scope_and_keep_attribute_order() {
        let value = evaluate(
            r#"<form id="formScope"><input id="target" name="control" onload="globalThis.calls.push('content'); globalThis.scopeCheck = [documentMarker, formMarker, elementMarker, event.type, arguments.length, this === document.getElementById('target')].join('|')"></form><main id="mount"></main>"#,
            r#"
                globalThis.calls = [];
                const target = document.getElementById('target');
                const form = document.getElementById('formScope');
                globalThis.documentMarker = 'global';
                globalThis.formMarker = 'global';
                globalThis.elementMarker = 'global';
                document.documentMarker = 'document';
                form.formMarker = 'form';
                target.elementMarker = 'element';
                const first = target.onload;
                target.addEventListener('load', () => calls.push('listener'));
                target.dispatchEvent(new Event('load'));
                const parserAttributeRunsFirst = calls.join(',') === 'content,listener';
                const scopeIsCorrect = scopeCheck === 'document|form|element|load|1|true' &&
                  first.name === 'onload';

                target.onload = () => calls.push('property');
                target.dispatchEvent(new Event('load'));
                const idlOverrideKeepsSlot = calls.slice(2).join(',') === 'property,listener';
                target.setAttribute('onload', "calls.push('replacement')");
                target.dispatchEvent(new Event('load'));
                const attributeOverridesIdl = calls.slice(4).join(',') === 'replacement,listener';
                target.removeAttribute('onload');
                target.dispatchEvent(new Event('load'));
                const removalDeactivatesOnlyHandler = target.onload === null &&
                  calls.slice(6).join(',') === 'listener';

                const mount = document.getElementById('mount');
                mount.innerHTML = '<img id="inserted" onerror="globalThis.insertedError = this === event.currentTarget && event.type === \'error\'">';
                const inserted = document.getElementById('inserted');
                inserted.dispatchEvent(new Event('error'));
                parserAttributeRunsFirst && scopeIsCorrect && idlOverrideKeepsSlot &&
                  attributeOverridesIdl && removalDeactivatesOnlyHandler && insertedError
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn invalid_content_handler_is_reported_and_getter_returns_null() {
        let value = evaluate(
            r#"<div id="broken" onload="return ("></div>"#,
            r#"
                let reports = 0;
                let reportedSyntaxError = false;
                window.addEventListener('error', event => {
                  reports++;
                  reportedSyntaxError = event.error instanceof SyntaxError &&
                    event.message.length > 0;
                });
                const target = document.getElementById('broken');
                const getter = target.onload;
                getter === null && reports === 1 && reportedSyntaxError
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn click_keyboard_form_and_submit_content_handlers_use_native_targets() {
        let value = evaluate(
            r#"<form id="scope-form"></form><button id="button" form="scope-form" onclick="globalThis.clickScope = [documentMarker, formMarker, elementMarker, this === event.currentTarget].join('|'); return false" onkeydown="globalThis.keyValue = event.key"></button><input id="control" form="scope-form" oninput="globalThis.inputScope = [documentMarker, formMarker, controlMarker, this === event.currentTarget].join('|')"><form id="submit-form" onsubmit="globalThis.submits = (globalThis.submits || 0) + 1; return false" onreset="globalThis.resets = (globalThis.resets || 0) + 1"></form>"#,
            r#"
                globalThis.documentMarker = 'global';
                globalThis.formMarker = 'global';
                globalThis.elementMarker = 'global';
                globalThis.controlMarker = 'global';
                document.documentMarker = 'document';
                const scopeForm = document.getElementById('scope-form');
                scopeForm.formMarker = 'form';
                const button = document.getElementById('button');
                button.elementMarker = 'button';
                const clickHandler = button.onclick;
                const clickAllowed = button.dispatchEvent(new MouseEvent('click', { cancelable: true }));
                const clickPassed = !clickAllowed &&
                  clickScope === 'document|form|button|true' && clickHandler === button.onclick;

                const keyAllowed = button.dispatchEvent(new KeyboardEvent('keydown', { key: 'K' }));
                const keyPassed = keyAllowed && keyValue === 'K' && typeof button.onkeydown === 'function';

                const control = document.getElementById('control');
                control.controlMarker = 'control';
                control.dispatchEvent(new Event('input'));
                const formScopePassed = inputScope === 'document|form|control|true';

                const submitForm = document.getElementById('submit-form');
                const submitAllowed = submitForm.dispatchEvent(new Event('submit', { cancelable: true }));
                const resetAllowed = submitForm.dispatchEvent(new Event('reset', { cancelable: true }));
                clickPassed && keyPassed && formScopePassed && !submitAllowed &&
                  submitForm.dispatchEvent(new Event('submit', { cancelable: true })) === false &&
                  resetAllowed && submits === 2 && resets === 1
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn specification_window_history_handlers_share_body_content_and_idl_slots() {
        let value = evaluate(
            r#"<body onpopstate="globalThis.historyHandlerCalls.push(['attribute', event.state.answer, this === window])" onhashchange="globalThis.historyHandlerCalls.push(['hash', event.oldURL, event.newURL])"></body>"#,
            r#"(() => {
                globalThis.historyHandlerCalls=[];
                const body=document.body;
                const check=(value,message)=>{if(!value)throw new Error(message);};
                if(typeof window.onpopstate!=='function' || body.onpopstate!==window.onpopstate ||
                    typeof window.onhashchange!=='function' || body.onhashchange!==window.onhashchange)
                    throw new Error('initial Window/body history handler reflection');
                dispatchEvent(new PopStateEvent('popstate',{state:{answer:42}}));
                dispatchEvent(new HashChangeEvent('hashchange',{oldURL:'#old',newURL:'#new'}));
                const replacement=function(event){historyHandlerCalls.push(['idl',event.state.answer,this===window]);};
                body.onpopstate=replacement;
                check(window.onpopstate===replacement,'body IDL setter must replace Window handler');
                dispatchEvent(new PopStateEvent('popstate',{state:{answer:7}}));
                window.onpopstate=null;
                check(body.onpopstate===null,'Window clearing must clear body handler');
                dispatchEvent(new PopStateEvent('popstate',{state:{answer:0}}));
                body.setAttribute('onpopstate',"globalThis.historyHandlerCalls.push(['changed', event.state.answer, this === window])");
                check(typeof window.onpopstate==='function' && body.onpopstate===window.onpopstate,'attribute replacement must share Window/body handler');
                dispatchEvent(new PopStateEvent('popstate',{state:{answer:9}}));
                body.removeAttribute('onhashchange');
                const valid=body.onhashchange===null && window.onhashchange===null &&
                    JSON.stringify(historyHandlerCalls)===JSON.stringify([
                        ['attribute',42,true],['hash','#old','#new'],['idl',7,true],['changed',9,true]]);
                check(valid,'actual handler calls '+JSON.stringify(historyHandlerCalls));
                return true;
            })()"#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }

    #[test]
    fn body_load_and_error_attributes_reflect_window_handlers() {
        let value = evaluate(
            r#"<body onload="globalThis.bodyLoadState = this === window && event.currentTarget === window && event.target === window" onerror="globalThis.bodyErrorArgs = [event, source, lineno, colno, error]; return true"></body>"#,
            r#"
                const body = document.body;
                const loadHandler = body.onload;
                const reflectedLoad = loadHandler === window.onload;
                const loadAllowed = window.dispatchEvent(new Event('load'));

                const original = new Error('original body error');
                const errorEvent = new ErrorEvent('error', {
                  message: 'resource failed', filename: 'asset.js', lineno: 12,
                  colno: 7, error: original, cancelable: true
                });
                const errorAllowed = window.dispatchEvent(errorEvent);
                const errorHandler = body.onerror;
                reflectedLoad && loadAllowed && bodyLoadState && !errorAllowed &&
                  errorHandler === window.onerror && bodyErrorArgs[0] === 'resource failed' &&
                  bodyErrorArgs[1] === 'asset.js' && bodyErrorArgs[2] === 12 &&
                  bodyErrorArgs[3] === 7 && bodyErrorArgs[4] === original
            "#,
        );
        assert!(matches!(value, Value::Bool(true)));
    }
}
