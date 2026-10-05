//! Exercise the actual private lazy publisher independently of HTML adapter installation.
fn lazy_provider_contract(body: &'static str) {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(move || {
            let mut engine = lumen::Engine::new();
            let source = format!(
                "(function() {{\n{}\n{}\n}})()",
                include_str!("../src/js/preamble.js"),
                body
            );
            match engine
                .eval(&source, false)
                .expect("parse actual preamble contract")
            {
                lumen::Completion::Value(value) => assert_eq!(value, "true"),
                lumen::Completion::Throw { name, message } => panic!("{name}: {message}"),
            }
        })
        .unwrap()
        .join()
        .unwrap();
}

#[test]
fn lazy_provider_preserves_host_descriptors_and_deleted_exports() {
    lazy_provider_contract(
        r#"
            let reads=0, writes=0, runs=0;
            const host={};
            __lazyDefineProperty(globalThis,'lpExisting',{value:host,writable:false,configurable:false});
            __lazyWeb('lpExisting lpData lpAccessor lpDeleted lpHidden! lpNew', (out,define)=>{
                runs++;
                out.lpExisting='wrong'; out.lpData='wrong'; out.lpAccessor='wrong'; out.lpDeleted='wrong';
                out.lpHidden=8; out.lpNew=9;
                define('lpAccessor',{value:'also wrong',configurable:true});
                define('lpInternal',{value:17,writable:false,configurable:false});
            });
            __lazyDefineProperty(globalThis,'lpData',{value:host,writable:true,enumerable:false,configurable:false});
            const getter=()=>{reads++;return host;}, setter=()=>writes++;
            __lazyDefineProperty(globalThis,'lpAccessor',{get:getter,set:setter,enumerable:true,configurable:true});
            delete globalThis.lpDeleted;
            const value=lpNew;
            const data=__lazyGetDescriptor(globalThis,'lpData');
            const accessor=__lazyGetDescriptor(globalThis,'lpAccessor');
            return value===9 && runs===1 && lpExisting===host && data.value===host &&
                data.writable && !data.enumerable && !data.configurable && accessor.get===getter &&
                accessor.set===setter && accessor.enumerable && accessor.configurable && reads===0 && writes===0 &&
                __lazyGetDescriptor(globalThis,'lpDeleted')===undefined && lpHidden===8 &&
                !__lazyGetDescriptor(globalThis,'lpHidden').enumerable && lpInternal===17 &&
                !__lazyGetDescriptor(globalThis,'lpInternal').configurable;
        "#,
    );
}

#[test]
fn lazy_provider_reentry_failure_and_captured_intrinsics() {
    lazy_provider_contract(
        r#"
            let runs=0, nested='unset', conversions=0;
            const host={}, failure={};
            __lazyWeb('lpOuter lpNested lpReplaced lpPartial lpFailed', (out,define)=>{
                runs++; nested=globalThis.lpNested;
                globalThis.lpReplaced=host;
                out.lpReplaced='wrong'; out.lpOuter=3; out.lpPartial=4;
                define({toString(){conversions++;return 'lpKey';}}, {value:5,configurable:false});
                throw failure;
            });
            const descriptor=Object.getOwnPropertyDescriptor, define=Object.defineProperty,
                create=Object.create, keys=Reflect.ownKeys;
            Object.getOwnPropertyDescriptor=Object.defineProperty=Object.create=Reflect.ownKeys=()=>{throw 'author intrinsic';};
            let caught=false;
            try { globalThis.lpOuter; } catch(error) { caught=error===failure; }
            finally { Object.getOwnPropertyDescriptor=descriptor; Object.defineProperty=define;
                Object.create=create; Reflect.ownKeys=keys; }
            return caught && runs===1 && nested===undefined && lpOuter===3 && lpPartial===4 &&
                lpReplaced===host && lpFailed===undefined && lpNested===undefined && lpKey===5 &&
                conversions===1 && runs===1;
        "#,
    );
}

#[test]
fn lazy_provider_preserves_modified_lazy_accessor_descriptors() {
    lazy_provider_contract(
        r#"
        let writes=0;
        __lazyWeb('lpSetterModified lpLocked lpEnumModified lpDescriptorTrigger', (out,define)=>{
            out.lpSetterModified=1; out.lpLocked=2; out.lpEnumModified=3;
            define('lpLocked',{value:'wrong',configurable:true});
            out.lpDescriptorTrigger=4;
        });
        const a=__lazyGetDescriptor(globalThis,'lpSetterModified');
        const b=__lazyGetDescriptor(globalThis,'lpLocked');
        const c=__lazyGetDescriptor(globalThis,'lpEnumModified');
        const replacement=()=>writes++;
        __lazyDefineProperty(globalThis,'lpSetterModified',{set:replacement});
        __lazyDefineProperty(globalThis,'lpLocked',{configurable:false});
        __lazyDefineProperty(globalThis,'lpEnumModified',{enumerable:false});
        if(lpDescriptorTrigger!==4 || lpSetterModified!==undefined || lpLocked!==undefined || lpEnumModified!==undefined)return false;
        const aa=__lazyGetDescriptor(globalThis,'lpSetterModified');
        const bb=__lazyGetDescriptor(globalThis,'lpLocked');
        const cc=__lazyGetDescriptor(globalThis,'lpEnumModified');
        return aa.get===a.get && aa.set===replacement && aa.configurable && aa.enumerable &&
            bb.get===b.get && bb.set===b.set && !bb.configurable && bb.enumerable &&
            cc.get===c.get && cc.set===c.set && cc.configurable && !cc.enumerable && writes===0;
    "#,
    );
}

#[test]
fn lazy_provider_generated_glue_keeps_replacement_and_delivers_real_channel_message() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let mut engine = lumen::Engine::new();
            lumen_host::install(&mut engine, &[lumen_web::extension()]);
            // The standalone web extension has no timer provider. Supply a test host
            // task queue; execute genuine MessagePort callbacks without replacing
            // any messaging constructor or delivery method.
            let source = r#"
                globalThis.tasks=[];
                globalThis.setTimeout=callback=>tasks.push(callback);
                globalThis.hostRejection=function HostRejection(){};
                Object.defineProperty(globalThis,'PromiseRejectionEvent',{
                    value:hostRejection,writable:false,enumerable:false,configurable:false
                });
                globalThis.channel=new MessageChannel();
                globalThis.received=[];
                channel.port1.onmessage=event=>received.push(event);
                channel.port2.postMessage({value:'generated-glue'});
                while(tasks.length)tasks.shift()();
                PromiseRejectionEvent===hostRejection &&
                    typeof MessagePort==='function' && channel.port1 instanceof MessagePort &&
                    received.length===1 && received[0] instanceof MessageEvent &&
                    received[0].data.value==='generated-glue' &&
                    !Object.getOwnPropertyDescriptor(globalThis,'PromiseRejectionEvent').configurable;
            "#;
            match engine.eval(source, false).expect("parse generated glue contract") {
                lumen::Completion::Value(value) => assert_eq!(value, "true"),
                lumen::Completion::Throw { name, message } => panic!("{name}: {message}"),
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
