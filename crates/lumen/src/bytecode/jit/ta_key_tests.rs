use super::str_static_tests::agree;
use super::switch_tests::evaluate;
use crate::{bytecode::Tier, JitMode};

#[test]
fn nested_decode_loops_recompile_and_stay_correct() {
    // Haxe `BaseCode.decodeBytes` with 6-bit symbols: the inner loop runs once or twice per
    // output byte.
    let src = r#"(function(){
          var B64='ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/',hex='0123456789abcdef';
          function table(alpha){var t=[];for(var q=0;q<256;q++)t.push(-1);
            for(var q=0;q<alpha.length;q++)t[alpha.charCodeAt(q)]=q;return t}
          var c6=table(B64),c4=table(hex);
          function mk(n,seed){var x=new Uint8Array(n),s=seed;
            for(var i=0;i<n;i++){s=(Math.imul(s,1103515245)+12345)&0x7fffffff;x[i]=s>>>16&255}return x}
          function enc6(x){var o=[],acc=0,nb=0;
            for(var i=0;i<x.length;i++){acc=acc<<8|x[i];nb+=8;
              while(nb>=6){nb-=6;o.push(B64.charCodeAt(acc>>nb&63))}acc&=(1<<nb)-1}
            if(nb>0)o.push(B64.charCodeAt(acc<<6-nb&63));return new Uint8Array(o)}
          function decode(a,g,size,b,c){var h=0,r=0,k=0,m=0;
            while(m<size){
              for(;8>h;){h+=b;r<<=b;var l=c[a.b[k++]];if(-1==l)throw new Error('bad');r|=l}
              h-=8;g.b[m++]=r>>h&255;
            }}
          var bad=0;
          for(var run=0;run<12;run++){
            var n=run===0?3:20000;
            var x=mk(n,run+1),a={b:enc6(x)},g={b:new Uint8Array(n)};
            decode(a,g,n,6,c6);
            for(var i=0;i<n;i++)if(g.b[i]!==x[i])bad++;
          }
          var known={b:new Uint8Array([100,101,97,100,98,101,101,102])},out={b:new Uint8Array(4)};
          decode(known,out,4,4,c4);
          var caught='';
          try{decode({b:new Uint8Array([49,122])},{b:new Uint8Array(1)},1,4,c4)}catch(e){caught=e.message}
          return bad+'|'+Array.prototype.join.call(out.b,',')+'|'+caught;
        })()"#;
    agree(src, Some("0|222,173,190,239|bad"), false);
    // The outer loop recompiles after its typed-array exit: each decode enters native code a
    // few times rather than once per output byte through the inner loop.
    let (_, stats) = evaluate(src, Tier::Bytecode, JitMode::Hot);
    assert!(
        stats.executed_entries < 20_000,
        "{} native entries",
        stats.executed_entries
    );
}

#[test]
fn checked_int32_sums_keep_number_semantics() {
    agree(
        r#"(function(){
          function add(a,n){var s=0;for(var i=0;i<n;i++)s+=a[i];return s}
          function sub(a,n){var s=0;for(var i=0;i<n;i++)s-=a[i];return s}
          function addU8(a,n){var s=2147483000;for(var i=0;i<n;i++)s+=a[i];return s}
          function subU8(a,n){var s=-2147483000;for(var i=0;i<n;i++)s-=a[i];return s}
          var w=[1,2,3,4],u=new Uint8Array([200,200,200,200]);
          for(var k=0;k<3000;k++){add(w,4);sub(w,4);addU8(u,2);subU8(u,2)}
          var big=[1073741824,1073741824,1073741824,1073741824];
          return [
            add(big,4),add([1,0.5,1],3),add([5,-0],2),1/add([-0],1),
            add([-1,2147483648],2),add([1,NaN],2),add([1,"2",3],3),add([1,2],3),
            sub(big,4),sub([1,0.5,1],3),1/sub([-0],1),sub([-1,2147483648],2),
            sub([1,NaN],2),sub([10,"3"],2),sub([1,2],3),sub([-2147483648],1),
            addU8(u,4),addU8(u,3),subU8(u,4),subU8(u,3)
          ].join('|');
        })()"#,
        Some("4294967296|2.5|5|Infinity|2147483647|NaN|123|NaN|-4294967296|-2.5|Infinity|-2147483647|NaN|-13|NaN|2147483648|2147483800|2147483600|-2147483800|-2147483600"),
        false,
    );
}

#[test]
fn typed_array_values_used_as_element_keys() {
    agree(
        r#"(function(){
          function look(tbl,u,n){var s=0;for(var i=0;i<n;i++){var v=tbl[u[i]];if(v!==undefined)s+=v}return s}
          function lookObj(o,u,n){var s=0;for(var i=0;i<n;i++){var v=o[u[i]];if(v!==undefined)s+=v.length}return s}
          function fill(out,u,n){for(var i=0;i<n;i++)out[u[i]]=i}
          var w=new Uint8Array(64);for(var q=0;q<64;q++)w[q]=q%5;
          var tw=[0,10,20,30,40],ow={1:'a',2:'bb',5:'ccccc'},fw=[];
          for(var k=0;k<400;k++){look(tw,w,64);lookObj(ow,w,64);fill(fw,w,64)}
          var tbl=[0,10,20];tbl[1.5]=7;
          var f=new Float64Array([1,1.5,2,0.5,-0]);
          var nanTbl={NaN:5,"1.5":2,"1":1};
          var fn=new Float64Array([1,1.5,NaN,2]);
          var big=new BigInt64Array([3n,1n,3n]);
          var o2=[];fill(o2,new Uint8Array([3,1,3,0]),4);
          var o3={};fill(o3,new Float64Array([1.5,2]),2);
          var o4=[];fill(o4,new BigInt64Array([2n,0n]),2);
          return [
            look(tw,new Uint8Array([1,2,3,4,0,9,1]),7),
            look(tbl,f,5),look([0,10,20],f,5),look(nanTbl,fn,4),
            look([0,10,20,30],big,3),
            lookObj(ow,new Uint8Array([1,2,5,3,2]),5),
            o2.join(','),JSON.stringify(o3),o4.join(',')
          ].join('|');
        })()"#,
        Some("110|37|30|8|70|10|3,1,,2|{\"2\":1,\"1.5\":0}|1,,0"),
        false,
    );
}

#[test]
fn boxed_element_keys_split_on_numbers() {
    // Keys read from a plain array stay Boxed: Numbers take the element fast paths, anything
    // else the generic one (a typed-array local keeps the key reads unspeculated).
    agree(
        r#"(function(){
          var keys=[0,1,2,'x',-1,1.5,undefined,'length',3,NaN,-0,4294967295,'2',null,true];
          function run(a,c,k,n){var o=new Uint8Array(n),w=[],s='';
            for(var m=0;m<n;m++){var j=m%k.length;
              s+=c[k[j]]+','+a.b[k[j]]+',';
              w[k[j]]=m;o[m]=c[a.b[m&3]]&255}
            return s.length+'|'+Object.keys(w).join()+'|'+w.length+'|'+o[5]}
          var c=[7,8,9,10];c.x='cx';
          var a={b:new Uint8Array([1,2,3,0])};a.b.x='bx';
          var r;for(var t=0;t<300;t++)r=run(a,c,keys,60);
          return r;
        })()"#,
        None,
        true,
    );
}

#[test]
fn chained_element_reads_lend_the_receiver() {
    // `a.b[i]` lends `a.b` to the element read; receivers that are not objects, other shapes
    // and out-of-range keys take the plain paths.
    agree(
        r#"(function(){
          function sum(a,n){var s=0;for(var k=0;k<n;k++)s+=a.b[k];return s}
          function inc(a,n){var s=0,k=0;for(var m=0;m<n;m++)s+=a.b[k++]|0;return s}
          function two(a){var s=0;for(var k=0;k<8;k++)s+=a.b[3];return s}
          var u=new Uint8Array(64);for(var i=0;i<64;i++)u[i]=i*3;
          var out=[];
          for(var t=0;t<400;t++){
            var r=[sum({b:u},64),inc({b:u},70),two({b:u}),
              sum({b:[1,2,3]},3),sum({b:'abc'},3),inc({b:7},3),inc({c:1,b:u},5),
              sum({b:new Float64Array([0.5,1.5])},2)];
            if(t%97==0)out.push(r.join());
          }
          var g={get b(){return u}};
          for(var t=0;t<300;t++)r=sum(g,4);out.push(r);
          function edge(a,k0,n){var s=0,k=k0;for(var m=0;m<n;m++)s+=a.b[k++]|0;return s+'/'+k}
          for(var t=0;t<300;t++)r=edge({b:u},0,10);
          out.push(r,edge({b:u},2147483645,5),edge({b:{2147483646:9,2147483647:4,2147483648:1}},2147483645,5));
          return out.join('|');
        })()"#,
        None,
        true,
    );
}

#[test]
fn boxed_comparisons_stay_inline_and_see_js_side_effects() {
    agree(
        r#"(function(){
          function cmp(xs){var r=0;
            for(var i=0;i<xs.length;i++){var x=xs[i],y=xs[(i+1)%xs.length];
              r=r*3+((null==x)|0)+((x!=undefined)<<1)+((x===y)<<2)+((x==y)<<3)+((x<y)<<4)|0}
            return r}
          var o={},xs=[null,undefined,o,o,'a','a',1,'1',true,{},NaN,0,-0,null];
          var r1;for(var t=0;t<300;t++)r1=cmp(xs);
          function scan(u,o,n){var s=0;for(var i=0;i<n;i++){if(o<i)s+=1;s+=u[i]|0}return s}
          var u=new Uint8Array(new ArrayBuffer(64));for(var i=0;i<64;i++)u[i]=i;
          for(var t=0;t<300;t++)scan(u,{valueOf:function(){return 0}},64);
          var hits=0,o2={valueOf:function(){if(++hits==10)u.buffer.transfer();return 0}};
          function fresh(){var x=new Uint8Array(new ArrayBuffer(64));for(var i=0;i<64;i++)x[i]=i;return x}
          function viaIn(u,k,o,n){var s=0;for(var i=0;i<n;i++){if(k in o)s+=1;s+=u[i]|0}return s}
          function viaPlus(u,o,n){var s=0;for(var i=0;i<n;i++){s+=+o;s+=u[i]|0}return s}
          function viaEq(u,o,n){var s=0;for(var i=0;i<n;i++){if(o==1)s+=1;s+=u[i]|0}return s}
          var plain={valueOf:function(){return 1},toString:function(){return 'x'}},o3={x:1};
          for(var t=0;t<300;t++){var w=fresh();viaIn(w,plain,o3,64);viaPlus(w,plain,64);viaEq(w,plain,64)}
          var out=[];
          [viaIn,viaPlus,viaEq].forEach(function(f){
            var w=fresh(),n=0,k={valueOf:function(){if(++n==10)w.buffer.transfer();return 1},
              toString:function(){if(++n==10)w.buffer.transfer();return 'x'}};
            out.push(f===viaIn?f(w,k,o3,64):f(w,k,64),w.length)});
          return r1+'|'+scan(u,o2,64)+'|'+u.length+'|'+out.join();
        })()"#,
        None,
        true,
    );
}

#[test]
fn property_updates_in_place() {
    agree(
        r#"(function(){
          function C(){this.n=0;this.m=10;this.s='a'}
          C.prototype.step=function(k){var r=0;
            for(var i=0;i<k;i++){this.n++;r+=this.m--;r+=++this.n;r+=this.m++ +(--this.m);this.s++}
            return r};
          var c=new C(),r=0;for(var t=0;t<300;t++)r+=c.step(20);
          var f=Object.freeze(new C()),g=new C();Object.defineProperty(g,'n',{get:function(){return 5},set:function(v){this.m=v}});
          var b=new C();b.n=10n;
          var out=[r,c.n,c.m,c.s,f.step(3),f.n,g.step(3),g.m];
          try{out.push(b.step(2))}catch(e){out.push(e.name)}
          out.push(String(b.n));
          return out.join();
        })()"#,
        None,
        true,
    );
}

#[test]
fn property_updates_across_shapes_and_receivers() {
    agree(
        r#"(function(){
          'use strict';
          function bump(o,k){var r=0;for(var i=0;i<k;i++){o.n++;r+=o.inner.n--;r+=++o.inner.n}return r}
          function mk(){return {n:0,inner:{n:5}}}
          var a=mk(),r=0;for(var t=0;t<300;t++)r+=bump(a,20);
          var out=[r,a.n,a.inner.n];
          var p=Object.create({n:1});p.inner={n:0};out.push(bump(p,3),p.n,Object.getPrototypeOf(p).n);
          var w=mk();Object.defineProperty(w,'n',{value:7,writable:false});
          try{bump(w,2)}catch(e){out.push(e.name,w.n)}
          var f=Object.freeze(mk());try{bump(f,2)}catch(e){out.push(e.name,f.n)}
          var s=mk();delete s.n;out.push(bump(s,2),String(s.n));
          var q={inner:{n:1},n:2};out.push(bump(q,4),q.n,q.inner.n);
          return out.join();
        })()"#,
        None,
        true,
    );
}

#[test]
fn nullish_constant_on_either_side() {
    agree(
        r#"(function(){
          function f(xs){var r=0;
            for(var i=0;i<xs.length;i++){var x=xs[i];
              if(null==x)r+=1;if(undefined!=x)r+=2;if(x==null)r+=4;if(void 0==this)r+=8;
              r+=(null==xs[i])?16:0}
            return r}
          var xs=[null,undefined,0,'',false,{},[],NaN,1n,Symbol(),{valueOf:function(){throw 1}}],r=0;
          for(var t=0;t<300;t++)r+=f.call(t&1?null:{},xs);
          return r;
        })()"#,
        None,
        true,
    );
}
