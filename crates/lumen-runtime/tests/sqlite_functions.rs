use lumen_runtime::{Completion, Runtime};
fn run(source: &str) -> String {
    match Runtime::new().eval(source).expect("parse") {
        Completion::Value(value) => value,
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}
#[test]
fn scalar_functions_preserve_values_arity_and_native_flags() {
    assert_eq!(
        run(r#"
      const assert=require('node:assert/strict'); const {DatabaseSync}=require('node:sqlite');
      const db=new DatabaseSync(':memory:');
      db.function('date_ms',value=>value===null?null:Date.parse(value));
      assert.equal(db.prepare("SELECT date_ms('2026-01-01T00:00:00Z') value").get().value,1767225600000);
      db.function('binary',value=>value);assert.deepEqual([...db.prepare("SELECT binary(x'00ff') value").get().value],[0,255]);
      db.function('big',{useBigIntArguments:true},value=>value+1n);
      const big=db.prepare('SELECT big(9007199254740992) value');big.setReadBigInts(true);assert.equal(big.get().value,9007199254740993n);
      db.function('count_args',{varargs:true},(...args)=>args.length);assert.equal(db.prepare('SELECT count_args(1,2,3) value').get().value,3);
      db.function('safe',{deterministic:true},value=>value+1);db.exec('CREATE TABLE items(x);CREATE INDEX computed ON items(safe(x))');
      db.function('direct',{directOnly:true},()=>42);db.exec('CREATE VIEW v AS SELECT direct() value');assert.throws(()=>db.prepare('SELECT * FROM v').get());
      db.function('empty',()=>undefined);assert.equal(db.prepare('SELECT empty() value').get().value,null);
      db.function('date_ms',value=>7);assert.equal(db.prepare('SELECT date_ms(null) value').get().value,7);
      db.close();'verified';
    "#),
        "verified"
    );
}
#[test]
fn scalar_errors_preserve_identity_and_reentry_does_not_free_running_statements() {
    assert_eq!(
        run(r#"
      const assert=require('node:assert/strict');const {DatabaseSync}=require('node:sqlite');const db=new DatabaseSync(':memory:');
      const sentinel=new Error('original callback'); db.function('fail',()=>{throw sentinel});
      assert.throws(()=>db.prepare('SELECT fail()').get(),e=>e===sentinel);
      assert.throws(()=>db.exec('SELECT fail()'),e=>e===sentinel);
      db.function('too_big',()=>1n<<80n);assert.throws(()=>db.prepare('SELECT too_big()').get(),RangeError);
      db.function('unsafe',value=>value);assert.throws(()=>db.prepare('SELECT unsafe(9007199254740992)').get(),RangeError);
      db.function('close_db',()=>{db.close()});assert.throws(()=>db.prepare('SELECT close_db()').get(),e=>e.code==='ERR_INVALID_STATE');assert.equal(db.isOpen,true);
      let executing;db.function('self',()=>executing.get());executing=db.prepare('SELECT self()');assert.throws(()=>executing.get(),e=>e.code==='ERR_INVALID_STATE');
      db.function('nested',()=>db.prepare('SELECT 41 value').get().value+1);assert.equal(db.prepare('SELECT nested() value').get().value,42);
      assert.equal(db.prepare('SELECT 3 value').get().value,3);db.close();'verified';
    "#),
        "verified"
    );
}

#[test]
fn scalar_registration_survives_runtime_move_and_async_execution() {
    let mut runtime = Runtime::new();
    runtime.eval("var db=new (require('node:sqlite').DatabaseSync)(':memory:');db.function('plus_one',value=>value+1)").unwrap();
    let mut moved = Box::new(runtime);
    assert!(
        matches!(moved.eval("db.prepare('SELECT plus_one(41) value').get().value").unwrap(), Completion::Value(value) if value == "42")
    );
    moved.eval("var answer=0;setTimeout(async()=>{await Promise.resolve();answer=db.prepare('SELECT plus_one(6) value').get().value;db.close()},0)").unwrap();
    moved.run_to_completion();
    assert!(matches!(moved.eval("answer").unwrap(), Completion::Value(value) if value == "7"));
}

#[cfg(feature = "bundled-sqlite")]
#[test]
fn bundled_sqlite_is_wal_reset_safe_and_custom_library_errors_are_honest() {
    assert_eq!(
        run(r#"
        const assert=require('node:assert/strict');
        const {DatabaseSync}=require('node:sqlite');
        const db=new DatabaseSync(':memory:');
        assert.equal(db.prepare('SELECT sqlite_version() AS version').get().version,'3.51.3');
        db.close();
        'verified';
    "#),
        "verified"
    );
    assert_eq!(
        run(r#"
        const assert=require('node:assert/strict');
        process.env.LUMEN_SQLITE_LIBRARY='/nonexistent/hashset-sqlite-library';
        const {DatabaseSync}=require('node:sqlite');
        assert.throws(()=>new DatabaseSync(':memory:'), /library not found/);
        'verified';
    "#),
        "verified"
    );
}
