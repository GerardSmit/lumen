//! `new` from native code on an arbitrary callee value (see `helpers::construct`): the same
//! source on every tier must agree.

use super::switch_tests::{agree_on_every_tier, evaluate};
use crate::{bytecode::Tier, JitMode};

fn agree_with_interp(src: &str) {
    let (want, _) = evaluate(src, Tier::Interp, JitMode::Disabled);
    agree_on_every_tier(src, &want, false);
}

#[test]
fn callee_from_property_upvalue_and_array_element() {
    agree_on_every_tier(
        "function P(a,b){var s=0;for(var q=0;q<2;q++)s+=q;this.a=a;this.b=b}
         var holder={C:P,inner:{D:P}};var arr=[P];
         function viaUp(){var k=P;return function(x){return new k(x,1)}}
         function run(){
           var out=0,mk=viaUp();
           for(var i=0;i<40;i++){
             var o1=new holder.C(i,2),o2=new holder.inner.D(i,3),o3=new arr[0](i,4),o4=mk(i);
             out+=o1.a+o1.b+o2.b+o3.b+o4.b+(o1 instanceof P?1:0)+(Object.getPrototypeOf(o3)===P.prototype?1:0)
           }
           return out
         }
         var r=0;for(var j=0;j<5;j++)r+=run();r",
        "6300",
        false,
    );
}

#[test]
fn constructor_shapes_arguments_captures_and_missing_parameters() {
    agree_on_every_tier(
        "function Q(){var s=0;for(var i=0;i<2;i++)s+=i}
         function AR(){var s=0;for(var i=0;i<2;i++)s+=i;this.n=arguments.length}
         function CP(a){var s=0;for(var i=0;i<2;i++)s+=i;this.f=function(){return a}}
         function P(a,b){var s=0;for(var i=0;i<2;i++)s+=i;this.a=a;this.b=b}
         var h={Q:Q,AR:AR,CP:CP,P:P};var res;
         for(var k=0;k<40;k++){
           res=[Object.getPrototypeOf(new h.Q())===Q.prototype,new h.AR(1,2,3).n,new h.CP(9).f(),
                typeof new h.P(1).b,new h.P(1,2,3,4).b].join()
         }
         res",
        "true,3,9,undefined,2",
        false,
    );
}

#[test]
fn prototype_is_read_at_construct_time_with_object_prototype_fallback() {
    agree_on_every_tier(
        "function F(){var s=0;for(var q=0;q<2;q++)s+=q;this.x=s}
         var h={F:F};var kept;
         function run(){
           var res=[];
           F.prototype={tag:'init'};
           var o=new h.F();res.push(Object.getPrototypeOf(o)===F.prototype);
           var p1={tag:'p1'};F.prototype=p1;
           res.push(Object.getPrototypeOf(o)!==p1);
           var o2=new h.F();res.push(o2.tag);
           F.prototype=5;res.push(Object.getPrototypeOf(new h.F())===Object.prototype);
           F.prototype=null;res.push(Object.getPrototypeOf(new h.F())===Object.prototype);
           F.prototype='s';res.push(Object.getPrototypeOf(new h.F())===Object.prototype);
           F.prototype={tag:'p2'};res.push(new h.F().tag);
           return res.join()
         }
         for(var i=0;i<30;i++)kept=run();kept",
        "true,true,p1,true,true,true,p2",
        false,
    );
}

#[test]
fn return_override_follows_object_primitive_and_nothing() {
    agree_on_every_tier(
        "function RO(k){var s=0;for(var q=0;q<2;q++)s+=q;this.v=s;
           if(k===0)return {r:1};if(k===1)return 5;if(k===2)return 'str';if(k===3)return null;
           if(k===4)return undefined;if(k===5)return function(){}}
         var h={RO:RO};
         function run(){
           var r=[];
           for(var k=0;k<=6;k++){
             var o=new h.RO(k);
             r.push(k===0?o.r:k===5?typeof o:(o instanceof RO)+':'+o.v)
           }
           return r.join('|')
         }
         var out;for(var i=0;i<30;i++)out=run();out",
        "1|true:1|true:1|true:1|true:1|function|true:1",
        false,
    );
}

#[test]
fn new_target_is_the_constructor_and_is_restored() {
    agree_on_every_tier(
        "function Leaf(){var s=0;for(var q=0;q<2;q++)s+=q}
         function NT(){var s=0;for(var q=0;q<2;q++)s+=q;
           this.nt=new.target===NT;this.t=typeof new.target;
           new h.Leaf();this.after=new.target===NT}
         function Other(){}
         var h={NT:NT,Leaf:Leaf};
         function readNT(){return typeof new.target}
         function run(){
           var o=new h.NT();var c={};NT.call(c);
           var r3=Reflect.construct(NT,[],Other);
           return [o.nt,o.t,o.after,c.nt,c.t,r3.nt,r3.t,readNT(),Object.getPrototypeOf(r3)===Other.prototype].join()
         }
         var out;for(var i=0;i<30;i++)out=run();out",
        "true,function,true,false,undefined,false,function,undefined,true",
        false,
    );
}

#[test]
fn exceptions_in_the_constructor_unwind_cleanly_and_keep_the_stack() {
    agree_with_interp(
        "function Thrower(k){var s=0;for(var q=0;q<2;q++)s+=q;if(k)throw new Error('boom'+s);this.ok=1}
         var h={Thrower:Thrower};
         function outer(k){return new h.Thrower(k)}
         function chk(){return typeof new.target}
         function run(){
           var r;
           try{outer(1)}catch(e){
             r=e.message+':'+(String(e.stack).indexOf('Thrower')>=0)+':'+(String(e.stack).indexOf('outer')>=0)
           }
           return r+':'+new h.Thrower(0).ok+':'+chk()
         }
         var out;for(var i=0;i<30;i++)out=run();out",
    );
}

#[test]
fn thrown_value_from_a_constructor_reaches_the_catch_with_state_restored() {
    agree_on_every_tier(
        "function T(k){var s=0;for(var q=0;q<2;q++)s+=q;if(k===1)throw 'x';if(k===2)null.z;this.k=k}
         var h={T:T};
         function run(){
           var r=[];
           for(var k=0;k<3;k++){
             try{r.push(new h.T(k).k)}catch(e){r.push(typeof e==='string'?e:e instanceof TypeError)}
           }
           return r.join()+':'+(new h.T(0) instanceof T)
         }
         var out;for(var i=0;i<30;i++)out=run();out",
        "0,x,true:true",
        false,
    );
}

#[test]
fn derived_classes_and_classes_with_fields_still_construct_correctly() {
    agree_on_every_tier(
        "class A{x=1;constructor(a){this.a=a}}
         class B extends A{y=2;constructor(a){super(a+1);this.b=a}}
         function Base(){var s=0;for(var q=0;q<2;q++)s+=q;this.base=s}
         class D extends Base{constructor(){super();this.d=1}}
         class E extends A{}
         var h={A:A,B:B,D:D,E:E};
         function run(){
           var o=new h.A(3),p=new h.B(4),d=new h.D(),e=new h.E(6);
           return [o.x,o.a,p.x,p.y,p.a,p.b,p instanceof A,d.base,d.d,d instanceof Base,e.a,e.x].join()
         }
         var out;for(var i=0;i<30;i++)out=run();out",
        "1,3,1,2,5,4,true,1,1,true,6,1",
        false,
    );
}

#[test]
fn bound_functions_and_proxies_as_constructors() {
    agree_on_every_tier(
        "function BC(a,b){var s=0;for(var q=0;q<2;q++)s+=q;this.a=a;this.b=b;this.nt=new.target===BC}
         var h={bound:BC.bind(null,7),px:new Proxy(BC,{}),
                px2:new Proxy(BC,{construct(t,args,nt){var o=Reflect.construct(t,args,nt);o.viaTrap=true;return o}})};
         function run(){
           var a=new h.bound(8),b=new h.px(1,2),c=new h.px2(3,4);
           return [a.a,a.b,a.nt,'|',b.a,b.b,b.nt,'|',c.a,c.b,c.viaTrap,c.nt,a instanceof BC].join()
         }
         var out;for(var i=0;i<30;i++)out=run();out",
        "7,8,true,|,1,2,false,|,3,4,true,false,true",
        false,
    );
}

#[test]
fn new_on_a_non_constructor_throws_a_type_error() {
    agree_on_every_tier(
        "var o={m(){return 1},gen:function*(){},get g(){return 1}};
         var fns=[()=>1,o.m,async function(){},o.gen,Math.max,5,undefined,Object.getOwnPropertyDescriptor(o,'g').get];
         var h={arrow:fns[0],method:o.m,mx:Math.max};
         function run(){
           var r=[];
           for(var i=0;i<fns.length;i++){try{new fns[i]();r.push('no')}catch(e){r.push(e instanceof TypeError)}}
           try{new h.arrow();r.push('no')}catch(e){r.push(e instanceof TypeError)}
           try{new h.method(1,2);r.push('no')}catch(e){r.push(e instanceof TypeError)}
           try{new h.mx();r.push('no')}catch(e){r.push(e instanceof TypeError)}
           return r.join()
         }
         var out;for(var i=0;i<30;i++)out=run();out",
        "true,true,true,true,true,true,true,true,true,true,true",
        false,
    );
}

#[test]
fn deep_recursion_through_new_and_the_stack_limit() {
    // Debug-build interpreter frames are large; the default 2 MiB test thread is too small.
    std::thread::Builder::new()
        .stack_size(crate::THREAD_STACK_SIZE)
        .spawn(deep_recursion_through_new)
        .unwrap()
        .join()
        .unwrap();
}

fn deep_recursion_through_new() {
    agree_on_every_tier(
        "function Node(d){this.d=d;this.child=d>0?new h.Node(d-1):null}
         var h={Node:Node};
         function depth(o){var n=0;while(o){n++;o=o.child}return n}
         function Inf(){new h.Inf()}
         h.Inf=Inf;
         function run(){
           var r=depth(new h.Node(200));
           try{new h.Inf();r+=':no'}catch(e){r+=':'+(e instanceof RangeError)}
           return r
         }
         var out;for(var i=0;i<5;i++)out=run();out",
        "201:true",
        false,
    );
}

#[test]
fn constructors_called_through_new_tier_up() {
    let src = "function V(i){this.i=i}function W(i){this.i=i;this.j=i+1}
        var h={V:V,W:W};var s=0;
        for(var k=0;k<20000;k++){var o=new h.W(k);s+=o.j-o.i;s+=new h.V(k).i-k}s";
    let (got, stats) = evaluate(src, Tier::Bytecode, JitMode::Hot);
    assert_eq!(got, "20000");
    assert!(stats.compiled_units > 0);
    agree_on_every_tier(src, "20000", false);
}

#[test]
fn lazily_allocated_property_caches_fill_on_every_tier() {
    agree_with_interp(
        "function Pt(x,y){this.x=x;this.y=y}
         function Pt3(x,y,z){this.x=x;this.y=y;this.z=z}
         function cold(o){return o.a+o.b+o.c+o.d}
         function poly(o){return o.x+(o.y|0)}
         function late(o){o.k=(o.k|0)+1;return o.k+(o instanceof Pt?1:0)}
         var shapes=[new Pt(1,2),new Pt3(1,2,3),{x:5,y:6},{y:1,x:2},{q:0,x:3,y:4},{r:0,s:0,x:1,y:1}];
         var s=0;
         for(var i=0;i<3000;i++){s+=poly(shapes[i%6])}
         var obj=new Pt(1,1);
         for(var j=0;j<500;j++){s+=late(obj)}
         s"
    );
}
