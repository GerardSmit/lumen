use super::switch_tests::evaluate;
use crate::{bytecode::Tier, JitMode};

pub(super) fn agree(src: &str, expected: Option<&str>, all_compile: bool) -> String {
    let (want, _) = evaluate(src, Tier::Interp, JitMode::Disabled);
    if let Some(e) = expected {
        assert_eq!(want, e, "interpreter");
    }
    for (tier, mode) in [
        (Tier::Bytecode, JitMode::Disabled),
        (Tier::Bytecode, JitMode::Hot),
    ] {
        assert_eq!(evaluate(src, tier, mode).0, want, "{tier:?} {mode:?}");
    }
    let (got, stats) = evaluate(src, Tier::Bytecode, JitMode::Eager);
    assert_eq!(got, want, "eager JIT");
    assert!(stats.compiled_units > 0, "nothing was compiled");
    if all_compile {
        assert_eq!(stats.failed_compilations, 0);
    }
    want
}

#[test]
fn from_char_code_in_a_loop_builds_the_string() {
    agree(
        "(function(){function f(n){var s='';for(var i=0;i<n;i++){s+=String.fromCharCode(65+i%3);}return s}
         var r=f(6000);return r.slice(0,6)+r.length})()",
        Some("ABCABC6000"),
        true,
    );
    agree(
        "(function(){function f(n){var s='';for(var i=0;i<n;i++){s+=String.fromCodePoint(97+i%26);}return s}
         var r=f(6000);return r.slice(0,6)+r.length})()",
        Some("abcdef6000"),
        true,
    );
}

#[test]
fn from_char_code_covers_every_unit_class() {
    agree(
        "(function(){
           function a(n){var h=0,s='';for(var i=0;i<n;i++){var c=String.fromCharCode((i*37)%70000-300);
             h=(h*31+c.length+c.charCodeAt(0))|0;if(i%101===0)s+=c;}return h+':'+s.length}
           function b(n){var h=0,s='';for(var i=0;i<n;i++){var c=String.fromCodePoint((i*131)%1114112);
             h=(h*31+c.length+c.codePointAt(0))|0;if(i%101===0)s+=c;}return h+':'+s.length}
           function c(n){var h=0;for(var i=0;i<n;i++){
             var u=String.fromCharCode(i*0.5-1000.25);
             var v=String.fromCharCode(0/0);var w=String.fromCharCode(1/0);var x=String.fromCharCode(-i);
             var y=String.fromCharCode(i*65536+65);var z=String.fromCharCode(-0);
             h=(h*31+u.charCodeAt(0)+v.charCodeAt(0)+w.charCodeAt(0)+x.charCodeAt(0)+y.charCodeAt(0)+z.charCodeAt(0))|0;}return h}
           function d(n){var h=0;for(var i=0;i<n;i++){var u=String.fromCodePoint(i%256);var v=String.fromCodePoint(-0);
             var s=String.fromCodePoint(55296+i%2048);h=(h*31+u.charCodeAt(0)+v.length+s.charCodeAt(0))|0;}return h}
           return [a(9000),b(9000),c(5000),d(5000)].join('|')})()",
        None,
        false,
    );
}

#[test]
fn from_code_point_throws_exactly_where_the_call_does() {
    let mut body = String::new();
    for (k, lit) in ["-1", "1.5", "1114112", "0/0", "1/0", "-1e300", "4294967296+65"]
        .iter()
        .enumerate()
    {
        body.push_str(&format!(
            "function g{k}(n,bad){{var s='';for(var i=0;i<n;i++){{var x=i%200;if(i===bad)x={lit};s+=String.fromCodePoint(x);}}return s.length}}
             function t{k}(bad){{try{{return g{k}(6000,bad)}}catch(e){{return e.name+':'+e.message+':'+(e instanceof RangeError)}}}}
             r.push(t{k}(5500),t{k}(-1),t{k}(5500));"
        ));
    }
    let src = format!("(function(){{var r=[];{body}return r.join('|')}})()");
    agree(&src, None, false);
}

#[test]
fn replacing_a_string_static_mid_loop_takes_effect() {
    agree(
        "(function(){
           var log=[];var oc=String.fromCharCode,op=String.fromCodePoint;
           function f(n){var s='';for(var i=0;i<n;i++){
             if(i===3000){String.fromCharCode=function(x){log.push(x);return 'Z'}}
             s+=String.fromCharCode(65+i%3);}return s}
           function g(n){var s='';for(var i=0;i<n;i++){
             if(i===3000){String.fromCodePoint=function(x){return '#'+x}}
             s+=String.fromCodePoint(65+i%3);}return s}
           var a=f(6000);String.fromCharCode=oc;var b=g(6000);String.fromCodePoint=op;
           var c=f(10);String.fromCharCode=oc;
           return a.length+':'+a.slice(2995,3010)+'|'+b.length+':'+b.slice(2995,3010)+'|'+log.length+':'+c})()",
        None,
        false,
    );
}

#[test]
fn string_static_next_to_typed_array_reads_in_a_loop() {
    agree(
        "(function(){
           function h(d,n){var s='',t=0;for(var i=0;i<n;i++){var b=d[i%d.length];
             s+=String.fromCharCode(b);t+=d[(i+1)%d.length];s+=String.fromCodePoint(b);}
             return s.length+':'+t+':'+s.charCodeAt(7)+':'+s.charCodeAt(s.length-1)}
           var u=new Uint8Array(251);for(var k=0;k<251;k++)u[k]=(k*7+3)&255;
           return h(u,9000)+'|'+h(new Uint8Array([200,201,65,66,255,0]),7000)})()",
        None,
        false,
    );
}

const WRAPPED: &str = "
var Ib={fromCharCode:function(a){return String.fromCodePoint(a)}};
function getString(d,a,b){var c=\"\",e=Ib.fromCharCode,r=a;for(a+=b;r<a;)if(b=d[r++],128>b){if(0==b)break;c+=e(b)}else if(224>b)c+=e((b&63)<<6|d[r++]&127);else{var h=d[r++];c+=e((b&31)<<12|(h&127)<<6|d[r++]&127)}return c}
function encode(cps){var out=[];for(var i=0;i<cps.length;i++){var cp=cps[i];
  if(cp<128)out.push(cp);else if(cp<2048){out.push(192|cp>>6,128|cp&63)}else{out.push(224|cp>>12,128|cp>>6&63,128|cp&63)}}
  return new Uint8Array(out)}
function sample(n){var cps=[],ref='';var pool=[65,66,97,32,233,255,1024,2047,2048,19990,55295,57344,65535];
  for(var i=0;i<n;i++){var cp=pool[(i*7+i%5)%pool.length];cps.push(cp);ref+=String.fromCodePoint(cp)}
  return {cps:cps,ref:ref}}
";

#[test]
fn wrapper_around_from_code_point_decodes_utf8() {
    let src = format!(
        "(function(){{{WRAPPED}
           var s=sample(8000),bytes=encode(s.cps);
           var got=getString(bytes,0,bytes.length);
           var got2=getString(bytes,0,bytes.length);
           var ascii=encode([72,101,108,108,111]);
           return [got===s.ref,got2===s.ref,got.length,getString(ascii,0,5),getString(ascii,1,3)].join(',')}})()"
    );
    let r = agree(&src, None, true);
    assert!(r.starts_with("true,true,8000,Hello,ell"), "{r}");
}

#[test]
fn wrapper_calls_survive_other_wrappers_and_replacements() {
    let src = format!(
        "(function(){{{WRAPPED}
           var s=sample(6000),bytes=encode(s.cps);
           var first=getString(bytes,0,bytes.length)===s.ref;
           var Jb={{fromCharCode:function(a){{return String.fromCharCode(a)}}}};
           var Kb={{fromCharCode:function(a){{return String.fromCodePoint(a)+'!'}}}};
           var Sb={{fromCharCode:function(a){{'use strict';return String.fromCodePoint(a)}}}};
           var res=[first];
           var old=Ib;
           for(var w=0;w<3;w++){{Ib=[Jb,Kb,Sb][w];res.push(getString(bytes,0,bytes.length)===s.ref,getString(bytes,0,bytes.length).length)}}
           Ib=old;
           res.push(getString(bytes,0,bytes.length)===s.ref);
           var orig=String.fromCodePoint;
           function swap(n){{var out='';var f=Ib.fromCharCode;for(var i=0;i<n;i++){{
             if(i===3000)String.fromCodePoint=function(a){{return '#'}};out+=f(65+i%3)}}return out}}
           var sw=swap(6000);String.fromCodePoint=orig;
           res.push(sw.length,sw.slice(2998,3004));
           function swap2(n){{var out='';for(var i=0;i<n;i++){{
             if(i===3000)Ib.fromCharCode=function(a){{return 'Y'}};out+=Ib.fromCharCode(65+i%3)}}return out}}
           var keep=Ib.fromCharCode;var sw2=swap2(6000);Ib.fromCharCode=keep;
           res.push(sw2.length,sw2.slice(2998,3004));
           function many(f,n,last){{var c=0;for(var i=0;i<n;i++){{var x=65+i%50;if(i===n-1)x=last;c+=f(x).length}}return c}}
           function tryMany(last){{try{{return many(Ib.fromCharCode,6000,last)}}catch(e){{return e.name+':'+e.message}}}}
           res.push(tryMany(70),tryMany(1114111),tryMany(1114112),tryMany(-5),tryMany(2.5),tryMany(70));
           return res.join(',')}})()"
    );
    agree(&src, None, false);
}

#[test]
fn strict_wrapper_and_method_calls_fold() {
    let src = "(function(){
        var Sb={fromCharCode:function(a){'use strict';return String.fromCharCode(a)}};
        function viaMethod(n){var s='';for(var i=0;i<n;i++){s+=Sb.fromCharCode(65+i%7)}return s}
        function viaLocal(n){var f=Sb.fromCharCode,s='';for(var i=0;i<n;i++){s+=f(97+i%7)}return s}
        function viaName(n){var s='';for(var i=0;i<n;i++){s+=wrap(48+i%10)}return s}
        function wrap(a){return String.fromCodePoint(a)}
        var a=viaMethod(6000),b=viaLocal(6000),c=viaName(6000);
        return a.length+a.slice(0,8)+'|'+b.length+b.slice(0,8)+'|'+c.length+c.slice(0,10)})()";
    agree(src, Some("6000ABCDEFGA|6000abcdefga|60000123456789"), true);
}
