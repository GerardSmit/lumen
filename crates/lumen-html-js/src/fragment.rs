//! HTML fragment selection and the shared reveal/scroll/focus continuation.
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Indicated { None, Top, Element(NodeId) }

pub(crate) struct FragmentState {
    target: Option<Value>,
    sequential_start: Option<NodeRetention>,
    generation: u64,
    pending: bool,
    queued: Option<u64>,
    ready: Rc<Cell<bool>>,
}

impl Default for FragmentState {
    fn default() -> Self { Self { target: None, sequential_start: None,
        generation: 0, pending: false, queued: None, ready: Rc::new(Cell::new(false)) } }
}

impl FragmentState {
    pub(crate) fn trace_values(&self, visit: &mut dyn FnMut(&Value)) {
        if let Some(target)=&self.target { visit(target); }
    }
}

pub(crate) fn sequential_focus_start(realm: &DomRealm) -> Option<NodeId> {
    let state=realm.fragment_state.borrow();
    let token=state.as_ref()?.sequential_start.as_ref()?;
    let original=token.realm.upgrade()?;
    let (owner,node)=original.resolve_adopted_node(token.node);
    core::ptr::eq(owner.as_ref(),realm).then_some(node)
}

pub(crate) fn clear_sequential_focus_start(realm: &DomRealm) {
    if let Some(state)=realm.fragment_state.borrow_mut().as_mut() { state.sequential_start=None; }
}

pub(crate) fn url_changed(realm: &DomRealm, initial: bool) {
    let has_fragment = realm.document_url().and_then(|url| lumen_common::url::parse(&url,None).ok())
        .is_some_and(|url| url.fragment.is_some());
    let mut state = realm.fragment_state.borrow_mut();
    if state.is_none() && !(initial && has_fragment) { return; }
    let state = state.get_or_insert_with(FragmentState::default);
    state.generation = state.generation.wrapping_add(1);
    state.pending = initial && has_fragment;
    scheduling::mark_task_ready(&state.ready);
}

pub(crate) fn mutation_checkpoint(realm: &DomRealm) {
    if let Some(state) = realm.fragment_state.borrow().as_ref().filter(|state| state.pending) {
        scheduling::mark_task_ready(&state.ready);
    }
}

fn potential(document: &lumen_html::Document, fragment: &str) -> OpResult<Option<NodeId>> {
    if let Some(node) = selector::get_element_by_id(document,document.root(),fragment).map_err(dom_error)? { return Ok(Some(node)); }
    let mut node = document.first_child(document.root()).map_err(dom_error)?;
    while let Some(current) = node {
        if lumen_html::forms::html_element_local_name(document,current)==Some("a") &&
            document.get_attribute_ns_ref(current,None,"name").map_err(dom_error)?==Some(fragment) { return Ok(Some(current)); }
        node = selector::next_descendant(document,document.root(),current).map_err(dom_error)?;
    }
    Ok(None)
}

fn indicated(realm: &DomRealm) -> OpResult<Indicated> {
    if !realm.is_html_document { return Ok(Indicated::None); }
    let Some(url) = realm.document_url().and_then(|url| lumen_common::url::parse(&url,None).ok()) else { return Ok(Indicated::None); };
    let Some(fragment) = url.fragment else { return Ok(Indicated::None); };
    if fragment.is_empty() { return Ok(Indicated::Top); }
    let session = realm.session.borrow();
    let document = session.document();
    if let Some(node) = potential(document,&fragment)? { return Ok(Indicated::Element(node)); }
    let bytes = lumen_common::codec::percent_decode(fragment.as_bytes());
    let decoded = String::from_utf8_lossy(&bytes);
    if let Some(node) = potential(document,&decoded)? { return Ok(Indicated::Element(node)); }
    Ok(if decoded.eq_ignore_ascii_case("top") { Indicated::Top } else { Indicated::None })
}

#[derive(Clone, Copy)]
enum Reveal { UntilFound(NodeId), Details(NodeId) }

fn until_found(document: &lumen_html::Document, node: NodeId) -> bool {
    lumen_html::forms::html_element_local_name(document,node).is_some() &&
        document.get_attribute_ns_ref(node,None,"hidden").ok().flatten()
            .is_some_and(|value| value.eq_ignore_ascii_case("until-found"))
}

fn reveal(ctx: &mut Ctx, realm: &Rc<DomRealm>, target: NodeId) -> OpResult<()> {
    let pairs = {
        let session = realm.session.borrow();
        let document = session.document();
        let mut pairs = Vec::new();
        let mut node = target;
        while let Some(parent) = document.flat_tree_parent(node).map_err(dom_error)? {
            let hidden = until_found(document,node);
            let details = document.details_content_parent(node);
            pairs.try_reserve(usize::from(hidden)+usize::from(details.is_some()))
                .map_err(|_| OpError::new("QuotaExceededError","fragment ancestor allocation"))?;
            if hidden { pairs.push(Reveal::UntilFound(node)); }
            if let Some(details) = details { pairs.push(Reveal::Details(details)); }
            node = parent;
        }
        pairs
    };
    // Author handlers may detach, adopt or collect any of the captured nodes.
    let mut leases=Vec::new();
    leases.try_reserve(pairs.len()).map_err(|_|OpError::new("QuotaExceededError","fragment ancestor retention allocation"))?;
    for pair in &pairs {
        let node=match pair { Reveal::UntilFound(node)|Reveal::Details(node)=>*node };
        leases.push(NodeRetention::new(realm,node));
    }
    for pair in pairs {
        let node = match pair { Reveal::UntilFound(node)|Reveal::Details(node)=>node };
        if !realm.session.borrow().document().is_connected_element(node) { return Ok(()); }
        match pair {
            Reveal::UntilFound(_) => {
                if !until_found(realm.session.borrow().document(),node) { return Ok(()); }
                realm.dispatch_user_agent(ctx,node,"beforematch",true,false,&[])?;
                let mut session = realm.session.borrow_mut();
                if !session.document().is_connected_element(node) || !until_found(session.document(),node) { return Ok(()); }
                session.document_mut().remove_attribute_ns(node,None,"hidden").map_err(dom_error)?;
            }
            Reveal::Details(_) => {
                let mut session = realm.session.borrow_mut();
                if session.document().details_open_state(node)!=Some(false) { return Ok(()); }
                session.document_mut().set_attribute_ns(node,None,"open","").map_err(dom_error)?;
            }
        }
    }
    Ok(())
}

fn scroll(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<Indicated> {
    let part = indicated(realm)?;
    let target = match part { Indicated::Element(node)=>Some(node), _=>None };
    let retention = target.map(|node|realm.wrap(ctx,node));
    {
        let mut session=realm.session.borrow_mut();
        session.document_mut().set_target_element(target).map_err(dom_error)?;
        if let Some(original_node)=target {
            let original=Rc::downgrade(realm);
            session.document_mut().set_target_element_resolver(Some(Rc::new(move |document| {
                let original=original.upgrade()?;
                let (owner,node)=original.resolve_adopted_node(original_node);
                (Rc::ptr_eq(&owner,&original) && document.kind(node).is_ok()).then_some(node)
            })));
        }
    }
    realm.fragment_state.borrow_mut().get_or_insert_with(FragmentState::default).target = retention;
    match part {
        Indicated::None=>{},
        Indicated::Top=>{
            let root = realm.session.borrow().document().root();
            scrolling::apply_offset(ctx,realm,root,0.0,0.0,scrolling::ScrollBehavior::Auto)?;
        },
        Indicated::Element(node)=>{
            let _lease = NodeRetention::new(realm,node);
            reveal(ctx,realm,node)?;
            // Re-read real layout after beforematch may have moved the target.
            let active=realm.browsing_context().is_none_or(|context|browsing_context::is_active_document(&context,realm));
            if active && realm.session.borrow().document().is_connected_element(node) {
                scrolling::into_view_inner(ctx,realm,node,scrolling::IntoViewOptions::default())?;
                if realm.focusable_node(node)? { realm.focus(ctx,Some(node))?; } else { realm.focus(ctx,None)?; }
                realm.fragment_state.borrow_mut().get_or_insert_with(FragmentState::default)
                    .sequential_start=Some(NodeRetention::new(realm,node));
            }
        },
    }
    Ok(part)
}

pub(crate) fn navigate(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    let generation = {
        let mut state = realm.fragment_state.borrow_mut();
        let state = state.get_or_insert_with(FragmentState::default);
        state.generation=state.generation.wrapping_add(1);
        state.pending=false;
        scheduling::mark_task_ready(&state.ready);
        state.generation
    };
    let part = scroll(ctx,realm)?;
    if part==Indicated::None && realm.has_live_document_parser()
        && realm.document_url().and_then(|url|lumen_common::url::parse(&url,None).ok())
            .is_some_and(|url|url.fragment.is_some()) {
        if let Some(state)=realm.fragment_state.borrow_mut().as_mut().filter(|state|state.generation==generation) { state.pending=true; }
        queue_retry(ctx,realm)?;
    }
    Ok(())
}

fn queue_retry(ctx: &mut Ctx, realm: &Rc<DomRealm>) -> OpResult<()> {
    let (generation,ready)={
        let mut state=realm.fragment_state.borrow_mut();
        let Some(state)=state.as_mut().filter(|state|state.pending && state.queued.is_none()) else { return Ok(()); };
        state.queued=Some(state.generation);
        (state.generation,state.ready.clone())
    };
    let document=Rc::downgrade(realm);
    if let Err(error)=scheduling::queue_navigation_when_ready(ctx,ready,move |ctx| {
        let Some(realm)=document.upgrade().filter(|realm|!realm.lifecycle.destroyed.get()) else { return Ok(()); };
        let run={
            let mut state=realm.fragment_state.borrow_mut();
            let Some(state)=state.as_mut() else { return Ok(()); };
            if state.queued==Some(generation) { state.queued=None; }
            state.pending && state.generation==generation
        };
        if run { checkpoint(ctx,&realm,false)?; }
        queue_retry(ctx,&realm)
    }) {
        if let Some(state)=realm.fragment_state.borrow_mut().as_mut().filter(|state|state.queued==Some(generation)) { state.queued=None; }
        return Err(error);
    }
    Ok(())
}

/// Parser/rendering opportunities awaken one existing task instead of polling
/// a paused parser with an unbounded sequence of immediately runnable tasks.
pub(crate) fn checkpoint(ctx: &mut Ctx, realm: &Rc<DomRealm>, final_parser_turn: bool) -> OpResult<()> {
    let generation={
        let state=realm.fragment_state.borrow();
        let Some(state)=state.as_ref().filter(|state|state.pending) else { return Ok(()); };
        state.generation
    };
    let part=scroll(ctx,realm)?;
    if let Some(state)=realm.fragment_state.borrow_mut().as_mut().filter(|state|state.generation==generation) {
        state.pending=part==Indicated::None && !final_parser_turn && realm.has_live_document_parser();
        if state.pending { state.ready.set(false); } else { scheduling::mark_task_ready(&state.ready); }
    }
    queue_retry(ctx,realm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen::Engine;
    struct NoText;
    impl lumen_html::paint::TextShaper for NoText {
        fn shape(&self,_:&str,_:f32)->Result<lumen_html::paint::ShapedRun,()> { Err(()) }
        fn ascent(&self,size:f32)->f32 { size*0.8 }
        fn line_height(&self,size:f32)->f32 { size*1.2 }
    }
    fn layout(realm:&Rc<DomRealm>) {
        realm.set_layout_flusher(Rc::new(|session|session.display_list(100,80,&NoText)
            .map(|_|()).map_err(|error|format!("{error:?}"))));
    }
    fn check(engine:&mut Engine,source:&str) {
        let value=engine.eval_value(source).unwrap().unwrap_or_else(|exception|{
            let description=engine.ctx().member_get(&exception,"stack").ok()
                .and_then(|value|engine.ctx().coerce_string(&value).ok()).map(|value|value.to_string()).unwrap_or_default();
            panic!("fragment guard: {description}");
        });
        assert!(matches!(value,Value::Bool(true)));
    }

    #[test]
    fn specification_window_rootless_document_rendering_runs_real_animation_frame() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<!doctype html><style>@keyframes anim{from{opacity:0}to{opacity:1}}div{animation:notfound 1s;width:20px;height:10px;background:green}</style><div></div>",96).unwrap();
        layout(&realm);realm.flush_layout().unwrap();
        check(&mut engine,"globalThis.ran=false;globalThis.removedRoot=document.documentElement;document.querySelector('style').remove();removedRoot.remove();requestAnimationFrame(()=>{removedRoot.classList.remove('test-wait');globalThis.ran=true});document.documentElement===null");
        realm.flush_layout().expect("a legitimate document without documentElement paints blank");
        let opportunity=lumen_host::perf::web_now_ms();
        let host_realm=engine.ctx().current_host_realm();
        assert!(realm.rendering_blocked_at(opportunity),"body-null document remains blocked before the UA deadline");
        assert!(crate::scheduling::run_animation_frame_in_realm_at(&mut engine,&host_realm,opportunity).is_empty());
        check(&mut engine,"!ran && document.documentElement===null");
        assert!(!realm.rendering_blocked_at(opportunity+10_000.),"UA deadline releases a rootless document");
        assert!(crate::scheduling::run_animation_frame_in_realm_at(&mut engine,&host_realm,opportunity+10_000.).is_empty(),"retained RAF runs at the next unblocked rendering opportunity");
        check(&mut engine,"ran && document.documentElement===null && document.getAnimations().length===0");
        check(&mut engine,"document.appendChild(removedRoot);document.documentElement===removedRoot");
        realm.flush_layout().expect("reattachment rebuilds actual root geometry");
    }

    #[test]
    fn specification_window_fragment_selection_target_style_focus_and_same_hash() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),r#"<!doctype html><style>
            html,body{margin:0}#space{height:600px}div:target{color:rgb(255,0,0)}
            #plain{position:absolute;top:120px;height:10px;width:10px}
            [id='raw%20id']{position:absolute;top:220px;height:10px;width:10px}
            [id='raw id']{position:absolute;top:320px;height:10px;width:10px}
            a{position:absolute;top:380px;height:10px;width:10px}
            #top{position:absolute;top:40px;height:10px;width:10px}
            </style><div id=space></div><div id=plain tabindex=0></div>
            <div id='raw%20id'></div><div id='raw id'></div><a name=legacy></a><div id=top></div>"#,128).unwrap();
        realm.set_document_url("https://fragment.test/page.html");
        layout(&realm);
        check(&mut engine,r#"(() => {
            const check=(ok,message)=>{if(!ok)throw new Error(message);};
            const plain=document.getElementById('plain');
            const originalURL=location.href;
            location.hash='';check(location.href===originalURL,'null old fragment compares as empty for Location hash bailout');
            location.hash='plain';
            check(document.activeElement===plain && window.scrollY===120,'real fragment focus and geometry');
            check(plain.matches(':target') && getComputedStyle(plain).color==='rgb(255, 0, 0)','shared target matcher and style');
            location.hash='raw%20id';
            const raw=document.getElementById('raw%20id');
            check(document.querySelector(':target')===raw && window.scrollY===220,'raw encoded id wins before decoding');
            check(document.activeElement===document.body,'unfocusable target must focus viewport fallback');
            scrollTo(0,0);location.hash='#raw%20id';
            check(window.scrollY===0,'same parsed hash must not navigate');
            location.href=location.href;
            check(window.scrollY===220,'same href still scrolls the indicated part');
            raw.id='other';location.hash='plain';location.hash='raw%20id';
            check(document.querySelector(':target').id==='raw id' && window.scrollY===320,'decoded id fallback');
            history.pushState(null,'','#unmatched');
            check(document.querySelector(':target').id==='raw id','pushState alone does not update target');
            location.hash='legacy';check(document.querySelector(':target').getAttribute('name')==='legacy' && window.scrollY===380,'anchor name fallback');
            location.hash='top';check(document.querySelector(':target').id==='top' && window.scrollY===40,'id top beats top sentinel');
            location.hash='TOP';check(document.querySelector(':target')===null && window.scrollY===0,'decoded case-insensitive top');
            location.hash='missing';check(document.querySelector(':target')===null,'unmatched clears target');
            location.hash='';check(document.URL.endsWith('#') && window.scrollY===0,'empty fragment remains distinct from null');
            return true;
        })()"#);
        let target=selector::get_element_by_id(realm.session.borrow().document(),realm.session.borrow().document().root(),"top").unwrap().unwrap();
        assert_eq!(realm.sequential_focus_start(),Some(target));
    }

    #[test]
    fn specification_window_fragment_scroll_uses_signed_root_and_nested_scrollports() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),r#"<!doctype html><style>html,body{margin:0}html{direction:rtl}
            #extent{height:600px}#negative{position:absolute;left:-140px;top:100px;width:20px;height:20px;background:red}
            #port{position:absolute;left:0;top:200px;width:60px;height:30px;overflow:auto}
            #spacer{height:120px}#leaf{height:20px;background:blue}</style>
            <div id=extent></div><div id=negative></div><div id=port><div id=spacer></div><div id=leaf></div></div>"#,128).unwrap();
        realm.set_document_url("https://fragment.test/page.html");
        layout(&realm);
        check(&mut engine,r#"(() => {
            const check=(ok,message)=>{if(!ok)throw new Error(message);};
            const negative=document.getElementById('negative'),port=document.getElementById('port'),leaf=document.getElementById('leaf');
            location.hash='negative';
            const bounds=negative.getBoundingClientRect();
            check(scrollX===-140 && bounds.left===0 && bounds.top===0,'signed fragment movement must change actual geometry');
            scrollTo(-20,0);check(scrollX===-20,'native scroll accepts a real signed offset');
            scrollTo(-99999,0);check(scrollX===-140,'negative extent clamp');
            scrollTo(99999,0);check(scrollX===0,'positive extent clamp at RTL origin');
            location.hash='leaf';
            check(port.scrollTop===110 && leaf.getBoundingClientRect().top===0,'fragment reveals actual nested scrollports before viewport');
            return true;
        })()"#);
        let negative=selector::get_element_by_id(realm.session.borrow().document(),realm.session.borrow().document().root(),"negative").unwrap().unwrap();
        navigate(engine.ctx(),&realm).unwrap();
        realm.flush_layout().unwrap();
        assert_ne!(realm.session.borrow().hit_test(5.0,5.0),Some(negative));
    }

    #[test]
    fn specification_window_fragment_reveal_snapshots_and_author_mutation_abort() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),r#"<!doctype html><div id=h1 hidden=until-found><div id=child></div></div><div id=h2 hidden=until-found></div>
            <details id=a3><div id=a2 hidden=until-found><details id=a1 hidden=until-found><div id=deep></div></details></div></details>"#,128).unwrap();
        realm.set_document_url("https://fragment.test/page.html");
        layout(&realm);
        check(&mut engine,r#"(() => {
            const check=(ok,message)=>{if(!ok)throw new Error(message);};
            const h1=document.getElementById('h1'),h2=document.getElementById('h2');
            let calls=[];
            h1.onbeforematch=function(event){calls.push([event.target===h1,event.currentTarget===h1,event.isTrusted,event.bubbles,this===h1]);h2.append(h1);};
            h2.addEventListener('beforematch',event=>{if(event.target===h2)calls.push('unexpected second reveal');});
            location.hash='child';
            check(JSON.stringify(calls)==='[[true,true,true,true,true]]','snapshot and trusted typed beforematch slot');
            check(h1.hidden===false && h2.hidden==='until-found','new ancestor was not in reveal snapshot');
            const a1=document.getElementById('a1'),a2=document.getElementById('a2'),a3=document.getElementById('a3');
            let order=[];
            a1.addEventListener('beforematch',()=>{order.push([a1.open,a1.hidden,a2.hidden,a3.open]);});
            a2.addEventListener('beforematch',event=>{order.push(event.target===a1);a1.hidden=true;});
            location.hash='deep';
            check(JSON.stringify(order)==='[[true,"until-found","until-found",false],true]','details slot reveal precedes matching hidden ancestor');
            check(a1.open && a1.hidden===true && a2.hidden==='until-found' && !a3.open,'author change aborts remaining reveal pairs');
            const detached=document.createElement('div');detached.id='detached';detached.hidden='until-found';document.body.append(detached);
            detached.onbeforematch=()=>detached.remove();
            location.hash='detached';
            check(!detached.isConnected && detached.hidden==='until-found','detached matching target aborts attribute removal');
            return true;
        })()"#);
    }

    #[test]
    fn specification_window_resize_scroll_clamp_uses_real_queued_document_events() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<!doctype html><style>html,body{margin:0}#space{height:400px}</style><div id=space></div>",64).unwrap();
        realm.set_document_url("https://fragment.test/page.html");
        layout(&realm);
        check(&mut engine,"scrollTo(0,100);window.scrollY===100");
        assert!(scheduling::run_animation_frame(&mut engine).is_empty());
        check(&mut engine,r#"(() => {
            globalThis.clampEvents=[];
            document.addEventListener('scroll',event=>clampEvents.push([event.isTrusted,event.target===document,scrollY]));
            document.getElementById('space').style.height='0px';
            const y=scrollY;
            return y===0 && clampEvents.length===0;
        })()"#);
        realm.update_rendered_focus(engine.ctx()).unwrap();
        realm.update_rendered_focus(engine.ctx()).unwrap();
        assert!(scheduling::run_animation_frame(&mut engine).is_empty());
        check(&mut engine,"JSON.stringify(clampEvents)==='[[true,true,0]]'");
    }

    #[test]
    fn specification_window_fragment_target_follows_adopted_identity_and_real_tab_navigation() {
        let mut engine=Engine::new();
        let realm=crate::install(engine.ctx(),"<!doctype html><button id=before tabindex=1></button><div id=target></div><button id=after></button><button id=priority tabindex=2></button>",128).unwrap();
        realm.set_document_url("https://fragment.test/page.html");
        layout(&realm);
        check(&mut engine,r#"(() => {
            const target=document.getElementById('target');
            location.hash='target';
            const other=document.implementation.createHTMLDocument();
            other.body.append(other.adoptNode(target));
            if(target.matches(':target') || other.querySelector(':target')!==null)throw new Error('destination must not acquire the original document target');
            document.body.insertBefore(document.adoptNode(target),document.getElementById('after'));
            if(document.querySelector(':target')!==target)throw new Error('round-trip adoption must preserve actual target identity');
            target.remove();document.body.insertBefore(target,document.getElementById('after'));
            if(!target.matches(':target'))throw new Error('reinsertion must not select a new target');
            return true;
        })()"#);
        engine.collect_garbage();
        realm.focus_next(engine.ctx(),false).unwrap();
        check(&mut engine,"if(document.activeElement!==document.getElementById('after'))throw new Error('forward Tab after adopted target and intermediate-document GC: '+document.activeElement.id);true");
        check(&mut engine,"location.hash='before';location.hash='target';true");
        realm.focus_next(engine.ctx(),true).unwrap();
        check(&mut engine,"if(document.activeElement!==document.getElementById('before'))throw new Error('reverse Tab starts before fragment target: '+document.activeElement.id);true");
        check(&mut engine,"document.getElementById('target').tabIndex=-1;location.hash='before';location.hash='target';true");
        realm.focus_next(engine.ctx(),false).unwrap();
        check(&mut engine,"if(document.activeElement!==document.getElementById('after'))throw new Error('negative tabindex fragment target preserves DOM-order starting point: '+document.activeElement.id);true");
        check(&mut engine,"document.getElementById('before').focus();true");
        realm.focus_next(engine.ctx(),false).unwrap();
        check(&mut engine,"if(document.activeElement!==document.getElementById('priority'))throw new Error('explicit focus consumes fragment starting point and uses tabindex order: '+document.activeElement.id);true");
    }

    #[test]
    fn specification_window_initial_fragment_matures_in_live_parser_without_busy_tasks() {
        let mut engine=Engine::new();
        let realm=crate::install_live_html(engine.ctx(),"<!doctype html><style>html,body{margin:0}#space{height:500px}#late{position:absolute;top:200px;width:10px;height:10px}</style><script>globalThis.firstTurn=true</script><div id=space></div><div id=late tabindex=0></div><script>globalThis.targetTurn=true</script>",128).unwrap();
        realm.set_document_url("https://fragment.test/page.html#late");
        layout(&realm);
        assert!(realm.next_document_parser_script(engine.ctx()).unwrap().is_some());
        assert!(scheduling::run_navigation_tasks(&mut engine,64).is_empty());
        assert!(!scheduling::task_pending(engine.ctx()),"paused parser retry must be gated rather than busy-looping");
        assert!(realm.next_document_parser_script(engine.ctx()).unwrap().is_some());
        check(&mut engine,"window.scrollY===200 && document.activeElement===document.getElementById('late') && document.getElementById('late').matches(':target')");
        assert!(realm.next_document_parser_script(engine.ctx()).unwrap().is_none());
        assert!(scheduling::run_navigation_tasks(&mut engine,64).is_empty());
        assert!(scheduling::run_tasks(&mut engine,64).is_empty());
        assert!(!scheduling::task_pending(engine.ctx()));
        realm.set_same_document_url("https://fragment.test/page.html#absent".into());
        checkpoint(engine.ctx(),&realm,false).unwrap();
        check(&mut engine,"document.getElementById('late').matches(':target')");
    }
}
