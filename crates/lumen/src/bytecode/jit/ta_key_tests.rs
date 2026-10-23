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

#[test]
fn element_keys_from_locals_on_dictionaries() {
    agree(
        r#"(function(){
          function H(){this.h={}}
          H.prototype.get=function(k){return this.h[k]};
          H.prototype.set=function(k,v){this.h[k]=v};
          var m=new H(),keys=['a','bb','assets/x','1','',null,2,'toString','__proto__'],r=[];
          for(var t=0;t<300;t++){var k=keys[t%keys.length];var v=m.get(k);
            m.set(k,(typeof v=='number'?v:0)+1);r.push(typeof m.get(k))}
          var a=[1,2,3],o={x:1},s=0;
          for(var t=0;t<300;t++){var k=t%4,j='x';s+=(a[k]|0)+o[j];a[k]=t;o[j]=s&7}
          var g={get z(){return s}},q=0;for(var t=0;t<300;t++){var z='z';q+=g[z]}
          return [r.join(),s,a.join(),o.x,q,Object.keys(m.h).join()].join(';');
        })()"#,
        None,
        true,
    );
}

#[test]
fn string_keys_from_locals_on_exotic_receivers() {
    agree(
        r#"(function(){
          var ta=new Int8Array(4),px=new Proxy({a:1},{get:function(t,k){return k+'!'}}),
              acc={get a(){return 7},set a(v){this.b=v}},arr=[1,2],s=0,out=[];
          var rs=[ta,px,acc,arr,{a:3}];
          for(var t=0;t<300;t++){var k=t%3?'a':'length',r=rs[t%5];
            var v=r[k];s+=typeof v=='number'?v:String(v).length;r[k]=t%7;out.push(k.length)}
          return [s,ta.join(),acc.b,arr.length,out.length].join();
        })()"#,
        None,
        true,
    );
}

#[test]
fn instanceof_native_path_matches_the_interpreter() {
    agree(
        r#"(function(){
          function A(){} function B(){} B.prototype=Object.create(A.prototype);
          class C extends A{}
          var H={[Symbol.hasInstance]:function(x){return x===1}};
          function Even(){} Object.defineProperty(Even,Symbol.hasInstance,{value:function(x){return typeof x=='number'&&x%2===0}});
          var bound=A.bind(null),arrow=function(){},
              xs=['s',1,true,null,undefined,{},[],new A(),new B(),new C(),Symbol('q'),function(){}],
              out=[],bad=0;
          for(var t=0;t<400;t++){
            var x=xs[t%xs.length];
            out.push((x instanceof A?1:0)+(x instanceof B?2:0)+(x instanceof C?4:0)+(x instanceof Object?8:0)
              +(x instanceof Function?16:0)+(x instanceof bound?32:0)+(x instanceof H?64:0)+(x instanceof Even?128:0));
            try{ if(x instanceof arrow) bad+=1000 }catch(e){bad+=1}
            try{ x instanceof 5 }catch(e){bad+=e instanceof TypeError?1:100000}
            try{ x instanceof {} }catch(e){bad+=1}
          }
          var a=new A(),z=0;
          for(var t=0;t<400;t++){ if(t==200) Object.setPrototypeOf(a,B.prototype); if(a instanceof B) z++ }
          return out.slice(0,12).join()+'|'+bad+'|'+z+'|'+out.reduce(function(p,c){return p+c},0);
        })()"#,
        None,
        true,
    );
}

#[test]
fn instanceof_handler_that_throws_or_mutates() {
    agree(
        r#"(function(){
          var n=0;
          var T={[Symbol.hasInstance]:function(x){n++;if(n%50==0)throw new Error('h'+n);return true}};
          var c=0,caught=0;
          for(var t=0;t<300;t++){ try{ if(t instanceof T) c++ }catch(e){caught++} }
          function F(){} var o=new F(),g=0;
          for(var t=0;t<300;t++){ if(t==100) Object.defineProperty(F,Symbol.hasInstance,{value:function(){return false},configurable:true});
            if(o instanceof F) g++ }
          return [c,caught,n,g].join();
        })()"#,
        None,
        true,
    );
}

#[test]
fn string_key_stores_create_and_overwrite() {
    agree(
        r#"(function(){
          var o={},keys=['a','bb','cc','dd','toString','length','x y'],s=0;
          for(var t=0;t<600;t++){ var k=keys[t%keys.length]; o[k]=t; s+=o[k] }
          var r=0;
          for(var t=0;t<600;t++){ var k=keys[t%3]; r=o[k]=t+1; s+=r }
          var q={};
          for(var t=0;t<400;t++){ var k='k'+(t%10); q[k]=(q[k]|0)+1 }
          var inh=Object.create({base:1}),l;
          for(var t=0;t<400;t++){ l='base'; inh[l]=t; l='own'; inh[l]=t }
          return [s,r,Object.keys(o).join(),Object.keys(q).join(),inh.base,Object.keys(inh).join(),
                  Object.getPrototypeOf(inh).base].join('|');
        })()"#,
        None,
        true,
    );
}

#[test]
fn string_key_stores_respect_attributes_and_prototype_chain() {
    agree(
        r#"(function(){
          var log=[],out=[];
          var frozen=Object.freeze({a:1}),sealed=Object.seal({a:1}),ne=Object.preventExtensions({a:1});
          var setterProto={set s(v){log.push('set'+v)},get s(){return 'g'}};
          var viaSetter=Object.create(setterProto);
          var roProto=Object.create(Object.defineProperty({},'ro',{value:7,writable:false}));
          var ro2=Object.create(roProto);
          var swr=Object.create(Object.defineProperty({},'w',{value:1,writable:true,configurable:true}));
          var strict=function(o,k,v){'use strict';try{o[k]=v;return 'ok'}catch(e){return e.name}};
          var sloppy=function(o,k,v){o[k]=v;return 'ok'};
          for(var t=0;t<400;t++){
            var v=t;
            out.push(sloppy(frozen,'a',v)+sloppy(frozen,'n',v)+sloppy(sealed,'a',v)+sloppy(sealed,'n',v)
              +sloppy(ne,'a',v)+sloppy(ne,'n',v)+sloppy(viaSetter,'s',v)+sloppy(roProto,'ro',v)+sloppy(ro2,'ro',v)+sloppy(swr,'w',v));
          }
          var se=[];
          for(var t=0;t<400;t++){
            se.push([strict(frozen,'a',t),strict(frozen,'n',t),strict(sealed,'n',t),strict(ne,'n',t),
                     strict(viaSetter,'s',t),strict(ro2,'ro',t),strict(swr,'w',t)].join());
          }
          return [frozen.a,frozen.n,sealed.a,sealed.n,ne.a,ne.n,log.length,viaSetter.hasOwnProperty('s'),roProto.ro,ro2.ro,
                  swr.w,swr.hasOwnProperty('w'),out[399],se[399],out.length].join('|');
        })()"#,
        None,
        true,
    );
}

#[test]
fn string_key_stores_special_keys_and_receivers() {
    agree(
        r#"(function(){
          var o={},p={},arr=[1,2,3],ta=new Uint8Array(4),fn=function(){},
              px=new Proxy({},{set:function(t,k,v){t[k]=v+1;return true}}),
              keys=['__proto__','0','1','07','-1','1e3','NaN','Infinity','',' '],s=0;
          for(var t=0;t<500;t++){
            var k=keys[t%keys.length];
            o[k]=p; arr[k]=t; ta[k]=t; fn[k]=t; px[k]=t;
            var av=arr[k],tv=ta[k];s+=(typeof o[k]=='object'?1:0)+(typeof av=='number'?av:0)+(typeof tv=='number'?tv:0);
          }
          var lk='len';
          for(var t=0;t<500;t++){ arr['length']=3; lk='length'; arr[lk]=3; ta[lk]=1 }
          return [s,Object.getPrototypeOf(o)===p,Object.keys(o).join(),arr.join(),arr.length,ta.join(),
                  fn['1'],fn['07'],px.__proto__===Object.prototype,Object.keys(px).length].join('|');
        })()"#,
        None,
        true,
    );
}

#[test]
fn string_key_stores_with_the_key_in_a_local_and_as_an_expression() {
    agree(
        r#"(function(){
          function loc(o,n){ var k='f'+n,s=0; for(var i=0;i<n;i++){ o[k]=i; s+=o[k] } return s }
          function expr(o,k,n){ var r=0; for(var i=0;i<n;i++){ r+=(o[k]=i)+1 } return r }
          function chain(o,k,n){ var a,b; for(var i=0;i<n;i++){ a=b=o[k]=i } return a+b }
          function stack(o,n){ var s=0; for(var i=0;i<n;i++){ o['a'+(i&3)]=i; s+=o.a0|0 } return s }
          function viaThis(n){ for(var i=0;i<n;i++){ this['t'+(i&1)]=i } return this.t0+this.t1 }
          var a={},b={f:1},c=Object.freeze({z:1});
          return [loc(a,400),loc({},400),expr(b,'f',400),expr(c,'z',400),chain({},'q',400),stack({},400),
                  viaThis.call({},400),JSON.stringify(a),b.f,c.z,Object.keys(a).length].join('|');
        })()"#,
        None,
        true,
    );
}

#[test]
fn instanceof_bound_function_with_own_prototype() {
    agree(
        r#"(function(){
          function A(){} function B(){}
          var bb=B.bind(null);bb.prototype=A.prototype;
          var a=new A(),b=new B(),n=0;
          for(var t=0;t<400;t++){var x=t&1?a:b;if(x instanceof A)n+=1;if(x instanceof bb)n+=10}
          return n;
        })()"#,
        None,
        true,
    );
}

#[test]
fn string_key_store_replacing_a_global_array() {
    agree(
        r#"(function(){
          var g=globalThis,s=0,k='__jitArr';g[k]=[1,2,3];
          for(var t=0;t<400;t++){s+=__jitArr[t%3];if(t%50==49)g[k]=[t,t+1,t+2]}
          delete g[k];
          return s;
        })()"#,
        None,
        true,
    );
}

#[test]
fn set_prop_as_expression_keeps_the_value() {
    agree(
        r#"(function(){
          function P(){this.a=0;this.b=0}
          var log=[];
          Object.defineProperty(P.prototype,'s',{set:function(v){log.push(v)},get:function(){return 7},configurable:true});
          var o=new P(),r=0,out=[];
          for(var i=0;i<400;i++){
            r+=(o.a=i);
            var x,y;
            x=o.a=o.b=i+1;
            r+=x+o.a+o.b;
            var s=(o.a=''+i);
            r+=s.length;
            var q={k:i};
            var z=(o.b=q);
            r+=z.k+o.b.k;
            r+=(o.s=i);
            var t=(o.fresh=i*2);
            r+=t+o.fresh;
            y=(o.a=o.b=s);
            r+=y.length;
          }
          var fz=Object.freeze({a:1}),fr=0;
          for(var i=0;i<400;i++){fr+=(fz.a=i)+(fz.n=i)}
          var strictThrew=0;
          var st=function(){'use strict';var f=Object.freeze({a:1}),n=0;
            for(var i=0;i<400;i++){try{n+=(f.a=i)}catch(e){strictThrew++}}return n};
          function C(i){this.v=i;this.w=(this.u=i+1)}
          var cs=0;
          for(var i=0;i<400;i++){var c=new C(i);cs+=c.v+c.u+c.w}
          var strs=[];
          for(var i=0;i<400;i++){var w={};var v=(w.p='k'+i);strs.push(v+w.p)}
          return [r,o.a,o.b,log.length,fr,fz.a,st(),strictThrew,cs,strs[399],typeof o.fresh].join('|');
        })()"#,
        None,
        true,
    );
}

#[test]
fn set_prop_expression_with_receiver_kinds() {
    agree(
        r#"(function(){
          var g={n:0},arr=[1,2,3],s=0;
          function f(o,v){return o.x=v}
          function h(v){return this.x=v}
          var h1={h:h};
          for(var i=0;i<400;i++){
            s+=f(g,i);
            s+=f(arr,i);
            var str=f(g,'a'+i);
            s+=str.length;
            var ob=f(g,{i:i});
            s+=ob.i;
            s+=h1.h(i);
            s+=(arr.length=3);
            var p=f(1,i);
            s+=p;
          }
          return [s,g.x.i,arr.x].join('|');
        })()"#,
        None,
        true,
    );
}

#[test]
fn boxed_local_copied_into_numeric_local() {
    agree(
        r#"(function(){
          function run(a,n,sw){
            var b=a[0],t=0,x=0;
            for(var i=0;i<n;i++){
              var cur=b;
              x=cur;
              t+=x;
              if(i===sw)b='s';
              if(i===sw+5)b=1;
            }
            return [t,typeof x].join(',');
          }
          var r=[];
          r.push(run([1.5],600,10000));
          r.push(run([2],600,300));
          return r.join('|');
        })()"#,
        None,
        true,
    );
    agree(
        r#"(function(){
          var arr=[3,4,5],t=0;
          function f(o,n){var k=0,m=o.a;for(var i=0;i<n;i++){k=m;t+=k;if(i==350)m=o}return k}
          var ret=f({a:2},600);
          return t+'|'+typeof ret;
        })()"#,
        None,
        true,
    );
}

#[test]
fn mod_on_boxed_numbers_and_other_operands() {
    agree(
        r#"(function(){
          var vals=[7,-7,0,-0,NaN,Infinity,-Infinity,5.5,-5.5,2147483648,'7','abc',null,undefined,true,{valueOf:function(){return 4}}];
          var divs=[2,-2,0,-0,3.5,Infinity,NaN,'3',1];
          var out=[];
          function m(a,b){return a%b}
          for(var r=0;r<5;r++){
            out.length=0;
            for(var i=0;i<vals.length;i++)for(var j=0;j<divs.length;j++){
              var v=m(vals[i],divs[j]);
              out.push(Object.is(v,-0)?'-0':String(v));
            }
          }
          var big=0,caught=[];
          function mb(a,b){return a%b}
          for(var t=0;t<400;t++){
            big+=Number(mb(10n,3n));
            try{mb(10n,3)}catch(e){caught.push(e instanceof TypeError)}
            try{mb(3,10n)}catch(e){caught.push(e instanceof TypeError)}
          }
          function loop(a,n){var s=0;for(var i=1;i<=n;i++)s+=a[i%a.length]%(i+0.5);return s}
          var arr=[10,20.5,-30,40];
          var ls=0;for(var k=0;k<3;k++)ls=loop(arr,500);
          return out.join(',')+'|'+big+'|'+caught.length+caught[0]+'|'+ls;
        })()"#,
        None,
        true,
    );
}

#[test]
fn string_add_with_boxed_operands() {
    agree(
        r#"(function(){
          var sym=Symbol('q'),caught=0,out=[];
          var vo={valueOf:function(){return 5}},ts={toString:function(){return 'T'}};
          function add(a,b){return a+b}
          var cases=[['a','b'],['Loaded ','asset'],['x',1],[1,'x'],['',''],['s',vo],[vo,'s'],['s',ts],[ts,'s'],[1,2],[vo,vo],['a',null],[undefined,'u'],['a',sym],[sym,'a'],[[1,2],'z'],['k',{}]];
          for(var r=0;r<60;r++){
            out.length=0;caught=0;
            for(var i=0;i<cases.length;i++){
              try{out.push(String(add(cases[i][0],cases[i][1])))}catch(e){caught+=e instanceof TypeError?1:100}
            }
          }
          var acc='',n=0;
          for(var i=0;i<400;i++){var p='item'+(i%7);var s=add('Loaded asset ',p);n+=s.length;if(i<3)acc+=add(s,'!')}
          return out.join(';')+'|'+caught+'|'+n+'|'+acc;
        })()"#,
        None,
        true,
    );
}

#[test]
fn loop_resume_exits_widen_and_recompile() {
    agree(
        r#"(function(){
          var src=[],s=0,a=0,b=1;
          for(var i=0;i<4000;i++)src.push(i<1000?i:(i%3?null:'x'));
          for(var i=0;i<src.length;i++){var v=src[i];a=v;b=a;s+=(b==null?1:typeof b=='string'?2:b)}
          return s+'|'+a+'|'+b;
        })()"#,
        None,
        true,
    );
}

#[test]
fn chained_typed_array_sums_across_kinds() {
    agree(
        r#"(function(){
          function t1(a,n){var s=0;for(var k=0;k<n;k++)s+=a.b[k];return s}
          var N=4096,u=new Uint8Array(N),f=new Float64Array(N),h=new Int16Array(N);
          for(var i=0;i<N;i++){u[i]=i&255;f[i]=i*0.5;h[i]=(i*7)-9000}
          var o={b:u},p={b:f},q={b:h},out=[];
          for(var r=0;r<40;r++){out.length=0;out.push(t1(o,N),t1(p,N),t1(q,N))}
          return out.join('|');
        })()"#,
        None,
        true,
    );
}

#[test]
fn chained_read_cache_follows_receivers_and_mixed_kinds() {
    agree(
        r#"(function(){
          function t1(a,n){var s=0;for(var k=0;k<n;k++)s+=a.b[k];return s}
          var u=new Uint8Array(64),f=new Float32Array(64),w=new Uint32Array(64),arr=[];
          for(var i=0;i<64;i++){u[i]=i;f[i]=i/4;w[i]=4294967295-i;arr.push(i*3)}
          var recvs=[{b:u},{x:1,b:f},{b:w},{y:0,z:1,b:u},{b:arr},{b:f},{b:u}],out=[];
          for(var r=0;r<30;r++){
            out.length=0;
            for(var j=0;j<recvs.length;j++)out.push(t1(recvs[j],64));
            out.push(t1({b:u},64),t1({b:'abc'},3),t1({b:undefined},0));
          }
          var caught='';
          try{t1({b:null},2)}catch(e){caught=e instanceof TypeError}
          return out.join('|')+'|'+caught;
        })()"#,
        None,
        true,
    );
}

#[test]
fn chained_read_cache_dropped_when_a_getter_swaps_the_property() {
    agree(
        r#"(function(){
          function t(a,g,n){var s=0;for(var k=0;k<n;k++){s+=a.b[k];s+=g.x}return s}
          var u=new Uint8Array(32),f=new Float64Array(32),h=new Int16Array(32);
          for(var i=0;i<32;i++){u[i]=i+1;f[i]=(i+1)*100.5;h[i]=-(i+1)*1000}
          var o={b:u},calls=0,seq=[f,h,u,[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31,32]];
          var quiet={x:0};
          var g={get x(){calls++;if(calls%5===0)o.b=seq[(calls/5)%seq.length|0];return 1}};
          var out=[];
          for(var r=0;r<40;r++)out.push(t(o,quiet,32));
          for(var r=0;r<12;r++){o.b=u;out.push(t(o,g,32))}
          out.push(t(o,quiet,32),calls);
          return out.join('|');
        })()"#,
        None,
        true,
    );
}

#[test]
fn chained_read_cache_dropped_when_the_receiver_property_is_replaced_by_js_in_the_loop() {
    agree(
        r#"(function(){
          function t(a,g,n){var s=0;for(var k=0;k<n;k++){s+=a.b[k];if(k===3)s+=g.x}return s}
          var u=new Uint8Array(16),f=new Float64Array(16);
          for(var i=0;i<16;i++){u[i]=i;f[i]=1000+i}
          var o={b:u},mode=0;
          var g={get x(){if(mode)o.b=o.b===u?f:u;return 0}};
          var out=[];
          for(var r=0;r<60;r++)out.push(t(o,g,16));
          mode=1;
          for(var r=0;r<20;r++){o.b=u;out.push(t(o,g,16))}
          return out.join('|');
        })()"#,
        None,
        true,
    );
}

#[test]
fn chained_read_with_a_reassigned_receiver_slot() {
    agree(
        r#"(function(){
          function t(a,c,n){var s=0;for(var k=0;k<n;k++){s+=a.b[k];if(k%2)a=c;else a=a===c?c:a}return s}
          function t2(a,c,n){var s=0;for(var k=0;k<n;k++){s+=a.b[k];var tmp=a;a=c;c=tmp}return s}
          var u=new Uint8Array(40),f=new Float64Array(40);
          for(var i=0;i<40;i++){u[i]=i;f[i]=1000+i}
          var out=[];
          for(var r=0;r<40;r++){out.length=0;out.push(t({b:u},{b:f},40),t2({b:u},{b:f},40),t2({b:f},{b:u},39))}
          return out.join('|');
        })()"#,
        None,
        true,
    );
}

#[test]
fn chained_read_loop_with_property_stores() {
    agree(
        r#"(function(){
          function t(a,n){var s=0;for(var k=0;k<n;k++){s+=a.b[k];if(k===5)a.b=a.c}return s}
          function t3(a,n,v){var s=0;for(var k=0;k<n;k++){s+=a.b[k];a.n=k;a.b=v}return s}
          var u=new Uint8Array(20),f=new Float64Array(20);
          for(var i=0;i<20;i++){u[i]=i+1;f[i]=(i+1)*1000}
          var out=[];
          for(var r=0;r<40;r++){out.length=0;out.push(t({b:u,c:f},20),t({b:f,c:u},20),t3({b:u},20,f),t3({b:f},20,u))}
          return out.join('|');
        })()"#,
        None,
        true,
    );
}

#[test]
fn chained_read_loop_with_number_key_stores() {
    agree(
        r#"(function(){
          var tbl=[];for(var b=0;b<256;b++)tbl[b]=-1;for(var b=0;b<64;b++)tbl[65+b]=b;
          var n=1<<12;var u=new Uint8Array(n);for(var i=0;i<n;i++)u[i]=65+(i*7)%64;
          var o={b:u};
          function t4(a,c){var o=new Uint8Array(n);for(var m=0;m<n;m++){o[m]=c[a.b[m]]&255}
            var s=0;for(var m=0;m<n;m++)s=(s*31+o[m])|0;return s}
          function t5(a,c,n){var r=[];for(var k=0;k<n;k++){r[k]=a.b[k]+c[k&3];c[k&3]=k}return r.join(',')}
          function t6(a,n){var o=new Uint8Array(8),s=0;
            for(var k=0;k<n;k++){var nk=k%3===0?1.5:k%3===1?NaN:k&7;o[nk]=9;s+=a.b[k]+o[1]*1000}return s+'|'+o.join(',')}
          var out=[];
          for(var r=0;r<40;r++){out.length=0;
            out.push(t4(o,tbl),t5({b:[1,2,3,4,5,6]},[10,20,30,40],6),t6({b:u},16))}
          return out.join('#');
        })()"#,
        None,
        true,
    );
}

#[test]
fn chained_read_loop_sees_string_key_store_replacing_the_chained_property() {
    agree(
        r#"(function(){
          function t(a,keys,other,n){var s=0;for(var k=0;k<n;k++){s+=a.b[k];a[keys[k]]=other}return s}
          var u=new Uint8Array(12),f=new Float64Array(12);
          for(var i=0;i<12;i++){u[i]=i+1;f[i]=(i+1)*1000}
          var keys=['z','z','z','b','z','z','z','z','z','z','z','z'];
          var out=[];
          for(var r=0;r<40;r++){out.length=0;
            out.push(t({b:u},keys,f,12),t({b:f},keys,u,12),t({b:u},['z','b','b','z','b','z','b','z','z','z','z','z'],f,12))}
          return out.join('|');
        })()"#,
        None,
        true,
    );
}

#[test]
fn chained_read_loop_sees_nan_key_store_replacing_nan_property() {
    agree(
        r#"(function(){
          function t(a,other,n){var s=0;for(var k=0;k<n;k++){s+=a.NaN[k];var key=k===3?NaN:k+100;a[key]=other}return s}
          function t2(a,other,n){var s=0;for(var k=0;k<n;k++){s+=a.Infinity[k];var key=k===3?1/0:k+100;a[key]=other}return s}
          var u=new Uint8Array(12),f=new Float64Array(12);
          for(var i=0;i<12;i++){u[i]=i+1;f[i]=(i+1)*1000}
          var out=[];
          for(var r=0;r<40;r++){out.length=0;
            out.push(t({NaN:u},f,12),t2({Infinity:u},f,12))}
          return out.join('|');
        })()"#,
        None,
        true,
    );
}
