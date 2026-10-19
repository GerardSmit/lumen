//! The web extension publishes native classes lazily and never overwrites a host replacement.
fn run_in_web_realm(name: &'static str, source: &'static str) {
    std::thread::Builder::new()
        .name(name.into())
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let mut engine = lumen::Engine::new();
            lumen_host::install(&mut engine, &[lumen_web::extension()]);
            match engine.eval(source, false).expect("parse web realm contract") {
                lumen::Completion::Value(value) => assert_eq!(value, "true"),
                lumen::Completion::Throw { name, message } => panic!("{name}: {message}"),
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn lazy_provider_keeps_replacement_and_publishes_native_messaging() {
    run_in_web_realm(
        "lazy-replacement",
        r#"
            globalThis.hostRejection=function HostRejection(){};
            Object.defineProperty(globalThis,'PromiseRejectionEvent',{
                value:hostRejection,writable:false,enumerable:false,configurable:false
            });
            let withoutPorts='none';
            try { new MessageChannel(); } catch (error) { withoutPorts=error.constructor.name; }
            PromiseRejectionEvent===hostRejection &&
                typeof MessagePort==='function' && typeof BroadcastChannel==='function' &&
                Object.getPrototypeOf(MessageEvent)===Event &&
                new MessageEvent('message',{data:1}).data===1 &&
                withoutPorts==='TypeError' &&
                !Object.getOwnPropertyDescriptor(globalThis,'PromiseRejectionEvent').configurable;
        "#,
    );
}

#[test]
fn navigator_is_a_native_navigator_with_a_prototype_user_agent() {
    run_in_web_realm(
        "navigator-shape",
        r#"
            const global = Object.getOwnPropertyDescriptor(globalThis, 'navigator');
            const agent = Object.getOwnPropertyDescriptor(Navigator.prototype, 'userAgent');
            let illegal = 'none';
            try { new Navigator(); } catch (error) { illegal = error.constructor.name; }
            global.enumerable && global.writable && global.configurable &&
                navigator instanceof Navigator && navigator === globalThis.navigator &&
                navigator.userAgent === 'lumen' &&
                typeof agent.get === 'function' && agent.set === undefined && agent.enumerable &&
                !Object.prototype.hasOwnProperty.call(navigator, 'userAgent') &&
                illegal === 'TypeError' &&
                !Object.getOwnPropertyDescriptor(globalThis, 'Navigator').enumerable;
        "#,
    );
}

#[test]
fn raw_transport_namespaces_are_not_left_on_the_global() {
    run_in_web_realm(
        "no-raw-namespaces",
        r#"
            typeof __http === 'undefined' && typeof __url === 'undefined' &&
                typeof fetch === 'function' && typeof URL === 'function';
        "#,
    );
}
