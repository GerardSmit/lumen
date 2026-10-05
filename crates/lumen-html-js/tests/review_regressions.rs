use lumen::embed::Value;

#[test]
fn dom_callback_owner_preserves_connected_function_object_and_content_handlers() {
    let mut engine = lumen::Engine::new();
    lumen_html_js::install(engine.ctx(), "<body></body>", 64).unwrap();
    let result = engine.eval_value(r#"(() => {
        globalThis.callbackOwnerCalls = [];
        const functionNode = document.createElement('div');
        functionNode.id = 'function-owner';
        functionNode.onclick = function(e) {
            callbackOwnerCalls.push(this === functionNode && e.target === functionNode ? 'function' : 'bad');
        };
        const objectNode = document.createElement('div');
        objectNode.id = 'object-owner';
        objectNode.addEventListener('click', {
            owner: objectNode,
            handleEvent(e) { callbackOwnerCalls.push(e.target === this.owner ? 'object' : 'bad'); }
        });
        const contentNode = document.createElement('div');
        contentNode.id = 'content-owner';
        contentNode.setAttribute('onclick', "callbackOwnerCalls.push(this.id === 'content-owner' && event.target === this ? 'content' : 'bad')");
        document.body.append(functionNode, objectNode, contentNode);
        if (typeof contentNode.onclick !== 'function') throw new Error('content handler compiles');
        return true;
    })()"#).unwrap().ok().expect("connected callback owners prepare");
    assert!(matches!(result, Value::Bool(true)));
    engine.collect_garbage();
    let result = engine.eval_value(r#"(() => {
        for (const id of ['function-owner', 'object-owner', 'content-owner'])
            document.getElementById(id).dispatchEvent(new Event('click'));
        return callbackOwnerCalls.join(',') === 'function,object,content';
    })()"#).unwrap().ok().expect("connected callbacks dispatch after GC");
    assert!(matches!(result, Value::Bool(true)));
}

#[test]
fn dom_callback_owner_collects_unreachable_persistent_function_object_and_content_cycles() {
    let mut engine = lumen::Engine::new();
    lumen_html_js::install(engine.ctx(), "<body></body>", 64).unwrap();
    let result = engine.eval_value(r#"(() => {
        const functionNode = document.createElement('div');
        functionNode.onclick = () => functionNode;
        const objectNode = document.createElement('div');
        objectNode.addEventListener('click', { owner: objectNode, handleEvent() { return this.owner; } });
        const contentNode = document.createElement('div');
        contentNode.setAttribute('onclick', 'return this');
        if (typeof contentNode.onclick !== 'function') throw new Error('content handler compiles');
        globalThis.functionCycleProbe = functionNode;
        globalThis.objectCycleProbe = objectNode;
        globalThis.contentCycleProbe = contentNode;
        return true;
    })()"#).unwrap().ok().expect("detached callback cycles prepare");
    assert!(matches!(result, Value::Bool(true)));
    let mut probes = Vec::new();
    for name in ["functionCycleProbe", "objectCycleProbe", "contentCycleProbe"] {
        let value = engine.eval_value(name).unwrap().ok().expect("cycle probe exists");
        probes.push(engine.ctx().weak_value(&value).expect("cycle probe is native wrapper"));
    }
    engine.eval_value("functionCycleProbe = null; objectCycleProbe = null; contentCycleProbe = null").unwrap().ok().expect("cycle roots clear");
    engine.collect_garbage();
    for (name, probe) in ["function", "object", "content"].into_iter().zip(probes) {
        assert!(probe.upgrade().is_none(), "unreachable persistent {name} callback owner collects");
    }
}

#[test]
fn dom_callback_owner_event_target_edge_follows_event_reachability() {
    let mut engine = lumen::Engine::new();
    lumen_html_js::install(engine.ctx(), "<body></body>", 64).unwrap();
    engine.eval_value(r#"(() => {
        const target = document.createElement('div');
        target.onclick = () => target;
        globalThis.dispatchedEventProbe = new Event('click');
        target.dispatchEvent(dispatchedEventProbe);
        globalThis.dispatchedTargetProbe = target;
        return true;
    })()"#).unwrap().ok().expect("event/target prepare");
    let value = engine.eval_value("dispatchedTargetProbe").unwrap().ok().expect("target exists");
    let target = engine.ctx().weak_value(&value).expect("target is a wrapper");
    drop(value);
    engine.eval_value("dispatchedTargetProbe = null").unwrap().ok().expect("target root clears");
    engine.collect_garbage();
    assert!(target.upgrade().is_some(), "reachable dispatched Event retains its target");
    engine.eval_value("dispatchedEventProbe = null").unwrap().ok().expect("event root clears");
    engine.collect_garbage();
    assert!(target.upgrade().is_none(), "unreachable native Event target edge does not root callback cycle");
}

#[test]
fn ontoggle_uses_native_handlers_and_retains_callback_across_gc() {
    let mut engine = lumen::Engine::new();
    lumen_html_js::install(engine.ctx(), "<details></details><svg></svg>", 64).unwrap();
    let result = engine.eval_value(r#"(() => {
        globalThis.toggleTarget = document.querySelector('details');
        globalThis.toggleCalls = [];
        const target = toggleTarget;
        if (target.ontoggle !== null || document.querySelector('svg').ontoggle !== null)
            throw new Error('inherited initial handler');
        target.addEventListener('toggle', event => {
            if (!(event instanceof ToggleEvent) || event !== toggleEvent ||
                event.target !== target || event.currentTarget !== target)
                throw new Error('native event identity');
            toggleCalls.push('first');
        });
        target.ontoggle = () => toggleCalls.push('old');
        target.addEventListener('toggle', () => toggleCalls.push('last'));
        const replacement = function(event) {
            if (this !== target || event !== toggleEvent ||
                event.oldState !== 'closed' || event.newState !== 'open')
                throw new Error('handler receiver/state');
            toggleCalls.push('new');
        };
        target.ontoggle = replacement;
        if (target.ontoggle !== replacement) throw new Error('callback identity');
        return true;
    })()"#).unwrap().ok().expect("ontoggle preparation threw");
    assert!(matches!(result, Value::Bool(true)));
    engine.collect_garbage();
    let result = engine.eval_value(r#"(() => {
        globalThis.toggleEvent = new ToggleEvent('toggle', {oldState:'closed', newState:'open'});
        toggleTarget.dispatchEvent(toggleEvent);
        if (toggleCalls.join(',') !== 'first,new,last') throw new Error('replacement order');
        toggleTarget.ontoggle = null;
        if (toggleTarget.ontoggle !== null) throw new Error('null clearing');
        return true;
    })()"#).unwrap().ok().expect("ontoggle dispatch threw");
    assert!(matches!(result, Value::Bool(true)));
    engine.collect_garbage();
    let result = engine.eval_value(r#"(() => {
        toggleTarget.dispatchEvent(toggleEvent);
        if (toggleCalls.join(',') !== 'first,new,last,first,last')
            throw new Error('cleared handler still dispatched');
        const reassigned = () => toggleCalls.push('reassigned');
        toggleTarget.ontoggle = reassigned;
        if (toggleTarget.ontoggle !== reassigned) throw new Error('reassigned identity');
        toggleTarget.dispatchEvent(toggleEvent);
        return toggleCalls.join(',') === 'first,new,last,first,last,first,last,reassigned';
    })()"#).unwrap().ok().expect("ontoggle clearing/reassignment threw");
    assert!(matches!(result, Value::Bool(true)));
}

#[test]
fn css_rules_keep_identity_across_insert_delete_and_replacement() {
    let mut engine = lumen::Engine::new();
    lumen_html_js::install(
        engine.ctx(),
        "<style>.a {color:red} .b {color:blue}</style>",
        64,
    )
    .unwrap();
    let result = engine.eval_value(r#"(() => {
        const sheet = document.querySelector('style').sheet;
        const saved = sheet.cssRules[1], declaration = saved.style;
        if (sheet.cssRules[1] !== saved) throw new Error('rule identity');
        sheet.insertRule('.x {color:green}', 0);
        if (saved.selectorText !== '.b' || sheet.cssRules[2] !== saved) throw new Error('insert shifts saved rule');
        declaration.color = 'purple';
        if (sheet.cssRules[2].style.color !== 'purple' || sheet.cssRules[1].style.color !== 'red') throw new Error('saved declaration writes wrong rule');
        sheet.deleteRule(0);
        if (sheet.cssRules[1] !== saved) throw new Error('delete shifts saved rule');
        sheet.deleteRule(1);
        if (saved.selectorText !== '.b' || saved.parentStyleSheet !== null) throw new Error('deleted rule detachment');
        declaration.color = 'yellow';
        if (declaration.color !== 'yellow' || sheet.cssRules[0].style.color !== 'red') throw new Error('detached declaration writes live sheet');
        const constructed = new CSSStyleSheet();
        constructed.replaceSync('.old {color:blue}');
        const old = constructed.cssRules[0];
        constructed.replaceSync('.new {color:red}');
        if (old.selectorText !== '.old' || old.parentStyleSheet !== null) throw new Error('replace detachment');
        old.style.color = 'green';
        if (constructed.cssRules[0].style.color !== 'red') throw new Error('replace redirects writes');
        const equal = constructed.cssRules[0];
        constructed.replaceSync(constructed.cssRules[0].cssText);
        if (constructed.cssRules[0] === equal || equal.parentStyleSheet !== null) throw new Error('identical replacement keeps removed rule');
        equal.style.color = 'blue';
        if (constructed.cssRules[0].style.color !== 'red') throw new Error('identical replacement redirects writes');
        constructed.replaceSync('@keyframes fade {from{opacity:0}50%{opacity:0.5}to{opacity:1}}');
        const keys = constructed.cssRules[0], frame = keys.cssRules[1], frameStyle = frame.style;
        if (keys.findRule('50%') !== frame || keys.cssRules[1] !== frame) throw new Error('keyframe identity');
        constructed.insertRule('.before {color:black}',0);
        keys.deleteRule('from');
        if (frame.keyText !== '50%' || keys.cssRules[0] !== frame) throw new Error('keyframe index shifts');
        frameStyle.opacity = '0.75';
        if (keys.cssRules[0].style.opacity !== '0.75') throw new Error('saved keyframe declaration');
        keys.deleteRule('50%');
        frameStyle.opacity = '0.25';
        if (frame.keyText !== '50%' || frame.parentStyleSheet !== null || keys.cssRules[0].style.opacity !== '1') throw new Error('detached keyframe writes sibling');
        return true;
    })()"#).unwrap().ok().expect("CSS rule identity regression threw");
    assert!(matches!(result, Value::Bool(true)));
}
