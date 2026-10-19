use lumen::embed::Value;

fn run(source: &str, expected: &str) {
    let source = source.to_owned(); let expected = expected.to_owned();
    std::thread::Builder::new().stack_size(64 * 1024 * 1024).spawn(move || {
        let mut runtime = lumen_runtime::Runtime::new_browser();
        runtime.set_deadline(std::time::Duration::from_secs(10));
        let engine = runtime.engine();
        let realm = lumen_html_js::install(engine.ctx(), "<!doctype html><body></body>", 1024).unwrap();
        realm.set_document_url("https://database.test/basic.html");
        fn eval(engine: &mut lumen::Engine, source: &str) -> Value {
            match engine.eval_value(source).unwrap() {
                Ok(value) => value,
                Err(error) => { let detail = engine.ctx().get_member(&error, "stack").ok().and_then(|value| engine.ctx().coerce_string(&value).ok()); panic!("IndexedDB test threw: {detail:?}"); }
            }
        }
        eval(engine, &source);
        assert!(matches!(eval(engine, "!done"), Value::Bool(true)), "factory completion must be asynchronous");
        while engine.run_one_job() {}
        lumen_host::indexed_db::end_task(engine.ctx());
        for _ in 0..32 {
            let errors = lumen_html_js::scheduling::run_tasks(engine, 256);
            let diagnostics: Vec<_> = errors.iter().map(|error| engine.ctx().coerce_string(error).unwrap_or_default()).collect();
            assert!(errors.is_empty(), "task errors: {diagnostics:?}");
            if matches!(eval(engine, "done"), Value::Bool(true)) { break; }
        }
        let final_state = eval(engine, "JSON.stringify(log)");
        assert!(matches!(eval(engine, &expected), Value::Bool(true)), "final state: {}", engine.ctx().coerce_string(&final_state).unwrap_or_default());
    }).unwrap().join().unwrap();
}

#[test]
fn indexed_db_preserves_native_geometry_brands_and_shared_references() {
    run(r#"
        globalThis.done=false;globalThis.log=[];
        const check=(ok,message)=>{if(!ok)throw new Error(message)};
        const opening=indexedDB.open('geometry-values',1);
        opening.onupgradeneeded=()=>{
            const store=opening.result.createObjectStore('values');
            const matrix=new DOMMatrix([2,0,0,3,4,5]);
            const value={matrix,alias:matrix,readonly:new DOMMatrixReadOnly([1,0,0,1,6,7]),point:new DOMPoint(1,2,3,4),readonlyPoint:new DOMPointReadOnly(5,6,7,8),rect:new DOMRect(1,2,3,4),readonlyRect:new DOMRectReadOnly(5,6,7,8)};
            store.put(value,1).onsuccess=()=>store.get(1).onsuccess=e=>{
                const result=e.target.result;
                check(result.matrix instanceof DOMMatrix && result.matrix===result.alias,'matrix storage brand and alias');
                check(result.matrix.transformPoint(result.point).x===18,'homogeneous stored point transformation');
                check(result.readonly instanceof DOMMatrixReadOnly && !(result.readonly instanceof DOMMatrix),'readonly matrix storage');
                check(result.point instanceof DOMPoint && result.readonlyPoint instanceof DOMPointReadOnly && result.readonlyPoint.w===8,'point storage');
                check(result.rect instanceof DOMRect && result.readonlyRect instanceof DOMRectReadOnly && result.readonlyRect.height===8,'rect storage');
                log.push('geometry');
            };
        };
        opening.onerror=()=>{throw opening.error};
        opening.onsuccess=()=>{opening.result.close();done=true};
    "#, "done && log.join(',')==='geometry'");
}

#[test]
fn indexed_db_indexes_unique_multi_entry_compound_keys_and_generated_injection() {
    run(r#"
        globalThis.done=false;globalThis.log=[];
        const check=(ok,message)=>{if(!ok)throw new Error(message)};
        const opening=indexedDB.open('indexed',1);
        opening.onupgradeneeded=()=>{
            const db=opening.result,store=db.createObjectStore('items',{keyPath:'nested.id',autoIncrement:true});
            let syntax=false;try{db.createObjectStore('bad',{keyPath:'a..b'})}catch(e){syntax=e.name==='SyntaxError'}check(syntax,'invalid path syntax');
            let empty=false;try{db.createObjectStore('empty-sequence',{keyPath:[]})}catch(e){empty=e.name==='SyntaxError'}check(empty,'empty sequence invalid');
            let invalid=false;try{db.createObjectStore('compound-auto',{keyPath:['id'],autoIncrement:true})}catch(e){invalid=e.name==='InvalidAccessError'}check(invalid,'compound auto invalid');
            const unique=store.createIndex('email','email',{unique:true});
            const tags=store.createIndex('tags','tags',{multiEntry:true});
            const compound=store.createIndex('full',['profile.first','profile.last']);
            check(unique instanceof IDBIndex && unique.objectStore===store && unique.unique && tags.multiEntry,'typed index metadata');
            check(JSON.stringify(compound.keyPath)==='["profile.first","profile.last"]' && store.indexNames.contains('tags'),'compound metadata');
            let reads=0;const original={get email(){reads++;return 'first'},tags:['dup','dup',{},'z'],profile:{first:'A',last:'B'}};
            const first=store.put(original);
            check(reads===1 && original.nested===undefined,'clone getter once and original uninjected');
            first.onsuccess=()=>{
                check(first.result===1,'generated primary');
                const duplicate=store.put({email:'first'});
                duplicate.onerror=e=>{
                    check(duplicate.error.name==='ConstraintError','unique actual constraint');e.preventDefault();log.push('constraint');
                    const next=store.put({email:'second'});
                    next.onsuccess=()=>{
                        check(next.result===2,'failed uniqueness does not consume generator');
                        tags.getKey('dup').onsuccess=e=>check(e.target.result===1,'multi entry dedup');
                        compound.getKey(['A','B']).onsuccess=e=>check(e.target.result===1,'compound index');
                        unique.get('first').onsuccess=e=>check(e.target.result.nested.id===1,'generated key in cloned record');
                    };
                };
            };
        };
        opening.onsuccess=()=>{
            const db=opening.result,tx=db.transaction('items','readwrite'),store=tx.objectStore('items');
            store.delete(1).onsuccess=()=>store.index('email').get('first').onsuccess=e=>check(e.target.result===undefined,'delete removes secondary key');
            tx.oncomplete=()=>{db.close();log.push('complete');done=true;};
        };
    "#, "done && log.join(',')==='constraint,complete'");
}

#[test]
fn indexed_db_backfill_unique_failure_aborts_upgrade_atomically() {
    run(r#"
        globalThis.done=false;globalThis.log=[];
        const check=(ok,message)=>{if(!ok)throw new Error(message)};
        const first=indexedDB.open('backfill',1);
        first.onupgradeneeded=()=>{const store=first.result.createObjectStore('items');store.put({email:'same'},1);store.put({email:'same'},2);};
        first.onsuccess=()=>{
            first.result.close();const second=indexedDB.open('backfill',2);
            second.onupgradeneeded=()=>{second.result.transaction;const index=second.transaction.objectStore('items').createIndex('email','email',{unique:true});check(index instanceof IDBIndex,'index immediately available');log.push('created');};
            second.onerror=e=>{
                e.preventDefault();check(second.error.name==='AbortError','failed backfill aborts open');
                const retry=indexedDB.open('backfill');retry.onsuccess=()=>{const db=retry.result;check(db.version===1,'version rollback');const tx=db.transaction('items');check(!tx.objectStore('items').indexNames.contains('email'),'index rollback');tx.objectStore('items').get(2).onsuccess=e=>check(e.target.result.email==='same','records retained');tx.oncomplete=()=>{db.close();log.push('rolled-back');done=true;};};
            };
        };
    "#, "done && log.join(',')==='created,rolled-back'");
}

#[test]
fn indexed_db_async_upgrade_clone_put_get_and_microtask_activity() {
    run(r#"
        globalThis.done=false;globalThis.log=[];
        const check=(ok,message)=>{if(!ok)throw new Error(message)};
        const opening=indexedDB.open('records',1);
        check(opening instanceof IDBOpenDBRequest && opening instanceof IDBRequest,'request brands');
        check(opening.readyState==='pending' && opening.source===null && opening.transaction===null,'pending metadata');
        let threw=false;try{opening.result}catch(e){threw=e.name==='InvalidStateError'};check(threw,'pending result throws');
        opening.onupgradeneeded=e=>{
            check(e.isTrusted && e instanceof IDBVersionChangeEvent && e.oldVersion===0 && e.newVersion===1,'upgrade event');
            const db=opening.result,tx=opening.transaction;
            check(tx.db===db,'transaction database identity');
            check(db.name==='records' && db.version===1 && tx.mode==='versionchange','upgrade metadata');
            const store=db.createObjectStore('items');
            check(db.objectStoreNames.contains('items') && store.transaction===tx,'schema and transaction identity');
            const value={nested:{answer:42}},put=store.put(value,1);value.nested.answer=9;
            check(put.source===store && put.transaction===tx && put.readyState==='pending','put metadata');
            put.onsuccess=e=>{
                check(e.isTrusted && put.result===1,'put completion');log.push('put');
                Promise.resolve().then(()=>{
                    const read=store.get(1);
                    read.onsuccess=()=>{check(read.result.nested.answer===42,'storage clone snapshot');read.result.nested.answer=0;log.push('read-upgrade');};
                });
            };
            tx.oncomplete=()=>{
                check(store.transaction===tx && tx.db===db,'finished identity');
                const read=db.transaction('items');read.objectStore('items').get(1);
                log.push('upgrade-complete');
                Promise.resolve().then(()=>log.push('upgrade-microtask'));
            };log.push('upgrade');
        };
        opening.onsuccess=()=>{
            log.push('open');check(opening.transaction===null && opening.error===null,'terminal open');
            const db=opening.result,tx=db.transaction('items'),store=tx.objectStore('items');
            let readOnly=false;try{store.put('bad',3)}catch(e){readOnly=e.name==='ReadOnlyError'};check(readOnly,'readonly write');
            const read=store.get(IDBKeyRange.only(1));
            read.onsuccess=()=>{check(read.result.nested.answer===42,'fresh read clone');log.push('read');};
            tx.oncomplete=()=>{db.close();log.push('complete');done=true;};
        };
    "#, "done && log.join(',')==='upgrade,put,read-upgrade,upgrade-complete,upgrade-microtask,open,read,complete'");
}

#[test]
fn indexed_db_abort_rolls_back_upgrade_and_write_then_real_delete() {
    run(r#"
        globalThis.done=false;globalThis.log=[];
        const check=(ok,message)=>{if(!ok)throw new Error(message)};
        const first=indexedDB.open('abort',1);
        first.onupgradeneeded=()=>{
            const db=first.result,tx=first.transaction;
            db.addEventListener('error',()=>{throw new Error('open request acquired event parent')},true);
            db.addEventListener('abort',e=>{check(e.target===tx && e.bubbles,'abort bubbles to database');log.push('abort-db');});
            tx.onabort=()=>{
                check(tx.db===db,'aborted transaction database identity');log.push('abort-tx');
                Promise.resolve().then(()=>log.push('abort-microtask'));
            };
            db.createObjectStore('discarded');tx.abort();
        };
        first.onerror=e=>{
            check(first.error.name==='AbortError','upgrade abort request');log.push('upgrade-abort');
            const retry=indexedDB.open('abort');
            retry.onupgradeneeded=e=>{check(e.oldVersion===0,'aborted database absent');retry.result.createObjectStore('kept').put('original',1);};
            retry.onsuccess=()=>{
                const db=retry.result,tx=db.transaction('kept','readwrite');
                tx.objectStore('kept').put('changed',1).onsuccess=()=>tx.abort();
                tx.onabort=()=>{
                    const read=db.transaction('kept');read.objectStore('kept').get(1).onsuccess=e=>check(e.target.result==='original','aborted write invisible');
                    read.oncomplete=()=>{db.close();const deletion=indexedDB.deleteDatabase('abort');deletion.onsuccess=e=>{check(e.oldVersion===1&&e.newVersion===null,'delete version event');log.push('deleted');done=true;};};
                };
            };
        };
    "#, "done && log.join(',')==='abort-tx,abort-db,abort-microtask,upgrade-abort,deleted'");
}

#[test]
fn indexed_db_real_versionchange_blocked_and_close_resume() {
    run(r#"
        globalThis.done=false;globalThis.log=[];
        const check=(ok,message)=>{if(!ok)throw new Error(message)};
        const first=indexedDB.open('blocked',1);
        first.onupgradeneeded=()=>first.result.createObjectStore('items');
        first.onsuccess=()=>{
            const db=first.result;
            db.onversionchange=e=>{check(e.oldVersion===1&&e.newVersion===2,'versionchange');log.push('versionchange');};
            const second=indexedDB.open('blocked',2);
            second.onblocked=e=>{check(second.readyState==='pending','blocked still pending');log.push('blocked');db.close();};
            second.onupgradeneeded=e=>{check(e.oldVersion===1,'actual upgrade');log.push('upgrade');};
            second.onsuccess=()=>{second.result.close();log.push('success');done=true;};
        };
    "#, "done && log.join(',')==='versionchange,blocked,upgrade,success'");
}

#[test]
fn indexed_db_constraint_error_bubbles_and_cancellation_preserves_transaction() {
    run(r#"
        globalThis.done=false;globalThis.log=[];
        const check=(ok,message)=>{if(!ok)throw new Error(message)};
        const opening=indexedDB.open('cancel-error');
        opening.onupgradeneeded=()=>{
            const db=opening.result,tx=opening.transaction,store=db.createObjectStore('items');
            store.add('first',1);
            const duplicate=store.add('duplicate',1);
            duplicate.onerror=e=>{check(e.bubbles&&e.cancelable&&duplicate.error.name==='ConstraintError','real duplicate error');log.push('request-error');};
            tx.onerror=e=>{check(e.target===duplicate,'transaction sees original target');e.preventDefault();log.push('transaction-error');};
            db.addEventListener('error',e=>{check(e.target===duplicate&&e.defaultPrevented,'database sees canceled error');log.push('db-error');});
            tx.onabort=()=>{throw new Error('canceled error aborted')};
            tx.oncomplete=()=>log.push('complete');
        };
        opening.onsuccess=()=>{opening.result.close();done=true;};
    "#, "done && log.join(',')==='request-error,transaction-error,db-error,complete'");
}

#[test]
fn indexed_db_upgrade_handler_exception_reports_and_aborts_schema() {
    run(r#"
        globalThis.done=false;globalThis.log=[];
        const check=(ok,message)=>{if(!ok)throw new Error(message)};
        const sentinel=new Error('intentional upgrade failure');
        addEventListener('error',e=>{check(e.error===sentinel,'original exception report');e.preventDefault();log.push('reported');});
        const opening=indexedDB.open('throw-upgrade');let abortedDB;
        opening.onupgradeneeded=()=>{abortedDB=opening.result;abortedDB.createObjectStore('discarded');throw sentinel;};
        opening.onerror=()=>{
            check(opening.error.name==='AbortError'&&opening.result===undefined&&opening.transaction===null,'failed open state');
            check(abortedDB.version===0&&!abortedDB.objectStoreNames.contains('discarded'),'aborted connection schema/version');
            const retry=indexedDB.open('throw-upgrade');
            retry.onupgradeneeded=e=>{check(e.oldVersion===0,'schema never committed');};
            retry.onsuccess=()=>{retry.result.close();log.push('retry');done=true;};
        };
    "#, "done && log.join(',')==='reported,retry'");
}
