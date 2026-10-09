//! Script adapter for shared HTML focus scopes and autofocus lifecycle.
use super::*;
use lumen::embed::{JsHost, Slot};
use lumen_bind::{FromArg, Host};
use std::sync::atomic::{AtomicU64,Ordering};
static INSERTION_SEQUENCE: AtomicU64=AtomicU64::new(1);

#[derive(Clone, Copy, Default)]
pub(crate) struct FocusOptions {
    focus_visible: Option<bool>,
    prevent_scroll: bool,
}

impl<'a> FromArg<'a, JsHost> for FocusOptions {
    fn from_arg(cx: &'a lumen::embed::ArgCx<'_>, value: &'a Value, _at: Slot) -> Result<Self, Value> {
        if matches!(value, Value::Undefined | Value::Null) { return Ok(Self::default()); }
        <JsHost as Host>::with_ctx(cx, |ctx| {
            if !matches!(value, Value::Obj(_)) { return Err(ctx.make_error("TypeError", "FocusOptions requires an object")); }
            // Web IDL dictionary members are read in lexicographic order before
            // the operation can change focus, including getters that throw.
            let visible = ctx.member_get(value, "focusVisible")?;
            let focus_visible = (!matches!(visible, Value::Undefined)).then(|| ctx.to_boolean(&visible));
            let prevent = ctx.member_get(value, "preventScroll")?;
            Ok(Self { focus_visible, prevent_scroll: ctx.to_boolean(&prevent) })
        })
    }
}

pub(crate) fn focus_element(ctx: &mut Ctx, realm: &Rc<DomRealm>, node: NodeId, options: FocusOptions) -> OpResult<()> {
    let (realm,node)=realm.resolve_adopted_node(node);
    if !allow_focus_with_context(Some(ctx),&realm) { return Ok(()); }
    realm.focus(ctx, Some(node))?;
    if let Some(visible)=options.focus_visible {
        realm.focus_visible.set(if visible { realm.focused_node() } else { None });
        realm.publish_interaction_state();
    }
    if !options.prevent_scroll {
        scrolling::into_view_inner(ctx, &realm, node, scrolling::IntoViewOptions {
            behavior: scrolling::ScrollBehavior::Auto,
            block: scrolling::ScrollAlignment::Center,
            inline: scrolling::ScrollAlignment::Center,
            nearest_container: false,
        })?;
    }
    Ok(())
}

pub(crate) fn blur_element(ctx: &mut Ctx, realm: &Rc<DomRealm>, node: NodeId) -> OpResult<()> {
    let (realm,node)=realm.resolve_adopted_node(node);
    let Some(focused)=realm.focused_node() else { return Ok(()); };
    let delegated={
        let session=realm.session.borrow();let document=session.document();
        document.shadow_root(node).map_err(dom_error)?.filter(|root|
            document.shadow_options(*root).ok().flatten().is_some_and(|options|options.delegates_focus))
            .is_some_and(|root|document.is_host_including_inclusive_ancestor(root,focused).unwrap_or(false))
    };
    if focused!=node && !delegated { return Ok(()); }
    if !realm.focus_rendered(focused)? { return Ok(()); }
    realm.focus(ctx,None)?;
    if let Some(top)=top_document(&realm).filter(|top|!Rc::ptr_eq(top,&realm)) {
        let handle=top.browsing_context().map(|context|context.realm_handle())
            .unwrap_or_else(||ctx.current_host_realm());
        ctx.with_host_realm(&handle,|ctx|top.focus(ctx,None))
            .map_err(|error|OpError::new("InvalidStateError",error.to_string()))??;
    }
    Ok(())
}

macro_rules! bind_focus_mixin {
    ($ty:ident { $($body:tt)* }) => {
        #[lumen_bind::methods]
        impl $ty {
            $($body)*
            #[getter]
            fn tab_index(&self) -> lumen::embed::OpResult<i32> {
                let (realm,node)=self.base.base.realm.resolve_adopted_node(self.base.base.id);
                let index=lumen_html::focus::idl_tabindex(realm.session.borrow().document(),node);
                Ok(index)
            }
            #[setter(hint(js(ce_reactions)))]
            fn set_tab_index(&self, index:i32) -> lumen::embed::OpResult<()> {
                self.base.base.set_attribute_core("tabindex", &index.to_string())
            }
            #[getter]
            fn autofocus(&self) -> lumen::embed::OpResult<bool> {
                Ok(self.base.base.get_null_attribute("autofocus")?.is_some())
            }
            #[setter(hint(js(ce_reactions)))]
            fn set_autofocus(&self, value:bool) -> lumen::embed::OpResult<()> {
                if value { self.base.base.set_attribute_core("autofocus", "") }
                else { self.base.base.remove_attribute_core("autofocus") }
            }
            fn focus(ctx: &mut lumen::embed::Ctx, this:lumen_bind::This<lumen::embed::Value>, options:Option<crate::focus::FocusOptions>) -> lumen::embed::OpResult<()> {
                let (realm,node)=ctx.with_instance::<$ty,_>(&this.0,|element|(element.base.base.realm.clone(),element.base.base.id))?;
                crate::focus::focus_element(ctx,&realm,node,options.unwrap_or_default())
            }
            fn blur(ctx:&mut lumen::embed::Ctx, this:lumen_bind::This<lumen::embed::Value>) -> lumen::embed::OpResult<()> {
                let (realm,node)=ctx.with_instance::<$ty,_>(&this.0,|element|(element.base.base.realm.clone(),element.base.base.id))?;
                crate::focus::blur_element(ctx,&realm,node)
            }
        }
    }
}
pub(crate) use bind_focus_mixin;

struct PendingAutofocusCandidate {
    node: NodeId,
    sequence: u64,
    // Live insertion has already passed the insertion-time permission checks.
    // A host's pre-parsed document is initialized before its navigable is bound;
    // only that bootstrap snapshot still needs the shared admission checks.
    permission_checked: bool,
}

#[derive(Default)]
pub(crate) struct AutofocusState {
    processed: bool,
    pending: Vec<PendingAutofocusCandidate>,
    candidates: Vec<(std::rc::Weak<DomRealm>,NodeId,u64)>,
    blocking_stylesheets: HashMap<NodeId,usize>,
    admission_failed: bool,
}

pub(crate) fn resolve_focus_target(realm: &Rc<DomRealm>, node: NodeId, pointer: bool) -> OpResult<Option<NodeId>> {
    {
        let session=realm.session.borrow(); let document=session.document();
        if document.parent(node).map_err(dom_error)? == Some(document.root()) { return Ok(Some(document.root())); }
    }
    if realm.focusable_node(node)? { return Ok(Some(node)); }
    let shadow={
        let session=realm.session.borrow();
        let document=session.document();
        document.shadow_root(node).map_err(dom_error)?.filter(|root|
            document.shadow_options(*root).ok().flatten().is_some_and(|options|options.delegates_focus))
    };
    let Some(shadow)=shadow else {
        if pointer {
            // Dispatch retains its hit target; the focusing default action uses
            // the nearest focusable composed ancestor. Programmatic focus does
            // not climb, and delegated shadow focus keeps its existing search.
            let mut current=node;
            for _ in 0..512 {
                let parent=realm.session.borrow().document().composed_parent(current).map_err(dom_error)?;
                let Some(parent)=parent else {return Ok(None);};
                if let Some(target)=resolve_focus_target(realm,parent,false)? {return Ok(Some(target));}
                current=parent;
            }
            return Err(OpError::new("QuotaExceededError","Pointer focus ancestry limit"));
        }
        return Ok(None);
    };
    if let Some(focused)=realm.focused_node() {
        if realm.session.borrow().document().is_host_including_inclusive_ancestor(node,focused).map_err(dom_error)? {
            return Ok(Some(focused));
        }
    }
    // Search ordinary descendants. Cross only shadow boundaries that themselves
    // delegate, using an explicit stack rather than authored-depth recursion.
    let mut pending=vec![shadow];
    let mut fallback=None;
    while let Some(node)=pending.pop() {
        let (autofocus,children,delegated)={
            let session=realm.session.borrow(); let document=session.document();
            let autofocus=document.get_attribute_ns_ref(node,None,"autofocus").ok().flatten().is_some();
            let delegated=document.shadow_root(node).map_err(dom_error)?.filter(|root|
                document.shadow_options(*root).ok().flatten().is_some_and(|options|options.delegates_focus));
            let mut children=Vec::new();
            let mut child=document.first_child(node).map_err(dom_error)?;
            while let Some(id)=child {
                children.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","focus delegate allocation"))?;
                children.push(id); child=document.next_sibling(id).map_err(dom_error)?;
            }
            (autofocus,children,delegated)
        };
        let element=matches!(realm.session.borrow().document().kind(node),Ok(NodeKind::Element { .. }));
        if element && realm.focusable_node(node)? {
            if autofocus { return Ok(Some(node)); }
            fallback.get_or_insert(node);
        }
        if let Some(root)=delegated {
            pending.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","focus delegate allocation"))?;
            pending.push(root);
        } else {
            pending.try_reserve(children.len()).map_err(|_|OpError::new("QuotaExceededError","focus delegate allocation"))?;
            pending.extend(children.into_iter().rev());
        }
    }
    Ok(fallback)
}

pub(crate) fn focus_content_navigable(ctx: &mut Ctx, realm: &Rc<DomRealm>, node: NodeId) -> OpResult<()> {
    let frame=realm.frame_contexts.borrow().get(&node).and_then(std::rc::Weak::upgrade);
    if let Some(child)=frame.and_then(|context|context.document()) {
        if let Some(owner)=child.child_realm_handle()? {
            ctx.with_host_realm(&owner,|ctx|child.focus(ctx,None))
                .map_err(|error|OpError::new("InvalidStateError",error.to_string()))??;
        }
    }
    Ok(())
}

pub(crate) fn focus_parent_chain(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    let mut chain=Vec::new(); let mut context=realm.navigation_context();
    while let Some(current)=context.filter(|context|!context.is_top_level()) {
        if let Some(parent)=current.owner_document() {
            chain.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","focus chain allocation"))?;
            chain.push((parent,current.owner_node()));
        }
        context=current.parent_navigation_context();
    }
    for (parent,container) in chain.into_iter().rev() {
        if parent.focused_node()!=Some(container) {
            let handle=parent.child_realm_handle()?.unwrap_or_else(||ctx.current_host_realm());
            ctx.with_host_realm(&handle,|ctx|parent.focus(ctx,Some(container)))
                .map_err(|error|OpError::new("InvalidStateError",error.to_string()))??;
        }
    }
    Ok(())
}

pub(crate) fn autofocus_delegate(realm: &Rc<DomRealm>, root: NodeId) -> OpResult<Option<NodeId>> {
    let mut cursor=realm.session.borrow().document().first_child(root).map_err(dom_error)?;
    while let Some(node)=cursor {
        let autofocus=realm.session.borrow().document().get_attribute_ns_ref(node,None,"autofocus").ok().flatten().is_some();
        if autofocus {
            if let Some(target)=resolve_focus_target(realm,node,false)? { return Ok(Some(target)); }
        }
        cursor=selector::next_descendant(realm.session.borrow().document(),root,node).map_err(dom_error)?;
    }
    Ok(None)
}

fn local_order(realm: &Rc<DomRealm>) -> OpResult<Vec<NodeId>> {
    let candidates={
        let session=realm.session.borrow(); let document=session.document(); let root=document.root();
        let mut candidates=Vec::new(); let mut cursor=Some(root);
        while let Some(node)=cursor {
            if lumen_html::focus::focusable(document,node) {
                candidates.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","focus candidate allocation"))?;
                candidates.push(node);
            }
            cursor=selector::next_shadow_including_descendant(document,root,node).map_err(dom_error)?;
        }
        candidates
    };
    let mut rendered=HashSet::new();
    rendered.try_reserve(candidates.len()).map_err(|_|OpError::new("QuotaExceededError","focus candidate allocation"))?;
    for node in candidates { if realm.focus_rendered(node)? { rendered.insert(node); } }
    let session=realm.session.borrow(); let document=session.document();
    lumen_html::focus::sequential_order(document,|node|rendered.contains(&node),|node|document.popover_trigger(node)).map_err(dom_error)
}

/// A navigable container occupies its document's scope position; its active
/// document supplies the stops at that position. Cross-origin documents use
/// their native realm, without exposing their DOM through WindowProxy.
pub(crate) fn sequential_order(realm: &Rc<DomRealm>) -> OpResult<Vec<(Rc<DomRealm>,NodeId)>> {
    let top=top_document(realm).unwrap_or_else(||realm.clone());
    let mut pending=local_order(&top)?.into_iter().rev().map(|node|(top.clone(),node)).collect::<Vec<_>>();
    let mut result=Vec::new(); let mut visited=HashSet::new();
    visited.insert(top.session.borrow().document().root());
    while let Some((owner,node))=pending.pop() {
        let child=owner.frame_contexts.borrow().get(&node).and_then(std::rc::Weak::upgrade).and_then(|context|context.document());
        if let Some(child)=child {
            let root=child.session.borrow().document().root();
            visited.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","focus navigable allocation"))?;
            if visited.insert(root) {
                let order=local_order(&child)?;
                if !order.is_empty() {
                    pending.try_reserve(order.len()).map_err(|_|OpError::new("QuotaExceededError","focus navigable allocation"))?;
                    pending.extend(order.into_iter().rev().map(|node|(child.clone(),node)));
                    continue;
                }
            }
        }
        result.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","focus navigation allocation"))?;
        result.push((owner,node));
    }
    Ok(result)
}

pub(crate) fn currently_focused(realm: &Rc<DomRealm>) -> Option<NodeId> {
    let mut owner=top_document(realm).unwrap_or_else(||realm.clone());
    loop {
        let node=owner.focused_node()?;
        let child=owner.frame_contexts.borrow().get(&node).and_then(std::rc::Weak::upgrade).and_then(|context|context.document());
        match child {Some(child) if child.focused_node().is_some()=>owner=child,_=>return Some(node)}
    }
}

pub(crate) fn focus_in_realm(ctx: &mut Ctx, owner: &Rc<DomRealm>, node: NodeId) -> OpResult<()> {
    let handle=owner.child_realm_handle()?.unwrap_or_else(||ctx.current_host_realm());
    ctx.with_host_realm(&handle,|ctx|owner.focus(ctx,Some(node)))
        .map_err(|error|OpError::new("InvalidStateError",error.to_string()))?
}

fn autofocus_candidates(document:&lumen_html::Document,root:NodeId)->impl Iterator<Item=NodeId>+'_ {
    let mut cursor=Some(root);
    std::iter::from_fn(move||{
        while let Some(node)=cursor {
            cursor=selector::next_shadow_including_descendant(document,root,node).ok().flatten();
            if document.is_connected_element(node)&&document.get_attribute_ns_ref(node,None,"autofocus").ok().flatten().is_some(){return Some(node);}
        }
        None
    })
}
fn queue_candidate(state:&mut AutofocusState,node:NodeId,permission_checked:bool){
    state.pending.retain(|candidate|candidate.node!=node);
    let sequence=INSERTION_SEQUENCE.try_update(Ordering::Relaxed,Ordering::Relaxed,|value|value.checked_add(1));
    if let Ok(sequence)=sequence {
        if state.pending.try_reserve(1).is_ok(){state.pending.push(PendingAutofocusCandidate{node,sequence,permission_checked});}
        else{state.admission_failed=true;}
    }else{state.admission_failed=true;}
}
pub(crate) fn observe_insertion(realm:&Rc<DomRealm>,document:&lumen_html::Document,mutation:&lumen_html::observe::ObservedMutation){
    if !active(realm)||realm.lifecycle.sandboxed_automatic_features.get(){return;}
    let processed=top_document(realm).is_some_and(|top|top.autofocus.borrow().processed);
    for root in mutation.kind.added_nodes(){for node in autofocus_candidates(document,root){
        // Policy use is an autofocus candidate's insertion step. Unrelated
        // attribute/child mutations neither check nor report that feature.
        if !allow_focus(realm)||processed{continue;}
        queue_candidate(&mut realm.autofocus.borrow_mut(),node,true);
    }}
}
pub(crate) fn initial_candidates(realm:&Rc<DomRealm>){
    // Native root installation publishes its response identity immediately
    // afterward. Its parsed candidates must observe that response's policy.
    let permission_checked=active(realm)&&realm.document_identity.url.borrow().is_some();
    if permission_checked&&realm.lifecycle.sandboxed_automatic_features.get(){return;}
    let session=realm.session.borrow();let document=session.document();
    let mut candidates=autofocus_candidates(document,document.root()).peekable();
    if candidates.peek().is_none(){return;}
    for node in candidates{if !permission_checked||allow_focus(realm){queue_candidate(&mut realm.autofocus.borrow_mut(),node,permission_checked);}}
}

fn active(realm: &Rc<DomRealm>) -> bool {
    realm.browsing_context().is_some_and(|context|context.is_active() && context.document().is_some_and(|document|Rc::ptr_eq(&document,realm)))
}

fn top_document(realm: &Rc<DomRealm>) -> Option<Rc<DomRealm>> { realm.navigation_context()?.top_document() }

pub(crate) fn allow_focus(realm: &Rc<DomRealm>) -> bool {allow_focus_with_context(None,realm)}

/// HTML allow-focus uses the target Document, in this order, for element,
/// navigable and autofocus consumers. Script caller policy is not substituted.
pub(crate) fn allow_focus_with_context(ctx:Option<&mut Ctx>,realm:&Rc<DomRealm>)->bool {
    realm.use_permissions_policy(ctx,super::permissions_policy::FOCUS) || realm.has_transient_user_activation()
}

pub(crate) fn mark_processed(realm: &Rc<DomRealm>) {
    if let Some(top)=top_document(realm) {
        if !realm.document_origin().zip(top.document_origin()).is_some_and(|(origin,top)|origin.same_origin(&top)) { return; }
        let mut state=top.autofocus.borrow_mut(); state.processed=true; state.candidates.clear();
    }
}

pub(crate) fn admit_candidates(realm: &Rc<DomRealm>) -> OpResult<()> {
    if realm.autofocus.borrow().admission_failed { return Err(OpError::new("QuotaExceededError","autofocus candidate admission")); }
    let pending=std::mem::take(&mut realm.autofocus.borrow_mut().pending);
    if !active(realm) { return Ok(()); }
    let Some(top)=top_document(realm) else { return Ok(()); };
    let mut state=top.autofocus.borrow_mut();
    for candidate in pending {
        if !candidate.permission_checked && (realm.lifecycle.sandboxed_automatic_features.get() || !allow_focus(realm)) { continue; }
        if state.processed { continue; }
        let PendingAutofocusCandidate{node,sequence,..}=candidate;
        state.candidates.retain(|(owner,candidate,_)|*candidate!=node || !owner.ptr_eq(&Rc::downgrade(realm)));
        state.candidates.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","autofocus candidate allocation"))?;
        state.candidates.push((Rc::downgrade(realm),node,sequence));
    }
    Ok(())
}

/// Complete the native parse/replay insertion phase when response policy and
/// actual document ownership have been published, before author scripts. This
/// only admits candidates; focusing still belongs to the rendering checkpoint.
/// Preserve the existing producer-failure flag rather than abandoning a
/// published navigation or hiding an admission allocation failure.
pub(crate) fn admit_created_document(realm:&Rc<DomRealm>){
    if admit_candidates(realm).is_err(){realm.autofocus.borrow_mut().admission_failed=true;}
}
pub(crate) fn response_policy_ready(realm:&DomRealm){
    let Some(context)=realm.browsing_context().filter(|context|browsing_context::is_active_document(context,realm)) else{return;};
    if let Some(document)=context.document(){admit_created_document(&document);}
}

/// HTML update-the-rendering steps reveal the document, then flush the
/// top-level document's autofocus candidates before animation-frame callbacks.
/// Reuse the same fragment and focusing authorities as the later layout phase;
/// this checkpoint does not run layout observers, paint or a second evaluator.
pub(crate) fn rendering_checkpoint(ctx:&mut Ctx,timestamp:f64)->OpResult<()> {
    let Some(document)=super::window_globals::current_dom_realm(ctx) else{return Ok(());};
    if !active(&document) {return Ok(());}
    // A rendering task queued by a child still processes the eligible top-level
    // document first. RAF ownership remains with the caller's document.
    let Some(top)=top_document(&document) else{return Ok(());};
    if !active(&top) || top.lifecycle.hidden.get() || top.rendering_blocked_at(timestamp) {return Ok(());}
    let Some(handle)=top.relevant_host_realm(ctx) else{return Ok(());};
    ctx.with_host_realm(&handle,|ctx| {
        super::navigation_lifecycle::reveal(ctx)?;
        super::fragment::checkpoint(ctx,&top,false)?;
        flush_autofocus(ctx,&top)
    }).map_err(|error|OpError::new("InvalidStateError",error.to_string()))?
}

pub(crate) fn flush_autofocus(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    if !active(realm) || !realm.navigation_context().is_some_and(|context|context.is_top_level()) {return Ok(());}
    let Some(top)=top_document(realm) else { return Ok(()); };
    if top.autofocus.borrow().processed { return Ok(()); }
    let mut pending=vec![top.clone()];
    while let Some(document)=pending.pop() {
        admit_candidates(&document)?;
        for context in document.frame_contexts.borrow().values().filter_map(std::rc::Weak::upgrade) {
            if let Some(child)=context.document() {
                pending.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","autofocus navigable allocation"))?;
                pending.push(child);
            }
        }
    }
    top.autofocus.borrow_mut().candidates.sort_by_key(|(_,_,sequence)|*sequence);
    if top.autofocus.borrow().processed || top.autofocus.borrow().candidates.is_empty() { return Ok(()); }
    if top.focused_node().is_some() || top.session.borrow().document().target_element().is_some() {
        mark_processed(&top); return Ok(());
    }
    loop {
        let candidate=top.autofocus.borrow().candidates.first().cloned();
        let Some((owner,node,_))=candidate else { return Ok(()); };
        if let Some(owner)=owner.upgrade().filter(|owner|active(owner) && top_document(owner).is_some_and(|actual|Rc::ptr_eq(&actual,&top))) {
            if !owner.autofocus.borrow().blocking_stylesheets.is_empty() { return Ok(()); }
            top.autofocus.borrow_mut().candidates.remove(0);
            let mut context=owner.navigation_context(); let mut has_fragment=false;
            while let Some(current)=context {
                if current.current_document().is_some_and(|document|document.session.borrow().document().target_element().is_some()) {has_fragment=true;break;}
                context=current.parent_navigation_context();
            }
            if has_fragment || !owner.session.borrow().document().is_connected_element(node) { continue; }
            if let Some(target)=resolve_focus_target(&owner,node,false)? {
                mark_processed(&top);
                let handle=owner.child_realm_handle()?.unwrap_or_else(||ctx.current_host_realm());
                ctx.with_host_realm(&handle,|ctx|owner.focus(ctx,Some(target)))
                    .map_err(|error|OpError::new("InvalidStateError",error.to_string()))??;
                return Ok(());
            }
        } else { top.autofocus.borrow_mut().candidates.remove(0); }
    }
}

pub struct ScriptBlockingStylesheet { owner: std::rc::Weak<DomRealm>, node: NodeId }
impl Drop for ScriptBlockingStylesheet {
    fn drop(&mut self) {
        if let Some(owner)=self.owner.upgrade() {
            let mut state=owner.autofocus.borrow_mut();
            if let Some(count)=state.blocking_stylesheets.get_mut(&self.node) {
                *count-=1;
                if *count==0 {state.blocking_stylesheets.remove(&self.node);}
            }
        }
    }
}
impl DomRealm {
    pub fn script_blocking_stylesheets_pending(&self)->bool {
        !self.autofocus.borrow().blocking_stylesheets.is_empty()
    }

    /// Resource controllers hold this weak lease while a real script-blocking
    /// stylesheet is loading. Overlapping requests cannot release each other.
    pub fn begin_script_blocking_stylesheet(self: &Rc<Self>, node: NodeId) -> OpResult<ScriptBlockingStylesheet> {
        let mut state=self.autofocus.borrow_mut();
        state.blocking_stylesheets.try_reserve(1).map_err(|_|OpError::new("QuotaExceededError","blocking stylesheet admission"))?;
        let count=state.blocking_stylesheets.entry(node).or_default();
        *count=count.checked_add(1).ok_or_else(||OpError::new("QuotaExceededError","blocking stylesheet admission"))?;
        Ok(ScriptBlockingStylesheet {owner:Rc::downgrade(self),node})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;
    fn check(engine: &mut Engine, source: &str) {
        match engine.eval_value(source) {
            Ok(Ok(Value::Bool(true)))=>{},
            Ok(Err(error))=>match engine.describe_throw(error) {
                lumen::Completion::Throw {name,message}=>panic!("focus guard exception: {name}: {message}"),
                lumen::Completion::Value(message)=>panic!("focus guard exception: {message}"),
            },
            _=>panic!("focus guard did not return true"),
        }
    }
    #[test]
    fn specification_window_focus_options_namespace_mixins_and_scroll_are_shared() {
        struct NoText;
        impl lumen_html::paint::TextShaper for NoText {
            fn shape(&self,_:&str,_:f32)->Result<lumen_html::paint::ShapedRun,()> { Err(()) }
            fn ascent(&self,size:f32)->f32 { size*0.8 }
            fn line_height(&self,size:f32)->f32 { size*1.2 }
        }
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<!doctype html><body style='margin:0'><button id=first></button><div style='height:500px'></div><button id=last style='display:block;height:20px'></button><svg><a id=svg></a></svg><math><mi id=math></mi></math>",256).unwrap();
        realm.set_layout_flusher(Rc::new(|session|session.display_list(100,80,&NoText).map(|_|()).map_err(|error|format!("{error:?}"))));
        check(&mut engine,r#"globalThis.first=document.getElementById('first');globalThis.last=document.getElementById('last');
            first.focus({preventScroll:true});const reads=[];
            try{last.focus({get focusVisible(){reads.push('visible');return true},get preventScroll(){reads.push('scroll');throw Error('stop')}})}catch(e){if(e.message!=='stop')throw e}
            if(reads.join(',')!=='visible,scroll'||document.activeElement!==first)throw Error('conversion before focus');
            last.focus({preventScroll:true,focusVisible:true});if(scrollY!==0||!last.matches(':focus-visible'))throw Error('preventScroll/indication');
            last.focus();if(scrollY<=0)throw Error('center scrolling');
            const svg=document.getElementById('svg'), math=document.getElementById('math');
            if(svg.tabIndex!==0||math.tabIndex!==-1)throw Error('namespace defaults');
            svg.tabIndex=0;math.tabIndex=0;svg.autofocus=true;math.autofocus=true;
            if(!svg.hasAttribute('autofocus')||!math.hasAttribute('autofocus'))throw Error('namespace reflection');
            svg.focus({preventScroll:true});if(document.activeElement!==svg)throw Error('SVG focus');
            math.focus({preventScroll:true});if(document.activeElement!==math)throw Error('MathML focus');
            math.blur();if(document.activeElement===math)throw Error('MathML blur');true"#);
    }
    #[test]
    fn specification_window_mutable_internal_slots_trace_replacement_after_object_freeze() {
        let mut engine=Engine::new();
        let owner=engine.ctx().plain_object(&[]);
        let first=engine.ctx().plain_object(&[]);
        let second=engine.ctx().plain_object(&[]);
        let first_weak=engine.ctx().weak_value(&first).expect("object weak identity");
        let second_weak=engine.ctx().weak_value(&second).expect("object weak identity");
        let key=engine.ctx().allocate_native_private_slot_name();
        assert!(engine.ctx().define_native_internal_value_slot(&owner,&key,first.clone()).is_ok());
        engine.ctx().freeze_native_object(&owner);
        assert!(engine.ctx().set_native_internal_value_slot(&owner,&key,second.clone()).is_ok());
        assert!(engine.ctx().define_native_private_value_slot(&owner,&key,Value::Null).is_err(),"authored readonly association initializer remains readonly");
        assert!(engine.ctx().set_native_internal_value_slot(&owner,"ordinary",Value::Null).is_err());
        drop(first);drop(second);engine.collect_garbage();
        assert!(first_weak.upgrade().is_none(),"replaced internal edge must release its old object");
        assert!(second_weak.upgrade().is_some(),"current slot is an ordinary traced object edge");
        assert!(engine.ctx().set_native_internal_value_slot(&owner,&key,Value::Null).is_ok());
        engine.collect_garbage();
        assert!(second_weak.upgrade().is_none(),"clearing mutable state releases its edge");
    }
    #[test]
    fn specification_window_focus_uses_actual_disabled_face_and_shadow_scope_order() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<!doctype html><button id=first tabindex=1></button><div id=host tabindex=0></div><button id=after tabindex=3></button>",256).unwrap();
        check(&mut engine,r#"globalThis.host=document.getElementById('host');globalThis.shadow=host.attachShadow({mode:'open'});
            shadow.innerHTML='<button id=late tabindex=2></button><button id=early tabindex=" +01tail"></button>';
            document.getElementById('first').focus();true"#);
        realm.focus_next(engine.ctx(),false).unwrap();
        check(&mut engine,"if(document.activeElement.id!=='after')throw Error('nested positive tabindex escaped its scope');true");
        realm.focus_next(engine.ctx(),false).unwrap();
        check(&mut engine,"if(document.activeElement!==host)throw Error('host stop');true");
        realm.focus_next(engine.ctx(),false).unwrap();
        check(&mut engine,"if(document.activeElement!==host||shadow.activeElement.id!=='early')throw Error('local shadow scope sort');true");
        check(&mut engine,r#"class FocusFace extends HTMLElement {static formAssociated=true;constructor(){super();this.attachInternals();}}
            customElements.define('focus-face',FocusFace);
            globalThis.face=document.createElement('focus-face');face.tabIndex=0;document.body.append(face);
            face.focus();if(document.activeElement!==face)throw Error('enabled FACE');
            face.setAttribute('disabled','');document.getElementById('first').focus();face.focus();
            if(document.activeElement===face)throw Error('disabled FACE still focuses');
            const plain=document.createElement('div');plain.tabIndex=0;plain.setAttribute('disabled','');document.body.append(plain);plain.focus();
            if(document.activeElement!==plain)throw Error('arbitrary disabled attribute');
            const delegate=document.createElement('div');document.body.append(delegate);const root=delegate.attachShadow({mode:'open',delegatesFocus:true});
            root.innerHTML='<button id=ordinary></button><input id=preferred autofocus>';delegate.focus();
            if(document.activeElement!==delegate||root.activeElement.id!=='preferred')throw Error('delegated autofocus target');true"#);
    }
    #[test]
    fn specification_window_autofocus_preserves_best_candidate_stylesheet_wait_and_fragment_priority() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<!doctype html><link id=sheet rel=stylesheet><input id=first autofocus><input id=second autofocus>",128).unwrap();
        let sheet=realm.with_session(|session|selector::get_element_by_id(session.document(),session.document().root(),"sheet").unwrap().unwrap());
        let lease=realm.begin_script_blocking_stylesheet(sheet).unwrap();
        let overlapping=realm.begin_script_blocking_stylesheet(sheet).unwrap();
        realm.update_rendered_focus(engine.ctx()).unwrap();
        check(&mut engine,"if(document.activeElement.id==='first')throw Error('autofocus ran before its stylesheet completed');true");
        drop(lease);realm.update_rendered_focus(engine.ctx()).unwrap();
        check(&mut engine,"if(document.activeElement.id==='first')throw Error('overlapping load lost its blocker');true");
        drop(overlapping);realm.update_rendered_focus(engine.ctx()).unwrap();
        check(&mut engine,"if(document.activeElement.id!=='first')throw Error('best candidate lost while waiting');true");
        check(&mut engine,"document.activeElement.blur();document.body.insertAdjacentHTML('beforeend','<input id=late autofocus>');true");
        realm.update_rendered_focus(engine.ctx()).unwrap();
        check(&mut engine,"if(document.activeElement.id==='late')throw Error('autofocus processed flag reset');true");
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<!doctype html><input id=auto autofocus><div id=target tabindex=0></div>",64).unwrap();
        let target=realm.with_session(|session|selector::get_element_by_id(session.document(),session.document().root(),"target").unwrap().unwrap());
        realm.with_session(|session|session.document_mut().set_target_element(Some(target)).unwrap());
        realm.update_rendered_focus(engine.ctx()).unwrap();
        check(&mut engine,"if(document.activeElement.id==='auto')throw Error('autofocus overrode fragment targeting');true");
    }
}
