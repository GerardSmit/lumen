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
