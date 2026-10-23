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
