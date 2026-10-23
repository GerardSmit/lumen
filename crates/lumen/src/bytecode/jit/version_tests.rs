//! Loops whose invariant guards the optimizer checks once ahead of a fast copy (see
//! `lumen_codegen::version`): every assumption broken mid-loop must send the rest of the run
//! back through the checks.

use super::str_static_tests::agree;

#[test]
fn global_callee_rebound_mid_loop() {
    agree(
        "(function(){
           globalThis.vf=function(a,b){return a+b};
           function run(n,at){var s=0;for(var i=0;i<n;i++){if(i===at)vf=function(a,b){return a*2-b};s=vf(s,i)|0}return s}
           var r=[];for(var k=0;k<20;k++)r.push(run(300,-1));
           r.push(run(3000,1700));r.push(run(3000,-1));
           globalThis.vf=function(a,b){return a+b};
           r.push(run(3000,-1));
           return r.join(',')})()",
        None,
        false,
    );
}

#[test]
fn property_added_mid_loop_changes_the_shape() {
    agree(
        "(function(){
           function run(n,at){var o={a:1,b:2};var s=0;
             for(var i=0;i<n;i++){s+=o.a+o.b;if(i===at){o.c=3;o.a=10}if(i===at+500)delete o.b}
             return s+':'+Object.keys(o).join('')}
           var r=[];for(var k=0;k<20;k++)r.push(run(300,-1000));
           r.push(run(3000,1000));r.push(run(3000,-1000));
           return r.join(',')})()",
        None,
        false,
    );
}

#[test]
fn pushing_to_the_array_being_iterated() {
    agree(
        "(function(){
           function run(cap){var a=[1,2,3];var s=0;
             for(var i=0;i<a.length;i++){s+=a[i];if(a.length<cap)a.push(i&7)}return s+':'+a.length}
           function grow(a,n){var s=0;for(var i=0;i<n;i++){s+=a[i&3]|0;if(i%97===0)a.push(i)}return s+':'+a.length}
           var r=[];for(var k=0;k<20;k++)r.push(run(300));
           r.push(run(5000));
           var b=[1,2,3,4];for(var k=0;k<10;k++)r.push(grow(b,400));
           return r.join(',')})()",
        None,
        false,
    );
}

#[test]
fn zero_iteration_loops_with_failing_guards() {
    agree(
        "(function(){
           function run(o,a,n){var s=0;for(var i=0;i<n;i++){s+=o.x+a[0]}return s}
           function len(a,n){var s=0;for(var i=0;i<a.length&&i<n;i++){s+=a[i].x}return s}
           var r=[];
           for(var k=0;k<30;k++)r.push(run({x:k},[k],50+k%3));
           r.push(run(null,null,0),run(undefined,[],0),run({x:1},[],0),run({y:1},[5],3),run(7,'s',0));
           for(var k=0;k<30;k++)r.push(len([{x:k},{x:1}],100));
           r.push(len([],100),len([{y:1}],0),len([{x:2}],0));
           try{r.push(len([null],1))}catch(e){r.push(e.constructor.name)}
           try{r.push(run(null,[1],1))}catch(e){r.push(e.constructor.name)}
           return r.join(',')})()",
        None,
        false,
    );
}

#[test]
fn getters_that_mutate_what_the_loop_reads() {
    agree(
        "(function(){
           function run(n){var k=0;var o={y:1};
             Object.defineProperty(o,'x',{get:function(){k++;if(k===500)o.y=7;return k}});
             var s=0;for(var i=0;i<n;i++){s+=o.x+o.y}return s}
           function shrink(a,objs){var s=0;for(var i=0;i<a.length;i++){s+=a[i]+objs[i%5].v}return s+':'+a.length}
           var r=[];for(var j=0;j<10;j++)r.push(run(2000));
           var objs=[];for(var j=0;j<5;j++)objs.push({v:j});
           for(var j=0;j<20;j++){var a=[];for(var m=0;m<200;m++)a.push(m);r.push(shrink(a,objs))}
           var a=[];for(var m=0;m<200;m++)a.push(m);
           objs[2]={get v(){a.length=a.length>100?a.length-7:a.length;return 1}};
           r.push(shrink(a,objs));
           return r.join(',')})()",
        None,
        false,
    );
}

#[test]
fn typed_array_detached_mid_loop() {
    agree(
        "(function(){
           function detach(b){
             try{if(typeof b.transfer==='function')b.transfer();else structuredClone(b,{transfer:[b]})}catch(e){}
           }
           function sum(t,objs,n){var s=0;for(var i=0;i<n;i++){s+=(t[i&63]|0)+objs[i%7].v}return s}
           function plain(t,n){var s=0;for(var i=0;i<n;i++){s+=t[i&63]|0}return s}
           var buf=new ArrayBuffer(64);var t=new Uint8Array(buf);for(var i=0;i<64;i++)t[i]=i;
           var objs=[];for(var k=0;k<7;k++)objs.push({v:k});
           var r=[];for(var k=0;k<20;k++)r.push(sum(t,objs,500),plain(t,300));
           objs[3]={get v(){detach(buf);return 100}};
           r.push(sum(t,objs,500),t.length,plain(t,300),sum(t,objs,500));
           return r.join(',')})()",
        None,
        false,
    );
}

#[test]
fn hoisted_global_array_reassigned_by_a_slow_path() {
    agree(
        "(function(){
           globalThis.va=[];for(var i=0;i<100;i++)va.push({x:i});
           function run(objs,n){var s=0;for(var r=0;r<n;r++)for(var i=0;i<va.length;i++){s+=va[i].x+objs[i%3].v}return s}
           var objs=[{v:1},{v:2},{v:3}];
           var out=[];for(var k=0;k<10;k++)out.push(run(objs,5));
           objs[1]={get v(){if(va.length>50)va=va.slice(0,50);return 2}};
           out.push(run(objs,5),va.length);
           return out.join(',')})()",
        None,
        false,
    );
}
