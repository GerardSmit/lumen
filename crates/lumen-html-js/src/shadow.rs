use super::*;

#[lumen_bind::class(name = "ShadowRoot", extends = DomDocumentFragment, hint(js(webidl)))]
pub(crate) struct DomShadowRoot {
    pub(crate) base: DomDocumentFragment,
}
#[lumen_bind::methods]
impl DomShadowRoot {
    fn get_animations(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let node = &self.base.base;
        animations::for_tree_root(ctx, &node.realm, node.id)
    }

    #[getter]
    fn adopted_style_sheets(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let node = &self.base.base;
        cssom::adopted_stylesheets(ctx, &node.realm, Some(node.id))
    }
    #[setter]
    fn set_adopted_style_sheets(&self, ctx: &mut Ctx, value: Value) -> OpResult<()> {
        let node = &self.base.base;
        cssom::set_adopted_stylesheets(ctx, &node.realm, Some(node.id), value)
    }
    #[getter]
    fn host(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let node = &self.base.base;
        let host = node
            .realm
            .session
            .borrow()
            .document()
            .shadow_host(node.id)
            .map_err(dom_error)?;
        Ok(node.realm.wrap_option(ctx, host))
    }
    #[getter]
    fn mode(&self) -> OpResult<String> {
        let node = &self.base.base;
        Ok(match node
            .realm
            .session
            .borrow()
            .document()
            .shadow_mode(node.id)
            .map_err(dom_error)?
        {
            Some(lumen_html::ShadowMode::Open) => "open",
            _ => "closed",
        }
        .into())
    }
    #[getter]
    fn active_element(&self, ctx: &mut Ctx) -> OpResult<Value> {
        let node = &self.base.base;
        let Some(focused) = node.realm.focused_node() else {
            return Ok(Value::Null);
        };
        let target = node
            .realm
            .session
            .borrow()
            .document()
            .retarget(focused, Some(node.id))
            .map_err(dom_error)?;
        let root = node
            .realm
            .session
            .borrow()
            .document()
            .root_node(target, false)
            .map_err(dom_error)?;
        Ok(node
            .realm
            .wrap_option(ctx, (root == node.id).then_some(target)))
    }
    fn get_element_by_id(&self, ctx: &mut Ctx, id: &str) -> OpResult<Value> {
        let node = &self.base.base;
        let found = element_by_id_in(node.realm.session.borrow().document(), node.id, id)
            .map_err(dom_error)?;
        Ok(node.realm.wrap_option(ctx, found))
    }
}

#[lumen_bind::class(name = "HTMLSlotElement", extends = DomHtmlElement, hint(js(webidl)))]
pub(crate) struct DomSlotElement {
    pub(crate) base: DomHtmlElement,
}
#[lumen_bind::methods]
impl DomSlotElement {
    #[getter]
    fn name(&self) -> OpResult<String> {
        Ok(self
            .base
            .base
            .base
            .get_attribute("name")?
            .unwrap_or_default())
    }
    #[setter]
    fn set_name(&self, value: &str) -> OpResult<()> {
        self.base.base.base.set_attribute_core("name", value)
    }
    fn assigned_nodes(&self, ctx: &mut Ctx, options: Option<Value>) -> OpResult<Value> {
        self.assigned(ctx, options, false)
    }
    fn assigned_elements(&self, ctx: &mut Ctx, options: Option<Value>) -> OpResult<Value> {
        self.assigned(ctx, options, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;
    fn check(engine: &mut Engine, source: &str) {
        match engine.eval_value(source).unwrap() {
            Ok(Value::Bool(true)) => (),
            Err(error) => {
                if let Ok(Value::Str(message)) = engine.ctx().get_member(&error, "stack") {
                    panic!("{message}");
                }
                panic!("shadow script threw");
            }
            _ => panic!("shadow assertion failed: {source}"),
        }
    }
    #[test]
    fn shadow_roots_slots_and_focus_keep_tree_identity() {
        let mut engine = Engine::new();
        let _realm = install(
            engine.ctx(),
            "<div id=host><b slot=named>A</b>text</div><div id=closed></div>",
            128,
        )
        .unwrap();
        check(
            &mut engine,
            "var host=document.getElementById('host'),root=host.attachShadow({mode:'open'}); root.innerHTML='<slot name=named></slot><slot></slot><input id=field>'; var slot=root.querySelector('slot'); root instanceof ShadowRoot && root instanceof DocumentFragment && root.host===host && root.mode==='open' && host.shadowRoot===root && root.parentNode===null && root.getRootNode()===root && root.getRootNode({composed:true})===document && root.isConnected && document.querySelector('slot')===null && slot instanceof HTMLSlotElement",
        );
        check(
            &mut engine,
            "var light=host.querySelector('b'); var snapshot=slot.assignedNodes(); Array.isArray(snapshot) && snapshot.length===1 && snapshot[0]===light && light.assignedSlot===slot && light.parentNode===host && slot.assignedElements()[0]===light",
        );
        check(
            &mut engine,
            "light.slot=''; slot.assignedNodes().length===0 && snapshot[0]===light && light.assignedSlot===root.querySelectorAll('slot')[1] && root.querySelectorAll('slot')[1].assignedNodes().length===2",
        );
        check(
            &mut engine,
            "slot.innerHTML='<slot name=unused><em>fallback</em></slot>'; slot.assignedNodes().length===0 && slot.assignedElements({flatten:true})[0].localName==='em'",
        );
        check(
            &mut engine,
            "var field=root.getElementById('field'); field.focus(); root.activeElement===field && document.activeElement===host && field.isConnected",
        );
        check(
            &mut engine,
            "var closed=document.getElementById('closed'),secret=closed.attachShadow({mode:'closed'}); secret.innerHTML='<slot></slot>'; closed.appendChild(document.createElement('b')); closed.shadowRoot===null && secret.mode==='closed' && closed.firstChild.assignedSlot===null && secret.querySelector('slot').assignedNodes()[0]===closed.firstChild",
        );
        check(
            &mut engine,
            "var duplicate=false,invalid=false; try{host.attachShadow({mode:'open'})}catch(e){duplicate=e.name==='NotSupportedError'} try{host.attachShadow({mode:'bad'})}catch(e){invalid=e.name==='TypeError'} duplicate && invalid",
        );
    }
    #[test]
    fn composed_events_retarget_and_hide_closed_paths() {
        let mut engine = Engine::new();
        let _realm = install(engine.ctx(), "<div id=host></div>", 128).unwrap();
        check(
            &mut engine,
            "var host=document.getElementById('host'), root=host.attachShadow({mode:'closed'});root.innerHTML='<button></button>';var button=root.firstChild, calls=[]; root.addEventListener('test',e=>{calls.push('root:'+e.eventPhase);if(e.target!==button || e.composedPath()[0]!==button)throw Error('internal retarget')});host.addEventListener('test',e=>{calls.push('host:'+e.eventPhase);if(e.target!==host || e.composedPath().includes(root) || e.composedPath().includes(button))throw Error('closed path leaked')});document.addEventListener('test',e=>{calls.push('doc:'+e.eventPhase);if(e.target!==host || e.composedPath()[0]!==host)throw Error('external retarget')});var e=new Event('test',{bubbles:true});button.dispatchEvent(e); calls.join(',')==='root:3' && e.target===null && e.composedPath().length===0",
        );
        check(
            &mut engine,
            "calls=[];var e=new Event('test',{bubbles:true,composed:true});button.dispatchEvent(e); e.composed && calls.join(',')==='root:3,host:2,doc:3' && e.target===host && e.composedPath().length===0",
        );
        check(
            &mut engine,
            "calls=[];button.dispatchEvent(new Event('test',{composed:true})); calls.join(',')==='host:2'",
        );
    }
    #[test]
    fn noncomposed_light_events_follow_slots_and_escape_the_shadow_root() {
        let mut engine = Engine::new();
        let _realm = install(engine.ctx(), "<div id=host><button></button></div>", 64).unwrap();
        check(
            &mut engine,
            "var host=document.getElementById('host'),light=host.firstChild,root=host.attachShadow({mode:'open'});root.innerHTML='<slot></slot>';var slot=root.firstChild,calls=[];slot.addEventListener('test',e=>{calls.push('slot');if(e.target!==light)throw Error('slotted target')});root.addEventListener('test',()=>calls.push('root'));host.addEventListener('test',()=>calls.push('host'));light.dispatchEvent(new Event('test',{bubbles:true}));calls.join(',')==='slot,root,host'",
        );
    }
    #[test]
    fn focus_related_targets_do_not_leak_or_cross_between_shadow_siblings() {
        let mut engine = Engine::new();
        let _realm = install(engine.ctx(), "<div id=host></div><input id=outside>", 64).unwrap();
        check(
            &mut engine,
            "var host=document.getElementById('host'),root=host.attachShadow({mode:'closed'}); root.innerHTML='<input id=a><input id=b>'; var a=root.firstChild,b=root.lastChild,outer=[],inner=[];host.addEventListener('focusin',e=>{outer.push('in');if(e.target!==host)throw Error('focus retarget')});host.addEventListener('focusout',e=>{outer.push('out');if(e.target!==host || e.relatedTarget!==document.getElementById('outside'))throw Error('related retarget')});root.addEventListener('focusin',e=>{if(e.target===b)inner.push(e.relatedTarget===a)});a.focus();b.focus();document.activeElement===host && root.activeElement===b && outer.join(',')==='in' && inner.join(',')==='true'",
        );
        check(
            &mut engine,
            "document.getElementById('outside').focus();outer.join(',')==='in,out' && root.activeElement===null",
        );
    }

    #[test]
    fn slotchange_coalesces_assignments_after_observers_and_stays_in_shadow() {
        let mut engine = Engine::new();
        let _realm = install(engine.ctx(), "<div id=host><b>A</b></div>", 128).unwrap();
        check(
            &mut engine,
            "var host=document.getElementById('host'),root=host.attachShadow({mode:'closed'});root.innerHTML='<slot></slot><slot name=named></slot>';var slot=root.firstChild,named=root.lastChild,light=host.firstChild,order=[],escaped=0;slot.addEventListener('slotchange',e=>{order.push('slot');if(e.target!==slot || !e.bubbles || e.composed || e.cancelable)throw Error('flags')});root.addEventListener('slotchange',()=>order.push('root'));host.addEventListener('slotchange',()=>escaped++);order.length===0",
        );
        engine.ctx().drain_microtasks_for_host();
        check(
            &mut engine,
            "if(order.join(',')!=='slot,root' || escaped!==0)throw Error(JSON.stringify({order,escaped}));true",
        );
        check(
            &mut engine,
            "order=[];var observer=new MutationObserver(()=>order.push('observer'));observer.observe(host,{subtree:true,attributes:true});light.slot='named';light.slot='';light.slot='named';Promise.resolve().then(()=>order.push('promise'));order.length===0",
        );
        engine.collect_garbage();
        engine.ctx().drain_microtasks_for_host();
        check(
            &mut engine,
            "order.join(',')==='observer,slot,root,root,promise' && escaped===0 && named.assignedNodes()[0]===light",
        );
        check(
            &mut engine,
            "observer.disconnect();order=[];light.firstChild.data='B';slot.innerHTML='<i>fallback</i>';true",
        );
        engine.ctx().drain_microtasks_for_host();
        check(&mut engine, "order.join(',')==='slot,root'");
        check(
            &mut engine,
            "order=[];named.innerHTML='<i>hidden fallback</i>';true",
        );
        engine.ctx().drain_microtasks_for_host();
        check(&mut engine, "order.length===0");
    }

    #[test]
    fn slotchange_reentrant_mutations_and_removed_slots_are_delivered() {
        let mut engine = Engine::new();
        let _realm = install(engine.ctx(), "<div id=host><b></b></div>", 128).unwrap();
        check(
            &mut engine,
            "var host=document.getElementById('host'),root=host.attachShadow({mode:'open'});root.innerHTML='<slot></slot>';var slot=root.firstChild,calls=0;slot.addEventListener('slotchange',()=>{calls++;if(calls===1)host.appendChild(document.createTextNode('more'))});calls===0",
        );
        engine.ctx().drain_microtasks_for_host();
        check(
            &mut engine,
            "if(calls!==2 || slot.assignedNodes().length!==2)throw Error('calls='+calls+' assigned='+slot.assignedNodes().length);true",
        );
        check(&mut engine, "root.removeChild(slot);calls===2");
        engine.collect_garbage();
        engine.ctx().drain_microtasks_for_host();
        check(&mut engine, "calls===3 && slot.assignedNodes().length===0");
    }
}
impl DomSlotElement {
    fn assigned(&self, ctx: &mut Ctx, options: Option<Value>, elements: bool) -> OpResult<Value> {
        let flatten = match options {
            Some(value) => matches!(
                ctx.get_member(&value, "flatten")
                    .map_err(|_| OpError::new("TypeError", "slot options getter failed"))?,
                Value::Bool(true)
            ),
            None => false,
        };
        let node = &self.base.base.base;
        let mut assigned = node
            .realm
            .session
            .borrow()
            .document()
            .assigned_nodes(node.id, flatten)
            .map_err(dom_error)?;
        if elements {
            assigned.retain(|id| {
                matches!(
                    node.realm.session.borrow().document().kind(*id),
                    Ok(NodeKind::Element { .. })
                )
            });
        }
        let values = assigned
            .into_iter()
            .map(|id| node.realm.wrap(ctx, id))
            .collect();
        Ok(ctx.make_array(values))
    }
}
