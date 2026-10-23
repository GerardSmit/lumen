use super::str_static_tests::agree;
use crate::{bytecode::Tier, Completion, Engine, JitMode};

/// `src` on the bytecode tier with the JIT in its default (hot) mode and tier threshold.
/// (Sources that must inline are strict: a sloppy callee is not inlined once any source in the
/// process could read `f.arguments`, see `reflect::enabled`.)
fn hot(src: &str) -> (String, crate::JitStats) {
    let mut engine = Engine::new();
    engine.set_tier(Tier::Bytecode);
    engine.set_jit_mode(JitMode::Hot);
    match engine.eval(src, false).expect("parse") {
        Completion::Value(v) => (v, engine.jit_stats()),
        Completion::Throw { name, message } => panic!("threw {name}: {message}"),
    }
}

#[test]
fn hot_inlining_in_a_hot_function() {
    let src = "(function(){'use strict';function add(a,b){return a+b}
               function sum(n){var s=0;for(var i=0;i<n;i++){s=add(s,i)}return s}
               var t=0;for(var k=0;k<2000;k++){t+=sum(10)}return t})()";
    let (got, stats) = hot(src);
    assert_eq!(got, "90000");
    assert!(stats.inlined_calls > 0, "no call was inlined: {stats:?}");
}

#[test]
fn hot_inlining_of_a_global_function() {
    let src = "'use strict';var o={a:1};
               function gget(p,x){return p.a+x}
               function gsum(n){var s=0;for(var i=0;i<n;i++){s=gget(o,s)}return s}
               var t=0;for(var k=0;k<2000;k++){t+=gsum(10)}t";
    let (got, stats) = hot(src);
    assert_eq!(got, "20000");
    assert!(stats.inlined_calls > 0, "no call was inlined: {stats:?}");
}

#[test]
fn small_pure_helpers() {
    agree(
        "(function(){
           function add(a,b){return a+b}
           function clamp(x,lo,hi){return x<lo?lo:x>hi?hi:x}
           function len(x,y){return Math.sqrt(x*x+y*y)}
           var s=0,c=0,l=0;
           for(var i=0;i<4000;i++){s=add(s,i);c+=clamp(i-2000,-5,5);l+=len(i%7,i%11)}
           return s+','+c+','+l.toFixed(6)})()",
        None,
        true,
    );
}

#[test]
fn inlining_happens() {
    let src = "(function(){'use strict';function add(a,b){return a+b}
               var s=0;for(var i=0;i<1000;i++){s=add(s,i)}return s})()";
    assert_eq!(agree(src, Some("499500"), true), "499500");
    // (Eager code is compiled before `add` exists: the hot loop is compiled once it does.)
    let (got, stats) = hot(&src.replace("1000", "100000"));
    assert_eq!(got, "4999950000");
    assert!(stats.inlined_calls > 0, "no call was inlined: {stats:?}");
}

#[test]
fn methods_and_getters_read_and_store_properties() {
    agree(
        "(function(){
           function P(x,y){this.x=x;this.y=y}
           P.prototype.norm1=function(){return Math.abs(this.x)+Math.abs(this.y)};
           P.prototype.move=function(dx){this.x+=dx;return this};
           Object.defineProperty(P.prototype,'sum',{get:function(){return this.x+this.y}});
           var p=new P(1,2),t=0;
           for(var i=0;i<3000;i++){p.move(i%3-1);t+=p.norm1()+p.sum}
           return t+','+p.x+','+p.y})()",
        None,
        true,
    );
}

#[test]
fn callee_with_a_loop() {
    agree(
        "(function(){
           function tri(n){var s=0;for(var k=0;k<n;k++){s+=k}return s}
           var t=0;for(var i=0;i<2000;i++){t+=tri(i%50)}return t})()",
        None,
        true,
    );
}

#[test]
fn nested_calls() {
    agree(
        "(function(){
           function sq(x){return x*x}
           function sumsq(a,b){return sq(a)+sq(b)}
           function dist2(a,b,c){return sumsq(a,b)+sq(c)}
           var t=0;for(var i=0;i<3000;i++){t+=dist2(i%5,i%7,i%3)}return t})()",
        None,
        true,
    );
}

#[test]
fn callee_throw_is_caught_with_its_frame_in_the_stack() {
    agree(
        "(function(){
           function innerThrower(x){if(x===2500)throw new Error('boom '+x);return x+1}
           var s=0,seen='';
           for(var i=0;i<3000;i++){
             try{s+=innerThrower(i)}catch(e){seen=e.message+':'+(e.stack.indexOf('innerThrower')>=0)}
           }
           return s+','+seen})()",
        None,
        true,
    );
}

#[test]
fn deopt_number_argument_becomes_string() {
    agree(
        "(function(){
           function f(a,b){return a+b}
           var r=[];
           for(var i=0;i<3000;i++){var v=f(i<2000?i:'s'+i,1);if(i%500===0||i===2001)r.push(v)}
           return r.join('|')})()",
        None,
        true,
    );
}

#[test]
fn deopt_shape_change_mid_body() {
    agree(
        "(function(){
           function get(o){return o.a+o.b}
           var t=0,o={a:1,b:2};
           for(var i=0;i<3000;i++){
             if(i===1500)o={b:5,a:7,c:1};
             if(i===2200){delete o.c;o.d=3}
             t+=get(o)}
           return t})()",
        None,
        true,
    );
}

#[test]
fn deopt_array_becomes_typed_array() {
    agree(
        "(function(){
           function sum3(a){return a[0]+a[1]+a[2]}
           var a=[1,2,3],ta=new Float64Array([0.5,1.5,2.5]),t=0;
           for(var i=0;i<3000;i++){t+=sum3(i<1800?a:ta)}
           return t})()",
        None,
        true,
    );
}

#[test]
fn closures_read_captured_variables() {
    agree(
        "(function(){
           var k=3;
           function addk(x){return x+k}
           var t=0;
           for(var i=0;i<3000;i++){if(i===1000)k=10;if(i===2000)k='z';t=addk(typeof t==='number'?t%100000:1)}
           return t})()",
        None,
        true,
    );
}

#[test]
fn closures_made_per_iteration() {
    agree(
        "(function(){
           function mk(n){return function(x){return x*n}}
           var fs=[mk(2),mk(3)],t=0;
           for(var i=0;i<3000;i++){t+=fs[i&1](i)}
           return t})()",
        None,
        true,
    );
}

#[test]
fn polymorphic_site_alternates_callees() {
    agree(
        "(function(){
           function a(x){return x+1}
           function b(x){return x*2}
           var fs=[a,b],t=0;
           for(var i=0;i<3000;i++){var f=fs[i%2];t+=f(i%100)}
           return t})()",
        None,
        true,
    );
}

#[test]
fn callee_redefined_after_warmup() {
    agree(
        "(function(){
           var f=function(x){return x+1};
           var other=function(x){return x-1};
           var t=0;
           for(var i=0;i<3000;i++){if(i===2000)f=other;t+=f(i)}
           return t})()",
        None,
        true,
    );
}

#[test]
fn recursion_is_not_inlined_into_itself() {
    agree(
        "(function(){
           function fib(n){return n<2?n:fib(n-1)+fib(n-2)}
           var t=0;for(var i=0;i<30;i++){t+=fib(i%20)}return t})()",
        None,
        true,
    );
}

#[test]
fn sloppy_and_strict_this_with_primitive_receiver() {
    agree(
        "(function(){
           Number.prototype.sloppyT=function(){return typeof this};
           Number.prototype.strictT=function(){'use strict';return typeof this};
           function plain(){return this===undefined?'u':typeof this}
           function strictPlain(){'use strict';return this===undefined?'u':typeof this}
           var r='';
           for(var i=0;i<2000;i++){
             var n=i;
             var s=n.sloppyT()+n.strictT()+plain()+strictPlain();
             if(i%999===0)r+=s+';'}
           return r})()",
        None,
        true,
    );
}

#[test]
fn arguments_length_callee_still_correct() {
    agree(
        "(function(){
           function cnt(){return arguments.length}
           function first(a){return arguments.length>1?a+arguments[1]:a}
           var t=0;for(var i=0;i<3000;i++){t+=cnt(i,i,i)+first(i,i%3)+first(i)}
           return t})()",
        None,
        true,
    );
}

#[test]
fn default_params() {
    agree(
        "(function(){
           function f(a,b=10,c=a+b){return a+b+c}
           var t=0;for(var i=0;i<3000;i++){t+=i%3===0?f(i):i%3===1?f(i,1):f(i,1,2)}
           return t})()",
        None,
        true,
    );
}

#[test]
fn getter_in_body_changes_callers_objects() {
    agree(
        "(function(){
           var o={n:0,k:1};
           var g={get v(){o.n+=1;if(o.n===1500){o.k='x';o.extra=1}return 2}};
           function use(h){return h.v+1}
           var t=0,s='';
           for(var i=0;i<3000;i++){t+=use(g);s=o.k+':'+o.n}
           return t+','+s+','+o.extra})()",
        None,
        true,
    );
}

#[test]
fn gc_during_inlined_body() {
    agree(
        "(function(){
           function make(i){var a=[];for(var k=0;k<20;k++){a.push({i:i,k:k,s:'v'+k})}return a[i%20].k+a.length}
           var t=0,keep={x:1,y:[1,2,3]};
           for(var i=0;i<6000;i++){t+=make(i)+keep.y[i%3]}
           return t+','+keep.y.join()})()",
        None,
        true,
    );
}

#[test]
fn deep_stack_overflow_is_a_range_error() {
    on_big_stack(deep_stack_overflow);
}

fn deep_stack_overflow() {
    agree(
        "(function(){
           function down(n){return n===0?0:1+step(n-1)}
           function step(n){return down(n)}
           var r=down(100)+'';
           try{down(1e7);r+=':no'}catch(e){r+=':'+(e instanceof RangeError)}
           return r+','+down(100)})()",
        None,
        false,
    );
}

#[test]
fn function_caller_sees_the_inlined_frame() {
    agree(
        "(function(){
           function inspect(){return inspect.caller===outer}
           function outer(){return inspect()}
           var t=0;for(var i=0;i<2000;i++){if(outer())t++}
           return t})()",
        None,
        true,
    );
}

#[test]
fn overflows_out_of_inlined_bodies_leave_the_depth_as_it_was() {
    on_big_stack(overflows_leave_the_depth);
}

fn overflows_leave_the_depth() {
    let src = "(function(){'use strict';
           function down(n){return n===0?0:1+step(n-1)}
           function step(n){return 0+down(n)}
           function deep(n){return 0+down(n)}
           function ok(n){try{down(n);return true}catch(e){if(e instanceof RangeError)return false;throw e}}
           function most(){var lo=0,hi=1<<20;while(lo<hi){var m=(lo+hi+1)>>1;if(ok(m))lo=m;else hi=m-1}return lo}
           var w=0;for(var i=0;i<3000;i++){w+=down(i%8)}
           var before=most(),c=0;
           for(var k=0;k<30;k++){try{deep(1e7)}catch(e){if(e instanceof RangeError)c++}}
           var after=most();
           return w+','+c+','+(before>50)+','+(after+2>=before||after+'<'+before)})()";
    assert_eq!(agree(src, None, false), "10500,30,true,true");
    let (got, stats) = hot(src);
    assert_eq!(got, "10500,30,true,true");
    assert!(stats.inlined_calls > 0, "no call was inlined: {stats:?}");
}

#[test]
fn frameless_leaf_keeps_the_callers_array_cache() {
    let src = "(function(){'use strict';function P(x){this.x=x}
           function add(a,b){return a+b}
           var arr=[];for(var i=0;i<100;i++)arr.push(new P(i));
           var s=0;for(var r=0;r<300;r++)for(var j=0;j<arr.length;j++){s=add(s,arr[j].x)}
           return s})()";
    assert_eq!(agree(src, Some("1485000"), true), "1485000");
    let (got, stats) = hot(src);
    assert_eq!(got, "1485000");
    assert!(stats.inlined_calls > 0, "no call was inlined: {stats:?}");
}

#[test]
fn frameless_body_that_calls_a_helper_commits_its_frame() {
    agree(
        "(function(){'use strict';
           function cat(a,b){return a+b}
           var n=0,g={get v(){n++;return 'ab'}};
           function pick(o){return o.v}
           var s='',t=0;for(var i=0;i<3000;i++){s=cat('x',i);t+=pick(g).length}
           return s+','+t+','+n})()",
        None,
        true,
    );
}

#[test]
fn frameless_body_throw_has_the_callee_in_its_stack() {
    agree(
        "(function(){'use strict';
           function bad(o){return o.x.y}
           var r=0,st='';
           for(var i=0;i<3000;i++){try{r+=bad(i===2900?{}:{x:{y:1}})}catch(e){st=String(e.stack).indexOf('bad')>=0}}
           return r+','+st})()",
        None,
        true,
    );
}

#[test]
fn frameless_body_exit_after_a_kind_change_finishes_the_call() {
    agree(
        "(function(){'use strict';
           function f(a,b){return a*2+b}
           var s=0,q='';for(var i=0;i<4000;i++){var v=f(i,1);if(typeof v==='number')s+=v;else q+=v}
           q=f('a','b');
           for(var k=0;k<4000;k++){var w=f(k%3===0?'z':k,1);if(typeof w==='number')s+=w;else q+=w.length}
           return s+','+q.length})()",
        None,
        true,
    );
}

#[test]
fn frameless_calls_still_hit_the_recursion_limit() {
    let src = "(function(){'use strict';
           function leaf(n){return n+1}
           function down(n){return n===0?leaf(0):down(n-1)+0}
           var c=0;for(var i=0;i<20;i++){try{down(1e7)}catch(e){if(e instanceof RangeError)c++}}
           var w=0;for(var k=0;k<3000;k++){w+=down(k%5)}
           return c+','+w})()";
    assert_eq!(agree(src, None, false), "20,3000");
}

#[test]
fn frameless_getter_on_the_receiver_runs_with_the_callee_frame() {
    agree(
        "(function(){'use strict';
           var log=0;
           function getx(o){return o.x}
           var plain={x:1};
           var acc={get x(){log++;return 2}};
           var s=0;for(var i=0;i<4000;i++){s+=getx(i%100===0?acc:plain)}
           return s+','+log})()",
        None,
        true,
    );
}

#[test]
fn strict_tail_call_sites_inline() {
    let src = "(function(){'use strict';
           function add(a,b){return a+b}
           function g(a,b){return add(a,b)}
           var s=0;for(var i=0;i<4000;i++){s=g(s,i)}
           return s})()";
    assert_eq!(agree(src, Some("7998000"), true), "7998000");
}

#[test]
fn exits_past_the_limit_stop_inlining_the_site() {
    agree(
        "(function(){'use strict';
           function f(a){return a.k+1}
           var shapes=[];for(var j=0;j<40;j++){var o={};o['p'+j]=j;o.k=j;shapes.push(o)}
           var s=0;for(var r=0;r<200;r++)for(var i=0;i<40;i++){s+=f(r<100?shapes[0]:shapes[i])}
           return s})()",
        None,
        false,
    );
}

/// Debug-build interpreter frames are large; the default 2 MiB test thread is too small.
fn on_big_stack(f: fn()) {
    std::thread::Builder::new().stack_size(crate::THREAD_STACK_SIZE).spawn(f).unwrap().join().unwrap();
}
