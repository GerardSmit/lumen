use super::str_static_tests::agree;

#[test]
fn dictionary_getter_returns_numbers() {
    agree(
        r#"(function(){
          function H(){this.h={}}
          H.prototype.get=function(k){return this.h[k]};
          var m=new H(),ks=[];
          for(var i=0;i<64;i++){ks.push('assets/data/k'+i);m.h[ks[i]]=i*3+1}
          var s=0;
          for(var r=0;r<20000;r++)s+=m.get(ks[r&63]);
          return s})()"#,
        None,
        true,
    );
}

#[test]
fn getter_returns_strings_and_objects() {
    agree(
        r#"(function(){
          function H(){this.h={}}
          H.prototype.get=function(k){return this.h[k]};
          var m=new H(),ks=['a','b','c','d'];
          m.h.a='str';m.h.b={v:7};m.h.c=[1,2,3];m.h.d=null;
          var n=0,t='';
          for(var r=0;r<20000;r++){
            var v=m.get(ks[r&3]);
            if(typeof v==='string')t=v;else if(v===null)n+=100;else n+=v.v||v.length;
          }
          var o=m.get('b');o.v=9;
          return n+'|'+t+'|'+m.h.b.v})()"#,
        None,
        true,
    );
}

#[test]
fn missing_key_reads_undefined() {
    agree(
        r#"(function(){
          function H(){this.h={x:1}}
          H.prototype.get=function(k){return this.h[k]};
          var m=new H(),ks=['x','y','toString','__proto__','0'],u=0,s=0;
          for(var r=0;r<20000;r++){
            var v=m.get(ks[r%5]);
            if(v===undefined)u++;else if(typeof v==='number')s+=v;else u+=2;
          }
          return u+'|'+s})()"#,
        None,
        true,
    );
}

#[test]
fn accessor_on_the_dictionary_runs_once_per_read() {
    agree(
        r#"(function(){
          function H(){this.h=Object.create({get hot(){calls++;return 5}})}
          var calls=0;
          H.prototype.get=function(k){return this.h[k]};
          var m=new H(),ks=['hot','cold'],s=0;
          m.h.cold=2;
          for(var r=0;r<20000;r++){var v=m.get(ks[r&1]);s+=v}
          return s+'|'+calls})()"#,
        None,
        true,
    );
}

#[test]
fn dictionary_replaced_mid_loop() {
    agree(
        r#"(function(){
          function H(){this.h={a:1,b:2}}
          H.prototype.get=function(k){return this.h[k]};
          var m=new H(),ks=['a','b'],s=0,u=0;
          for(var r=0;r<30000;r++){
            if(r===10000)m.h=5;
            if(r===15000)m.h=null;
            if(r===20000)m.h='ab';
            if(r===25000)m.h={a:10,b:20};
            var v;
            try{v=m.get(ks[r&1])}catch(e){u++;continue}
            if(typeof v==='number')s+=v;else if(v===undefined)u+=1000;else u+=1000000;
          }
          return s+'|'+u})()"#,
        None,
        true,
    );
}

#[test]
fn integer_keys_on_arrays() {
    agree(
        r#"(function(){
          function A(){this.a=[]}
          A.prototype.at=function(i){return this.a[i]};
          var m=new A(),s=0,u=0;
          for(var i=0;i<50;i++)m.a.push(i*2);
          m.a[10]='x';m.a[11]={q:1};
          for(var r=0;r<20000;r++){
            var v=m.at(r%53);
            if(typeof v==='number')s+=v;else if(v===undefined)u++;else u+=100;
          }
          var f=0.5,w=m.at(f);
          return s+'|'+u+'|'+w+'|'+m.at(-1)+'|'+m.at(1.0)})()"#,
        None,
        true,
    );
}

#[test]
fn computed_number_key() {
    agree(
        r#"(function(){
          function A(){this.a=[5,6,7,8]}
          A.prototype.at=function(i){return this.a[i&3]};
          var m=new A(),s=0;
          for(var r=0;r<20000;r++)s+=m.at(r);
          return s})()"#,
        None,
        true,
    );
}

#[test]
fn getter_with_a_store_and_a_keyed_read() {
    agree(
        r#"(function(){
          function H(){this.h={a:1,b:2};this.n=0}
          H.prototype.get=function(k){this.n=this.n+1;return this.h[k]};
          var m=new H(),ks=['a','b'],s=0;
          for(var r=0;r<20000;r++)s+=m.get(ks[r&1]);
          return s+'|'+m.n})()"#,
        None,
        true,
    );
}

#[test]
fn non_key_arguments_take_the_ordinary_call() {
    agree(
        r#"(function(){
          function H(){this.h={a:1,'true':2,'undefined':3}}
          H.prototype.get=function(k){return this.h[k]};
          var m=new H(),ks=['a',true,undefined,{toString:function(){return 'a'}}],s=0;
          for(var r=0;r<20000;r++){var v=m.get(ks[r&3]);s+=v===undefined?0:v}
          return s})()"#,
        None,
        true,
    );
}

#[test]
fn numeric_consumer_of_mixed_results() {
    agree(
        r#"(function(){
          function H(){this.h={a:1,b:'x',c:{},d:2}}
          H.prototype.get=function(k){return this.h[k]};
          var m=new H(),ks=['a','b','c','d','e'],s=0;
          for(var r=0;r<20000;r++){s=s-m.get(ks[r%5]);if(s!==s)s=0}
          return s})()"#,
        None,
        true,
    );
}

#[test]
fn element_then_property_reads_holes_primitives_and_mixed_arrays() {
    agree(
        "(function(){function P(x){this.x=x}
           var a=[];for(var i=0;i<64;i++)a.push(new P(i));
           var s=0;for(var r=0;r<200;r++)for(var j=0;j<a.length;j++){s+=a[j].x}
           var h=a.slice();delete h[5];h[9]=7;h[11]='str';h[13]={x:'q'};
           var t='';for(var r2=0;r2<50;r2++)for(var k=0;k<h.length;k++){
             try{var v=h[k].x;t+=typeof v}catch(e){t+='E'}}
           return s+','+t.length+','+t.slice(0,40)})()",
        None,
        true,
    );
}

#[test]
fn element_then_property_reads_see_getters_and_mutation_through_them() {
    agree(
        "(function(){var a=[];
           for(var i=0;i<40;i++)a.push({x:i});
           Object.defineProperty(a[20],'x',{get:function(){a.length=10;return 1000}});
           var s=0;for(var r=0;r<100;r++){for(var j=0;j<a.length;j++){var o=a[j];s+=o?o.x:0}
             if(a.length<40)for(var k=a.length;k<40;k++)a.push({x:k})}
           return s})()",
        None,
        true,
    );
}

#[test]
fn modulo_index_keeps_the_array_view() {
    agree(
        "(function(){var a=[];for(var i=0;i<100;i++)a.push({x:i});
           var s=0;for(var r=0;r<300;r++)for(var j=0;j<a.length;j++){s+=a[(j+1)%100].x}
           return s})()",
        Some("1485000"),
        true,
    );
}

#[test]
fn string_keyed_local_reads_take_the_native_path() {
    agree(
        "(function(){'use strict';function get(o,k){return o[k]}
           var ks=['x','y','z'],o={x:1,y:2},p=Object.create({z:5});p.x=3;
           var s=0,u=0;for(var i=0;i<6000;i++){var v=get(i&1?o:p,ks[i%3]);if(v===undefined)u++;else s+=v}
           return s+','+u})()",
        None,
        true,
    );
}

#[test]
fn method_on_an_element_receiver_is_called_directly() {
    agree(
        "(function(){function P(x,y){this.x=x;this.y=y}
           P.prototype.dot=function(o){return this.x*o.x+this.y*o.y};
           function Q(x){this.x=x;this.y=0}
           Q.prototype.dot=function(o){return -1};
           var a=[];for(var i=0;i<100;i++)a.push(new P(i,i+1));
           var s=0;for(var r=0;r<100;r++){if(r===80)a[3]=new Q(1);
             for(var j=0;j<a.length;j++){s+=a[j].dot(a[j])}}
           return s})()",
        None,
        true,
    );
}
