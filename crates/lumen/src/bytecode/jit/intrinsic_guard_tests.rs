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
