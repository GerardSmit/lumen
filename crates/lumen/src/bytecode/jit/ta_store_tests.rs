use super::str_static_tests::agree;

const VALUES: &str = "[0,1,127,128,200,254.5,255,255.5,256,257,-1,-128,-129,65535,65536,70000,\
    2147483647,2147483648,4294967295,-2147483649,1.5,-1.5,2.5,0.5,-0.5,NaN,-0,Infinity,-Infinity,\
    1e20,-1e20,'7',null,undefined,true,3.7e9,16777217,0.1]";

#[test]
fn element_stores_convert_per_kind() {
    let src = format!(
        r#"(function(){{
          var vals={VALUES};
          function st(o,v,n){{var L=v.length;for(var i=0;i<n;i++){{o[i%L]=v[i%L]}}}}
          function show(o){{var r=[];for(var i=0;i<o.length;i++){{var x=o[i];r.push(Object.is(x,-0)?'-0':x)}}return r.join(',')}}
          var out=[];
          var kinds=[Int8Array,Uint8Array,Uint8ClampedArray,Int16Array,Uint16Array,Int32Array,Uint32Array,Float32Array,Float64Array];
          for(var round=0;round<3;round++){{
            for(var k=0;k<kinds.length;k++){{
              var o=new kinds[k](vals.length);
              st(o,vals,vals.length*(round===0?2:200));
              out.push(show(o));
            }}
          }}
          return out.join('|');
        }})()"#
    );
    agree(&src, None, true);
}

#[test]
fn element_stores_of_one_kind_stay_specialized() {
    for ctor in [
        "Int8Array",
        "Uint8Array",
        "Uint8ClampedArray",
        "Int16Array",
        "Uint16Array",
        "Int32Array",
        "Float32Array",
        "Float64Array",
    ] {
        let src = format!(
            r#"(function(){{
              var vals={VALUES};
              function st(o,v,n){{var L=v.length;for(var i=0;i<n;i++){{o[i%L]=v[i%L]}}}}
              function show(o){{var r=[];for(var i=0;i<o.length;i++){{var x=o[i];r.push(Object.is(x,-0)?'-0':x)}}return r.join(',')}}
              var o=new {ctor}(vals.length);
              for(var k=0;k<40;k++)st(o,vals,vals.length*8);
              var first=show(o);
              var p=new {ctor}(vals.length);
              st(p,['x',1,2,{{}},4],vals.length*50);
              var q=[];
              st(q,vals,vals.length*50);
              return first+'|'+show(p)+'|'+q.length+'|'+q[3];
            }})()"#
        );
        agree(&src, None, true);
    }
}

#[test]
fn out_of_range_and_odd_keys_do_not_store() {
    let src = r#"(function(){
          function w(o,k,n,v){for(var i=0;i<n;i++){o[k]=v}}
          function wi(o,n,v){for(var i=0;i<n;i++){o[i]=v+i}}
          var out=[];
          var kinds=[Int8Array,Uint8Array,Uint8ClampedArray,Int16Array,Float32Array,Float64Array];
          for(var k=0;k<kinds.length;k++){
            var o=new kinds[k](16);
            wi(o,40,300);
            w(o,-1,4000,5);
            w(o,1.5,4000,6);
            w(o,16,4000,7);
            w(o,4294967296,4000,8);
            w(o,'x',4000,9);
            w(o,'3',4000,10);
            w(o,-0,4000,11);
            out.push(o.length+':'+Array.prototype.join.call(o,',')+':'+o[-1]+o[1.5]+o[16]+o.x+Object.keys(o).length);
          }
          return out.join('|');
        })()"#;
    agree(src, None, true);
}

#[test]
fn detached_buffer_stores_are_ignored() {
    let src = r#"(function(){
          function detach(b){
            try{if(typeof b.transfer==='function')b.transfer();else structuredClone(b,{transfer:[b]})}catch(e){}
          }
          function fill(o,buf,n,at){for(var i=0;i<n;i++){if(i===at)detach(buf);o[i&63]=i;}return o.length}
          function fillv(o,buf,n,at,v){for(var m=0;m<n;m++){if(m===at)detach(buf);o[m&63]=v[m&7]&255;}return o.length}
          var out=[];
          var buf=new ArrayBuffer(64);
          var o=new Uint8Array(buf);
          for(var k=0;k<20;k++)fill(o,buf,500,-1);
          out.push(o.length,o[3]);
          out.push(fill(o,buf,5000,2500),o.length,o[3],o[0]);
          out.push(fill(o,buf,2000,-1),o[5]);
          var b2=new ArrayBuffer(256);
          var f=new Float32Array(b2);
          var vals=[1,2,3,4,5,6,7,8];
          for(var k=0;k<20;k++)fillv(f,b2,500,-1,vals);
          out.push(fillv(f,b2,5000,1000,vals),f.length,f[1]);
          var b3=new ArrayBuffer(64);
          var c=new Uint8ClampedArray(b3);
          for(var k=0;k<20;k++)fill(c,b3,500,-1);
          out.push(fill(c,b3,3000,10),c.length,c[2]);
          return out.join('|');
        })()"#;
    agree(src, None, true);
}

#[test]
fn shared_view_over_resizable_buffer_stores() {
    let src = r#"(function(){
          function fill(o,n){for(var i=0;i<n;i++){o[i&63]=i+1}return o.length}
          var out=[];
          try{
            var rb=new ArrayBuffer(64,{maxByteLength:128});
            var o=new Uint8Array(rb);
            for(var k=0;k<20;k++)fill(o,500);
            out.push(o.length,o[63],o[64]);
            rb.resize(32);
            out.push(fill(o,5000),o.length,o[31],o[32]);
            rb.resize(100);
            out.push(fill(o,5000),o.length,o[63],o[99]);
          }catch(e){out.push('unsupported')}
          return out.join('|');
        })()"#;
    agree(src, None, true);
}

#[test]
fn global_bound_changing_type_mid_loop() {
    let src = r#"(function(){
          globalThis.n=4096;
          var tbl=[];for(var b=0;b<256;b++)tbl[b]=-1;for(var b=0;b<64;b++)tbl[65+b]=b;
          var u=new Uint8Array(4096);for(var i=0;i<4096;i++)u[i]=65+(i*7)%64;
          var src={b:u};
          function t(a,c,at,to){
            var o=new Uint8Array(4096);
            for(var m=0;m<n;m++){if(m===at)n=to;o[m&4095]=c[a.b[m&4095]]&255}
            return o[5]+':'+o[1000]+':'+m
          }
          var out=[];
          for(var k=0;k<30;k++)t(src,tbl,-1,0);
          out.push(t(src,tbl,-1,0));
          out.push(t(src,tbl,1000,'1500'));
          n=4096;
          out.push(t(src,tbl,1000,'abc'));
          n=4096;
          out.push(t(src,tbl,500,2000.5));
          n=4096;
          out.push(t(src,tbl,500,{valueOf:function(){return 700}}));
          n=4096;
          out.push(t(src,tbl,100,undefined));
          n=4096;
          out.push(t(src,tbl,100,null));
          n=4096;
          out.push(t(src,tbl,3000,true));
          return out.join('|');
        })()"#;
    agree(src, None, true);
}

#[test]
fn table_translation_pattern() {
    let src = r#"(function(){
          globalThis.n=1<<14;
          var tbl=[];for(var b=0;b<256;b++)tbl[b]=-1;for(var b=0;b<64;b++)tbl[65+b]=b;
          var u=new Uint8Array(n);for(var i=0;i<n;i++)u[i]=65+(i*7)%64;
          var o={b:u};
          function t4(a,c){var o=new Uint8Array(n);for(var m=0;m<n;m++){o[m]=c[a.b[m]]&255}return o[5]+o[n-1]}
          function t4c(a,c,C){var o=new C(n);for(var m=0;m<n;m++){o[m]=c[a.b[m]]&255}return o[5]+o[n-1]}
          function t4h(a,c){var o=new Uint8Array(n);for(var m=0;m<n;m++){o[m]=c[a.b[m]]}return o[5]+':'+o[n-1]}
          var out=[];
          for(var r=0;r<30;r++)out.push(t4(o,tbl));
          out.push(t4c(o,tbl,Uint8Array),t4c(o,tbl,Uint8ClampedArray),t4c(o,tbl,Int16Array),t4c(o,tbl,Float64Array));
          var holey=[];holey[200]=1;holey[65]=2;
          out.push(t4h({b:new Uint8Array([65,66,200,1,65])},holey));
          out.push(t4({b:[65,66,67,68]},tbl));
          out.push(t4({b:new Uint16Array(n).fill(66)},tbl));
          out.push(t4({b:new Int8Array(n).fill(-3)},tbl));
          out.push(t4(o,tbl));
          return out.slice(25).join('|');
        })()"#;
    agree(src, None, true);
}

#[test]
fn site_fed_arrays_then_typed_arrays_recompiles_typed_first() {
    let src = r#"(function(){
          function sum(a,n){var s=0;for(var i=0;i<n;i++)s+=a.b[i];return s}
          function put(a,n,v){for(var i=0;i<n;i++)a.b[i]=v+i;return a.b[n-1]}
          var arrs=[1,2,3,4,5,6,7,8];
          var out=[];
          for(var k=0;k<400;k++)sum({b:arrs},8);
          out.push(sum({b:arrs},8));
          for(var k=0;k<400;k++)out.length>50||out.push(sum({b:new Uint8Array([1,2,3,250,5,6,7,8])},8));
          out.push(sum({b:new Float64Array([1.5,2,3,4,5,6,7,8])},8));
          out.push(sum({b:new Int16Array([-1,2,3,4,5,6,7,8])},8));
          out.push(sum({b:arrs},8));
          out.push(sum({b:'abcdefgh'},8));
          out.push(sum({b:{0:1,1:2,length:2}},4));
          out.push(sum({b:new Uint8Array(4)},8));
          for(var k=0;k<400;k++)put({b:[0,0,0,0]},4,k);
          for(var k=0;k<400;k++)put({b:new Int8Array(4)},4,k);
          out.push(put({b:new Uint8ClampedArray(4)},4,300),put({b:new Float32Array(4)},4,0.1),put({b:[9,9,9,9]},4,1));
          return out.slice(0,4).concat(out.slice(-9)).join('|');
        })()"#;
    agree(src, None, true);
}

#[test]
fn locals_created_by_new_in_the_prologue() {
    let src = r#"(function(){
          function a(n){var o=new Uint8Array(n);for(var i=0;i<n;i++)o[i]=i*3;var s=0;for(var i=0;i<n;i++)s+=o[i];return s}
          function b(n){var o=new Uint8Array(4);if(n>300)o=new Int16Array(4);for(var i=0;i<n;i++)o[i&3]=i*77;return o[0]+o[1]+o[2]+o[3]}
          function c(n){var o=new Uint8ClampedArray(n);for(var i=0;i<n;i++)o[i]=i*5-100;var s=0;for(var i=0;i<n;i++)s+=o[i];return s}
          function d(n,mk){var o=mk(n);for(var i=0;i<n;i++)o[i]=i*7.5;var s=0;for(var i=0;i<n;i++)s+=o[i];return s}
          var out=[];
          for(var k=0;k<300;k++){a(300);b(400);c(300);d(300,function(n){return new Float32Array(n)})}
          for(var k=0;k<5;k++)out.push(a(100+k*100),b(200+k*100),c(100+k*100));
          out.push(d(50,function(n){return new Int8Array(n)}),d(50,function(n){return[]}),d(50,function(n){return new Float64Array(n)}));
          return out.join('|');
        })()"#;
    agree(src, None, true);
}
