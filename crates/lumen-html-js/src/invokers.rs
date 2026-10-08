//! HTML button commands and button/input popover target activation.
use super::*;
use lumen_html::{invokers::{self, ButtonState, Command, PopoverAction}, top_layer};

pub(crate) fn reflected_popover_action(node: &DomNode) -> OpResult<String> {
    Ok(match PopoverAction::parse(&node.get_null_attribute("popovertargetaction")?.unwrap_or_default()) {
        PopoverAction::Toggle => "toggle", PopoverAction::Show => "show", PopoverAction::Hide => "hide",
    }.to_owned())
}

fn element_identity(ctx: &mut Ctx, value: &Value) -> OpResult<(Rc<DomRealm>, NodeId)> {
    ctx.with_instance::<DomElement, _>(value, |element| element.base.realm.resolve_adopted_node(element.base.id))
        .map_err(|_| OpError::type_error("invoker must be an Element"))
}

fn associated(ctx: &mut Ctx, realm: &Rc<DomRealm>, node: NodeId, attribute: &'static str) -> OpResult<Value> {
    let result = realm.reflected_elements.borrow().get(ctx, realm, node, attribute);
    result
}

pub(crate) fn button_activation(ctx: &mut Ctx, source: Value, event_target: Value) -> OpResult<()> {
    let (realm, node) = element_identity(ctx, &source)?;
    let (form, state, submit) = {
        let session = realm.session.borrow();
        let document = session.document();
        if lumen_html::forms::is_disabled(document, node) { return Ok(()); }
        (lumen_html::forms::form_owner(document, node), invokers::button_state(document, node), invokers::is_submit_button(document, node))
    };
    if !realm.browsing_context().is_some_and(|context| browsing_context::is_active_document(&context, &realm)) { return Ok(()); }
    if let Some(form) = form {
        if submit { realm.submit_form(ctx, form, Some(node))?; return Ok(()); }
        if state == ButtonState::Reset { forms::reset_form(ctx, &realm, form, &realm.forms)?; return Ok(()); }
        if state == ButtonState::Auto { return Ok(()); }
    }
    let target = associated(ctx, &realm, node, "commandfor")?;
    if matches!(target, Value::Null) { return popover_activation(ctx, source, event_target); }
    let (target_realm, target_node) = element_identity(ctx, &target)?;
    let command = realm.session.borrow().document().get_attribute_ns_ref(node, None, "command")
        .map_err(dom_error)?.map(Command::reflected).unwrap_or("").to_owned();
    let state = Command::parse(&command);
    if !state.valid_for(target_realm.session.borrow().document(), target_node) { return Ok(()); }
    if !command_events::dispatch(ctx, &target_realm, target_node, &command, source.clone())? { return Ok(()); }
    // The exact wrapper follows ordinary adoption during the command handler.
    let (target_realm, target_node) = element_identity(ctx, &target)?;
    if !target_realm.session.borrow().document().is_connected_element(target_node) { return Ok(()); }
    if state == Command::Custom { return Ok(()); }
    match state {
        Command::HidePopover | Command::ShowPopover | Command::TogglePopover => {
            let action = match state { Command::HidePopover => PopoverAction::Hide, Command::ShowPopover => PopoverAction::Show, _ => PopoverAction::Toggle };
            run_popover_action(ctx, &target_realm, target_node, action, source)?;
        }
        Command::Close | Command::RequestClose | Command::ShowModal => {
            let session = target_realm.session.borrow();
            let document = session.document();
            if top_layer::popover_visibility(document, target_node) == top_layer::PopoverVisibility::Showing { return Ok(()); }
            let open = document.get_attribute_ns_ref(target_node, None, "open").map_err(dom_error)?.is_some();
            drop(session);
            let (source_realm, source_node) = element_identity(ctx, &source)?;
            let value = source_realm.session.borrow().document().get_attribute_ns(source_node, None, "value").map_err(dom_error)?;
            match state {
                Command::Close if open => dialog_popover::close_dialog_with_source(ctx, &target_realm, target_node, value.as_deref(), source)?,
                Command::RequestClose if open => dialog_popover::request_close_dialog(ctx, &target_realm, target_node, value.as_deref(), source)?,
                Command::ShowModal if !open => dialog_popover::show_dialog_with_source(ctx, &target_realm, target_node, top_layer::DialogMode::Modal, source)?,
                _ => {},
            }
        }
        _ => {},
    }
    Ok(())
}

pub(crate) fn input_popover_activation(ctx: &mut Ctx, source: Value, event_target: Value) -> OpResult<()> {
    let (realm, node) = element_identity(ctx, &source)?;
    let session = realm.session.borrow();
    let document = session.document();
    if lumen_html::forms::form_owner(document, node).is_some()
        && !document.get_attribute_ns_ref(node, None, "type").map_err(dom_error)?
            .is_some_and(|value| value.eq_ignore_ascii_case("button")) { return Ok(()); }
    drop(session);
    popover_activation(ctx, source, event_target)
}

pub(crate) fn popover_activation(ctx: &mut Ctx, source: Value, event_target: Value) -> OpResult<()> {
    let (realm, node) = element_identity(ctx, &source)?;
    let action = {
        let session = realm.session.borrow();
        let document = session.document();
        if !invokers::is_button(document, node) || lumen_html::forms::is_disabled(document, node)
            || (lumen_html::forms::form_owner(document, node).is_some() && invokers::is_submit_button(document, node)) { return Ok(()); }
        PopoverAction::parse(document.get_attribute_ns_ref(node, None, "popovertargetaction").map_err(dom_error)?.unwrap_or(""))
    };
    let target = associated(ctx, &realm, node, "popovertarget")?;
    if matches!(target, Value::Null) { return Ok(()); }
    let (target_realm, target_node) = element_identity(ctx, &target)?;
    let event_target = element_identity(ctx, &event_target).ok();
    {
        let session = target_realm.session.borrow();
        let document = session.document();
        if top_layer::popover_mode(document, target_node).is_none() { return Ok(()); }
        if Rc::ptr_eq(&realm, &target_realm) {
            if let Some((event_realm, event_node)) = event_target {
                if Rc::ptr_eq(&event_realm, &target_realm)
                    && shadow_including_descendant(document, event_node, target_node, true)?
                    && shadow_including_descendant(document, target_node, node, false)? { return Ok(()); }
            }
        }
    }
    run_popover_action(ctx, &target_realm, target_node, action, source)
}

fn shadow_including_descendant(document: &lumen_html::Document, node: NodeId, ancestor: NodeId, inclusive: bool) -> OpResult<bool> {
    if inclusive && node == ancestor { return Ok(true); }
    let mut parent = document.shadow_including_parent(node).map_err(dom_error)?;
    while let Some(node) = parent {
        if node == ancestor { return Ok(true); }
        parent = document.shadow_including_parent(node).map_err(dom_error)?;
    }
    Ok(false)
}

fn run_popover_action(ctx: &mut Ctx, realm: &Rc<DomRealm>, node: NodeId, action: PopoverAction, source: Value) -> OpResult<()> {
    let showing = top_layer::popover_visibility(realm.session.borrow().document(), node) == top_layer::PopoverVisibility::Showing;
    if (showing && action == PopoverAction::Show) || (!showing && action == PopoverAction::Hide) { return Ok(()); }
    if showing {
        let valid = top_layer::validate_hide_popover(realm.session.borrow().document(), node).unwrap_or(false);
        if valid { dialog_popover::hide_popover_with_source(ctx, realm, node, source)?; }
    } else {
        let valid = top_layer::validate_show_popover(realm.session.borrow().document(), node).is_ok();
        if valid { dialog_popover::show_popover_with_source(ctx, realm, node, source)?; }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn check(source: &str) {
        let mut engine = lumen::Engine::new();
        let _realm = crate::install(engine.ctx(), "<!doctype html><body></body>", 512).unwrap();
        match engine.eval_value(source).unwrap() {
            Ok(Value::Bool(true)) => {},
            Ok(_) => panic!("invoker guard did not return true"),
            Err(error) => {
                let text=engine.ctx().coerce_string(&error).ok().map(|s|s.to_string()).unwrap_or_default();
                panic!("invoker guard threw: {text}");
            }
        }
    }
    #[test]
    fn specification_window_command_event_cancellation_capture_and_retarged_source() {
        check(r#"(() => {
            const b=document.createElement('button'),d=document.createElement('dialog');document.body.append(b,d);
            b.commandForElement=d;b.command='--Custom';let calls=0;
            d.oncommand=e=>{if(!(e instanceof CommandEvent)||!e.isTrusted||!e.cancelable||e.bubbles||e.composed||e.command!=='--Custom'||e.source!==b)throw Error('command metadata');calls++;};
            b.click();if(calls!==1||d.open||b.type!=='button')throw Error('auto command dispatch');
            b.command='show-modal';d.oncommand=e=>{e.preventDefault();};b.click();if(d.open)throw Error('canceled command activated');
            d.oncommand=e=>{b.command='close';};b.click();if(!d.open||!d.matches(':modal'))throw Error('command must be captured before dispatch');d.close();
            let order=[];const authored=new CommandEvent('command',new Proxy({source:b,command:'--x'},{get(t,k){order.push(k);return t[k];}}));
            if(order.join(',')!=='bubbles,cancelable,composed,command,source'||authored.isTrusted||authored.command!=='--x'||authored.source!==b)throw Error('WebIDL conversion order');
            const host=document.createElement('div');document.body.append(host);const shadow=host.attachShadow({mode:'open'});const inner=document.createElement('button');shadow.append(inner);
            const event=new CommandEvent('command',{source:inner,bubbles:true,composed:true});let seen=false;
            host.addEventListener('command',e=>{seen=e.source===host;});inner.dispatchEvent(event);
            if(!seen||event.source!==host)throw Error('source retargeting must follow currentTarget');
            return true;
        })()"#);
    }
    #[test]
    fn specification_window_reflected_invoker_targets_scope_mutation_and_adoption() {
        check(r#"(() => {
            const owner=document.createElement('section'),b=document.createElement('button'),target=document.createElement('div');target.id='t';owner.append(b,target);document.body.append(owner);
            b.commandForElement=target;if(b.getAttribute('commandfor')!==''||b.commandForElement!==target)throw Error('explicit association');
            b.setAttribute('commandfor','');if(b.commandForElement!==null)throw Error('same-value content mutation clears explicit reference');
            b.setAttribute('commandfor','t');if(b.commandForElement!==target)throw Error('tree-order ID fallback');
            b.commandForElement=target;const other=document.implementation.createHTMLDocument('other');other.adoptNode(owner);other.body.append(owner);
            if(b.commandForElement!==target||b.ownerDocument!==other)throw Error('explicit association follows adoption');
            document.adoptNode(owner);document.body.append(owner);if(b.commandForElement!==target)throw Error('roundtrip association');
            const host=document.createElement('div');document.body.append(host);const shadow=host.attachShadow({mode:'open'});shadow.append(b);
            b.commandForElement=target;if(b.commandForElement!==target)throw Error('explicit reference permits ancestor tree');
            b.setAttribute('commandfor','t');if(b.commandForElement!==null)throw Error('content reference cannot cross shadow root');
            b.commandForElement=null;if(b.hasAttribute('commandfor'))throw Error('null deletes content attribute');
            return true;
        })()"#);
    }
    #[test]
    fn specification_window_dialog_request_close_reentrancy_and_invoker_toggle_source() {
        check(r#"(() => {
            const b=document.createElement('button'),d=document.createElement('dialog');document.body.append(b,d);
            b.commandForElement=d;b.command='show-modal';let sources=[];d.onbeforetoggle=e=>sources.push(e.source);b.click();
            if(!d.open||sources[0]!==b)throw Error('show source');
            b.command='request-close';b.value='outer';let cancels=0;
            d.oncancel=e=>{cancels++;d.requestClose('nested');e.preventDefault();};b.click();
            if(!d.open||cancels!==1)throw Error('cancel and reentrant request arbitration');
            d.oncancel=e=>{cancels++;d.requestClose('nested');};b.click();
            if(d.open||d.returnValue!=='nested'||cancels!==2||sources[sources.length-1]!==null)throw Error('latest request return/source');
            d.show();d.closedBy='none';d.oncancel=null;d.requestClose('direct');if(d.open||d.returnValue!=='direct')throw Error('requestClose ignores closedBy');
            return true;
        })()"#);
    }
    #[test]
    fn specification_window_popover_invokers_source_precedence_and_input_default_action() {
        check(r#"(() => {
            const b=document.createElement('button'),p=document.createElement('div'),input=document.createElement('input');
            p.popover='manual';input.type='button';document.body.append(b,p,input);b.popoverTargetElement=p;
            let source=null;p.onbeforetoggle=e=>{source=e.source;};b.click();
            if(!p.matches(':popover-open')||source!==b)throw Error('button popover source');
            b.click();if(p.matches(':popover-open')||source!==b)throw Error('button toggle hide');
            input.popoverTargetElement=p;input.popoverTargetAction='SHOW';input.click();
            if(!p.matches(':popover-open')||source!==input||input.popoverTargetAction!=='show')throw Error('input popover action/source');p.hidePopover();
            b.commandForElement=p;b.command='invalid';b.click();if(p.matches(':popover-open'))throw Error('command target takes priority even for invalid command');
            b.command='show-popover';p.oncommand=e=>{e.preventDefault();};b.click();if(p.matches(':popover-open'))throw Error('canceled popover command');
            p.oncommand=null;b.click();if(!p.matches(':popover-open')||source!==b)throw Error('popover command transition');
            p.hidePopover();p.oncommand=()=>p.remove();b.click();if(p.matches(':popover-open'))throw Error('disconnected command target must not activate');
            return true;
        })()"#);
    }
}
