//! String-keyed element reads probed inline against the interpreter's `str_key_ic` memo, the
//! guard-first property update, and the name-cache checks classified as environment memory:
//! every way the memo or a hoisted check can go stale mid-loop must agree with the interpreter.

use super::str_static_tests::agree;

#[test]
fn getter_installed_on_the_read_property_mid_loop() {
    agree(
        r#"(function(){
          function get(o,k){return o[k]}
          function run(o,n,at){var s=0,calls=0;
            for(var i=0;i<n;i++){
              if(i===at)Object.defineProperty(o,'x',{get:function(){calls++;return 100},configurable:true});
              s+=get(o,i&1?'x':'y')}
            return s+':'+calls}
          var r=[];for(var k=0;k<20;k++)r.push(run({x:1,y:2},400,-1));
          r.push(run({x:1,y:2},4000,1500));
          var h={};function H(){this.h=h}H.prototype.at=function(k){return this.h[k]};
          var m=new H();h.a=1;h.b=2;var t=0;
          for(var i=0;i<20000;i++){if(i===9000)Object.defineProperty(h,'a',{get:function(){return 7}});t+=m.at(i&1?'a':'b')}
          r.push(t);
          return r.join(',')})()"#,
        None,
        false,
    );
}

#[test]
fn proxies_take_the_trap() {
    agree(
        r#"(function(){
          function get(o,k){return o[k]}
          var traps=0,p=new Proxy({x:1,y:2},{get:function(t,k){traps++;return k==='x'?50:t[k]}});
          var s=0;
          for(var i=0;i<6000;i++){var o=i<3000||i&1?{x:1,y:2}:p;s+=get(o,i&2?'x':'y')}
          function H(h){this.h=h}H.prototype.at=function(k){return this.h[k]};
          var a=new H({x:3}),b=new H(p),t=0;
          for(var i=0;i<20000;i++)t+=(i<15000?a:b).at('x');
          return s+':'+t+':'+traps})()"#,
        None,
        false,
    );
}

#[test]
fn shape_changes_and_deletes_mid_loop() {
    agree(
        r#"(function(){
          function get(o,k){return o[k]}
          function run(n,at){var o={a:1,b:2,c:3},ks=['a','b','c'],s=0,u=0;
            for(var i=0;i<n;i++){
              if(i===at)o.d=4;
              if(i===at+200)delete o.b;
              if(i===at+400){o.b=20;delete o.a}
              if(i===at+600)o.a='s';
              var v=get(o,ks[i%3]);if(typeof v==='number')s+=v;else if(v===undefined)u++;else u+=1000}
            return s+':'+u+':'+Object.keys(o).join('')}
          var r=[];for(var k=0;k<20;k++)r.push(run(300,-1000));
          r.push(run(4000,1000),run(4000,-1000));
          function H(){this.h={p:1,q:2}}H.prototype.at=function(k){return this.h[k]};
          var m=new H(),t=0,w=0;
          for(var i=0;i<20000;i++){if(i===5000)delete m.h.p;if(i===9000)m.h.p=9;if(i===12000)m.h.z=0;
            var v=m.at(i&1?'p':'q');if(v===undefined)w++;else t+=v}
          r.push(t+'/'+w);
          return r.join(',')})()"#,
        None,
        false,
    );
}

#[test]
fn non_string_and_numeric_string_keys() {
    agree(
        r#"(function(){
          function get(o,k){return o[k]}
          var sym=Symbol('s'),key={toString:function(){return 'x'}};
          var o={x:1,'0':2,'10':3,'01':4,'1e1':5,'-0':6};o[sym]=7;o[true]=8;o[null]=9;o[undefined]=10;
          var a=[11,12,13];a.x=14;
          var ks=['x','0','10','01','1e1','-0',sym,key,true,null,undefined,0,10,1.5,-0,'length','2',2];
          var out=[];
          for(var r=0;r<400;r++)for(var j=0;j<ks.length;j++){var v=get(r&1?a:o,ks[j]);if(r>=398)out.push(String(v))}
          function H(h){this.h=h}H.prototype.at=function(k){return this.h[k]};
          var m=new H(o),n=new H(a),s='';
          for(var r=0;r<2000;r++){var k=ks[r%ks.length];var v=(r&1?n:m).at(k);if(r>=2000-36)s+=String(v)+'|'}
          return out.join(',')+'#'+s})()"#,
        None,
        false,
    );
}

#[test]
fn prototype_chain_changes_mid_loop() {
    agree(
        r#"(function(){
          function get(o,k){return o[k]}
          var p1={z:1,w:2},p2={z:10},o=Object.create(p1);o.own=5;
          var s=0,u=0;
          for(var i=0;i<12000;i++){
            if(i===3000)p1.z=3;
            if(i===5000)Object.setPrototypeOf(o,p2);
            if(i===7000)p2.w=40;
            if(i===9000)o.z=100;
            if(i===10000)Object.setPrototypeOf(o,null);
            var v=get(o,['z','w','own'][i%3]);if(v===undefined)u++;else s+=v}
          return s+':'+u})()"#,
        None,
        false,
    );
}

#[test]
fn frozen_and_non_writable_properties() {
    agree(
        r#"(function(){
          function get(o,k){return o[k]}
          function bump(o,n){for(var i=0;i<n;i++)o.k++;return o.k}
          function sbump(o,n){'use strict';try{for(var i=0;i<n;i++)o.k--}catch(e){return e.constructor.name+o.k}return o.k}
          var r=[];
          for(var j=0;j<20;j++)r.push(bump({k:0},300),sbump({k:0},300));
          var f=Object.freeze({k:1,x:2});r.push(bump(f,300),sbump(f,300));
          var w={k:1};Object.defineProperty(w,'k',{writable:false});r.push(bump(w,300),sbump(w,300));
          var g={k:0};var t=0;for(var i=0;i<3000;i++){if(i===1000)Object.freeze(g);g.k++;t+=get(g,'k')}
          r.push(t,g.k);
          return r.join(',')})()"#,
        None,
        false,
    );
}

#[test]
fn updates_through_accessors_and_mixed_values() {
    agree(
        r#"(function(){
          function run(o,n){for(var i=0;i<n;i++)o.k--;return String(o.k)}
          var r=[];
          for(var j=0;j<20;j++)r.push(run({k:1000},300));
          var hits=0,a={get k(){hits++;return 7},set k(v){hits+=v}};r.push(run(a,300),hits);
          r.push(run({k:'12'},300),run({k:null},300),run({k:undefined},300),run({k:{valueOf:function(){hits++;return 3}}},300),hits);
          var proto={set k(v){hits+=1000}},c=Object.create(proto);r.push(run(c,300),hits,Object.keys(c).length);
          var d={k:5};function mid(o,n){for(var i=0;i<n;i++){if(i===150){o.z=1;delete o.k;o.k=0.5}o.k++}return o.k}
          r.push(mid(d,300));
          function stop(o,n){'use strict';for(var i=0;i<n;i++){if(i===200)Object.defineProperty(o,'k',{get:function(){return 1}});o.k++}return o.k}
          try{r.push(stop({k:0},300))}catch(e){r.push(e.constructor.name)}
          return r.join(',')})()"#,
        None,
        false,
    );
}

#[test]
fn setters_on_the_prototype() {
    agree(
        r#"(function(){
          var log=0,proto={set x(v){log+=v}};
          function put(o,v){o.x=v;return o.x}
          var s=0;
          for(var i=0;i<6000;i++){var o=i<4000?{x:0}:Object.create(proto);var r=put(o,i);s+=r===undefined?1:r}
          function H(){this.h={}}H.prototype.set=function(k,v){this.h[k]=v};H.prototype.at=function(k){return this.h[k]};
          var m=new H(),t=0;
          for(var i=0;i<20000;i++){if(i===10000)Object.setPrototypeOf(m.h,proto);m.set(i&1?'x':'y',i);var v=m.at('x');t+=v===undefined?1:v}
          return s+':'+log+':'+t})()"#,
        None,
        false,
    );
}

#[test]
fn many_keys_and_shapes_collide_in_the_memo() {
    agree(
        r#"(function(){
          function get(o,k){return o[k]}
          var objs=[],ks=[];
          for(var i=0;i<64;i++){var o={};for(var j=0;j<=(i%9);j++)o['p'+((i*7+j)%40)]=i*100+j;objs.push(o)}
          for(var i=0;i<1500;i++)ks.push('p'+(i%40));
          var s=0,u=0;
          for(var r=0;r<40000;r++){var v=get(objs[r&63],ks[(r*13)%ks.length]);if(v===undefined)u++;else s+=v}
          var t=0;for(var r=0;r<40000;r++){var k='k'+(r%1200),o={};o[k]=r;t+=get(o,k)}
          return s+':'+u+':'+t})()"#,
        None,
        false,
    );
}

#[test]
fn key_that_is_neither_string_nor_number_in_a_keyed_method() {
    agree(
        r#"(function(){
          function H(){this.h={x:1,'true':2,'null':3}}
          H.prototype.at=function(k){return this.h[k]};
          var m=new H(),sym=Symbol('x');m.h[sym]=4;
          var ks=['x',true,null,undefined,sym,{toString:function(){return 'x'}},1,'x'];
          var s='';for(var r=0;r<16000;r++){var v=m.at(ks[r&7]);if(r>=16000-8)s+=String(v)+'|'}
          return s})()"#,
        None,
        false,
    );
}

#[test]
fn property_sites_first_run_late_in_the_loop() {
    agree(
        r#"(function(){
          function run(n,at){var o={a:1,b:2},s=0;
            for(var i=0;i<n;i++){
              if(i>=at){o.b=o.a+i;o.a++;s+=o.b}else s+=o.a}
            return s+':'+o.a+':'+o.b}
          var r=[];for(var k=0;k<20;k++)r.push(run(300,1e9));
          r.push(run(3000,1500),run(3000,0),run(3000,1e9));
          var q={get a(){return 5},b:0};
          function late(o,n,at){var s=0;for(var i=0;i<n;i++){if(i>=at){o.b++}s+=o.a}return s+':'+o.b}
          for(var k=0;k<20;k++)r.push(late({a:1,b:0},300,1e9));
          r.push(late(q,3000,2000),late(Object.freeze({a:1,b:0}),3000,2000));
          return r.join(',')})()"#,
        None,
        false,
    );
}

#[test]
fn captured_callee_rebound_mid_loop() {
    agree(
        r#"(function(){
          function outer(){
            var f=function(a,b){return a+b};
            function swap(){f=function(a,b){return a*2-b}}
            function run(n,at){var s=0;for(var i=0;i<n;i++){if(i===at)swap();s=f(s,i)|0}return s}
            return run}
          var r=[];var run=outer();
          for(var k=0;k<20;k++)r.push(run(300,-1));
          r.push(run(3000,1700),run(3000,-1));
          var run2=outer();r.push(run2(3000,-1));
          return r.join(',')})()"#,
        None,
        false,
    );
}

#[test]
fn global_callee_shadowed_and_redefined_mid_loop() {
    agree(
        r#"(function(){
          globalThis.gk=function(a){return a+1};
          function run(n,at){var s=0;
            for(var i=0;i<n;i++){
              if(i===at)globalThis.gk=function(a){return a+2};
              if(i===at+300)Object.defineProperty(globalThis,'gk',{value:function(a){return a+3},writable:true,configurable:true});
              s=gk(s)|0}
            return s}
          var r=[];for(var k=0;k<20;k++)r.push(run(300,-1000));
          r.push(run(3000,1000));globalThis.gk=function(a){return a+1};r.push(run(3000,-1000));
          return r.join(',')})()"#,
        None,
        false,
    );
}
