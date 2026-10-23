use super::str_static_tests::agree;

const SMALL: &str = "
function sm(a,b){return Math.max(Math.floor(a),b)}
function ch(a){return String.fromCharCode(a)}
function cp(a){return String.fromCodePoint(a)}
function flo(a){return Math.floor(a)}
";

fn scenario(body: &str) -> String {
    format!("(function(){{{SMALL}{body}}})()")
}

#[test]
fn small_functions_using_intrinsics_agree_in_hot_loops() {
    agree(
        &scenario(
            "var t=0,s='',u='';
             for(var i=0;i<30000;i++){t+=sm(i*0.5,3)+flo(i/3);
               if(i%997===0){s+=ch(65+i%26);u+=cp(97+i%26);}}
             return t+':'+s+':'+u",
        ),
        None,
        false,
    );
}

#[test]
fn replacing_an_intrinsic_mid_loop_takes_effect() {
    agree(
        &scenario(
            "var of=Math.floor,om=Math.max,oc=String.fromCharCode,op=String.fromCodePoint;
             function run(n,swap){var t=0,s='';for(var i=0;i<n;i++){
               if(i===n/2)swap();
               t+=sm(i*0.5,3)+flo(i/3);s+=ch(65+i%3)+cp(97+i%3);}return t+':'+s.length+':'+s.slice(-8)}
             var a=run(30000,function(){Math.floor=function(x){return 7}});Math.floor=of;
             var b=run(30000,function(){Math.max=function(x,y){return -1}});Math.max=om;
             var c=run(30000,function(){String.fromCharCode=function(x){return 'Q'}});String.fromCharCode=oc;
             var d=run(30000,function(){String.fromCodePoint=function(x){return 'R'}});String.fromCodePoint=op;
             return [a,b,c,d,run(30000,function(){})].join('|')",
        ),
        None,
        false,
    );
}

#[test]
fn replacing_the_whole_receiver_object_mid_loop_takes_effect() {
    agree(
        &scenario(
            "var OM=globalThis.Math,OS=globalThis.String;
             function run(n,swap){var t=0,s='';for(var i=0;i<n;i++){
               if(i===n/2)swap();
               t+=sm(i*0.5,3)+flo(i/3);s+=ch(65+i%3)+cp(97+i%3);}return t+':'+s.length+':'+s.slice(-8)}
             var a=run(30000,function(){globalThis.Math={floor:function(x){return 5},max:OM.max}});globalThis.Math=OM;
             var b=run(30000,function(){globalThis.Math={floor:OM.floor,max:OM.max}});globalThis.Math=OM;
             var c=run(30000,function(){globalThis.String={fromCharCode:function(x){return 'Z'},fromCodePoint:OS.fromCodePoint}});globalThis.String=OS;
             var d=run(30000,function(){globalThis.String={fromCharCode:OS.fromCharCode,fromCodePoint:OS.fromCodePoint}});globalThis.String=OS;
             return [a,b,c,d].join('|')",
        ),
        None,
        false,
    );
}

#[test]
fn an_intrinsic_turned_into_a_getter_mid_loop_takes_effect() {
    agree(
        &scenario(
            "var of=Math.floor,oc=String.fromCharCode,hits=0;
             function run(n,swap){var t=0,s='';for(var i=0;i<n;i++){
               if(i===n/2)swap();
               t+=sm(i*0.5,3)+flo(i/3);s+=ch(65+i%3);}return t+':'+s.length+':'+s.slice(-4)}
             var a=run(30000,function(){Object.defineProperty(Math,'floor',{get:function(){hits++;return function(x){return 2}},configurable:true})});
             Object.defineProperty(Math,'floor',{value:of,writable:true,configurable:true});
             var b=run(30000,function(){Object.defineProperty(String,'fromCharCode',{get:function(){hits++;return function(x){return 'W'}},configurable:true})});
             Object.defineProperty(String,'fromCharCode',{value:oc,writable:true,configurable:true});
             return [a,b,hits>0].join('|')",
        ),
        None,
        false,
    );
}

#[test]
fn deleting_an_intrinsic_mid_loop_throws_where_the_call_does() {
    agree(
        &scenario(
            "var of=Math.floor,oc=String.fromCharCode;
             function run(n,swap){var t=0,s='';try{for(var i=0;i<n;i++){
               if(i===n/2)swap();
               t+=sm(i*0.5,3)+flo(i/3);s+=ch(65+i%3);}}catch(e){return e.name+':'+t+':'+s.length}return t+':'+s.length}
             var a=run(30000,function(){delete String.fromCharCode});String.fromCharCode=oc;
             var b=run(30000,function(){delete Math.floor});Math.floor=of;
             return [a,b,run(30000,function(){})].join('|')",
        ),
        None,
        false,
    );
}

#[test]
fn a_shadowing_var_from_sloppy_eval_takes_effect() {
    agree(
        "(function(){
           function outer(n,shadow){
             function sm(a,b){return Math.max(Math.floor(a),b)}
             function ch(a){return String.fromCharCode(a)}
             function cp(a){return String.fromCodePoint(a)}
             var t=0,s='';
             for(var i=0;i<n;i++){t+=sm(i*0.5,3);s+=ch(65+i%3)+cp(97+i%3);}
             if(shadow)eval('var Math={floor:function(x){return 9},max:function(x,y){return 1}};var String={fromCharCode:function(x){return \"c\"},fromCodePoint:function(x){return \"p\"}}');
             for(var i=0;i<n;i++){t+=sm(i*0.5,3);s+=ch(65+i%3)+cp(97+i%3);}
             return t+':'+s.length+':'+s.slice(-6)}
           return [outer(20000,false),outer(20000,true),outer(20000,false)].join('|')})()",
        None,
        false,
    );
}

const MATH_CALLEES: &str = "
function ff(a){return Math.floor(a*0.5)+Math.max(a,3)}
function fg(a,b){return Math.min(Math.abs(a-b),Math.sqrt(a))}
function fn2(a){return Math.floor(Math.sqrt(a))+Math.ceil(a/3)+Math.trunc(a/7)+Math.round(a/5)}
var G={f:ff,g:fg,n:fn2,
  h:function(a){return Math.floor(a*0.5)+Math.max(a,3)}};
";

#[test]
fn methods_and_functions_using_math_inline_in_hot_loops() {
    let src = format!(
        "{MATH_CALLEES}
         function runG(n){{var s=0;for(var i=0;i<n;i++){{s+=G.h(i)+G.g(i,40)+G.n(i)}}return s}}
         function runL(n){{var o={{f:function(a){{return Math.floor(a*0.5)+Math.max(a,3)}}}};var t=0;
           for(var i=0;i<n;i++){{t+=o.f(i)}}return t}}
         function runF(n){{var s=0;for(var i=0;i<n;i++){{s+=ff(i)+fg(i,9)}}return s}}
         function runS(n){{var g=ff,s=0;for(var i=0;i<n;i++){{s+=g(i)}}return s}}
         [runG(30000),runL(30000),runF(30000),runS(30000),runG(3),runL(3)].join('|')"
    );
    agree(&src, None, false);
}

#[test]
fn a_method_loop_over_math_runs_native_code() {
    let src = "var o={f:function(a){return Math.floor(a*0.5)+Math.max(a,3)}};
               var s=0;for(var i=0;i<200000;i++){s+=o.f(i)}s";
    let (want, _) = super::switch_tests::evaluate(
        src,
        crate::bytecode::Tier::Interp,
        crate::JitMode::Disabled,
    );
    let (got, stats) = super::switch_tests::evaluate(
        src,
        crate::bytecode::Tier::Bytecode,
        crate::JitMode::Eager,
    );
    assert_eq!(got, want);
    assert!(stats.compiled_units > 0 && stats.executed_entries > 0);
}

#[test]
fn inlined_math_bodies_keep_nan_zero_and_infinity_semantics() {
    let src = format!(
        "{MATH_CALLEES}
         var H={{a:function(a,b){{return Math.floor(a)+Math.max(a,b)}},
                b:function(a,b){{return Math.min(a,b)-Math.abs(a)}},
                c:function(a){{return Math.sqrt(a)*Math.ceil(a)}},
                d:function(a,b){{return Math.max(a,b)*1/Math.max(b,a)}},
                e:function(a,b){{return 1/Math.min(a,b)}}}};
         var xs=[0,-0,NaN,Infinity,-Infinity,-1.5,2.5,1e308,-1e-300,0.5,-0.5,3];
         function kind(r){{return Object.is(r,-0)?7:r!==r?11:r===Infinity?13:r===-Infinity?17:(r*1000)|0}}
         function viaUnboxed(n){{var h=0;for(var k=0;k<n;k++){{var x=k*0.25-40,y=k%7-3;
           h=(h*31+kind(H.a(x,y))+kind(H.b(x,y))+kind(H.c(x))+kind(H.d(x,y))+kind(H.e(x,y)))|0}}return h}}
         function viaBoxed(n){{var h=0;for(var k=0;k<n;k++){{var x=xs[k%xs.length],y=xs[(k*5+1)%xs.length];
           h=(h*31+kind(H.a(x,y))+kind(H.b(x,y))+kind(H.c(x))+kind(H.d(x,y))+kind(H.e(x,y)))|0}}return h}}
         function viaFunc(n){{var h=0;for(var k=0;k<n;k++){{var x=xs[k%xs.length];
           h=(h*31+kind(ff(x))+kind(fg(x,2))+kind(fn2(x)))|0}}return h}}
         function mixed(n){{var h=0;for(var k=0;k<n;k++){{var x=k===n-2?'7':k===n-1?undefined:k*0.5;
           h=(h*31+kind(G.h(x))+kind(ff(x)))|0}}return h}}
         [viaUnboxed(9000),viaBoxed(9000),viaFunc(9000),mixed(9000)].join('|')"
    );
    agree(&src, None, false);
}

#[test]
fn replacing_a_method_mid_loop_calls_the_new_one() {
    agree(
        "function run(n,swap){var o={f:function(a){return Math.floor(a*0.5)+Math.max(a,3)}};var t=0;
           for(var i=0;i<n;i++){if(i===n/2)swap(o);t+=o.f(i)}return t}
         var G={f:function(a){return Math.floor(a*0.5)+Math.max(a,3)}};
         function runG(n,swap){var t=0;for(var i=0;i<n;i++){if(i===n/2)swap();t+=G.f(i)}return t}
         var keep=G.f;
         [run(30000,function(o){o.f=function(a){return -a}}),
          run(30000,function(o){o.f=function(a){return Math.ceil(a)}}),
          run(30000,function(o){}),
          runG(30000,function(){G.f=function(a){return a+1}}),
          (G.f=keep,runG(30000,function(){G.f=function(a){return Math.min(a,5)}})),
          (G.f=keep,runG(30000,function(){delete G.f;G.__proto__={f:function(a){return 2*a}}})),
          (G.f=keep,runG(30000,function(){}))].join('|')",
        None,
        false,
    );
}

#[test]
fn replacing_math_under_an_inlined_body_takes_effect() {
    agree(
        &format!(
            "{MATH_CALLEES}
             var of=Math.floor,om=Math.max,OM=globalThis.Math;
             function run(n,swap){{var t=0;for(var i=0;i<n;i++){{
               if(i===n/2)swap();t+=G.h(i)+ff(i)+G.f(i)}}return t}}
             function runL(n,swap){{var o={{f:function(a){{return Math.floor(a*0.5)+Math.max(a,3)}}}},t=0;
               for(var i=0;i<n;i++){{if(i===n/2)swap();t+=o.f(i)}}return t}}
             var r=[];
             r.push(run(30000,function(){{Math.floor=function(x){{return 7}}}}));Math.floor=of;
             r.push(run(30000,function(){{Math.max=function(x,y){{return -1}}}}));Math.max=om;
             r.push(run(30000,function(){{globalThis.Math={{floor:function(x){{return 5}},max:OM.max}}}}));globalThis.Math=OM;
             r.push(run(30000,function(){{globalThis.Math={{floor:OM.floor,max:OM.max}}}}));globalThis.Math=OM;
             r.push(run(30000,function(){{Object.defineProperty(Math,'floor',{{get:function(){{return function(x){{return 2}}}},configurable:true}})}}));
             Object.defineProperty(Math,'floor',{{value:of,writable:true,configurable:true}});
             r.push(runL(30000,function(){{Math.floor=function(x){{return 3}}}}));Math.floor=of;
             r.push(runL(30000,function(){{delete Math.max;Math.max=function(x,y){{return 8}}}}));Math.max=om;
             r.push(run(30000,function(){{}}),runL(30000,function(){{}}));
             r.join('|')"
        ),
        None,
        false,
    );
}

#[test]
fn a_shadowing_math_in_the_callee_scope_takes_effect() {
    agree(
        "(function(){
           function outer(n,shadow){
             var o={f:function(a){return Math.floor(a*0.5)+Math.max(a,3)}};
             function ff(a){return Math.floor(a*0.5)+Math.max(a,3)}
             var t=0;
             for(var i=0;i<n;i++){t+=o.f(i)+ff(i)}
             if(shadow)eval('var Math={floor:function(x){return 9},max:function(x,y){return 1}}');
             for(var i=0;i<n;i++){t+=o.f(i)+ff(i)}
             return t}
           function mk(Math){return {f:function(a){return Math.floor(a*0.5)+Math.max(a,3)}}}
           function viaParam(n,m){var o=mk(m),t=0;for(var i=0;i<n;i++){t+=o.f(i)}return t}
           var fake={floor:function(x){return 4},max:function(x,y){return 2}};
           return [outer(20000,false),outer(20000,true),outer(20000,false),
                   viaParam(20000,Math),viaParam(20000,fake),viaParam(20000,Math)].join('|')})()",
        None,
        false,
    );
}

#[test]
fn methods_reading_this_next_to_math_stay_correct() {
    agree(
        "(function(){
           function Box(k){this.k=k}
           Box.prototype.scaled=function(a){return Math.floor(a*this.k)+Math.max(a,this.k)};
           Box.prototype.self=function(a){return this===undefined?-1:Math.floor(a)+(this.k|0)};
           var o={k:3,f:function(a){return Math.floor(a*0.5)+this.k},
                  g:function(a){return Math.floor(a*0.5)+(this===o?1:0)}};
           function run(n,b){var t=0;for(var i=0;i<n;i++){t+=o.f(i)+o.g(i)+b.scaled(i)+b.self(i)}return t}
           var bb=new Box(2.5);
           return [run(20000,bb),run(20000,new Box(1)),run(5,bb)].join('|')})()",
        None,
        false,
    );
}
