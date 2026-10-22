(function(){
var out=[];
var ctors=[Int8Array,Uint8Array,Uint8ClampedArray,Int16Array,Uint16Array,Int32Array,Uint32Array,Float32Array,Float64Array];
function fmt(v){return typeof v==='number'&&v===0&&1/v<0?'-0':String(v)}
function rd(a,k){return a[k]}
function wr(a,k,v){a[k]=v}
var keys=[0,1,3,4,-1,1.5,NaN,Infinity,-Infinity,-0,2**32,2**32-1,2**53,1e21,0.1];
var vals=[300.7,-5,1.5,2.5,NaN,-129,70000,2**33+3,1.1,Infinity,-0,'7',true,null,undefined,{valueOf:function(){return 42}}];
for(var c=0;c<ctors.length;c++){
  var C=ctors[c],a=new C(4),row=[C.name];
  for(var vi=0;vi<vals.length;vi++){
    for(var r=0;r<40;r++){wr(a,1,vals[vi]);}
    row.push(fmt(rd(a,1)));
  }
  for(var ki=0;ki<keys.length;ki++){
    for(var r=0;r<40;r++){wr(a,keys[ki],9);}
    row.push(fmt(rd(a,keys[ki])));
  }
  a['1.0']=5;a['-0']=6;a['1e1']=8;a['NaN']=3;
  row.push(fmt(a['1.0']),fmt(a['-0']),fmt(a['1e1']),fmt(a['NaN']),Object.keys(a).join(''),a.length,Array.prototype.join.call(a));
  out.push(row.join(' '));
}
var big=[BigInt64Array,BigUint64Array];
for(var c=0;c<2;c++){
  var b=new big[c](3),row=[big[c].name];
  for(var r=0;r<40;r++){wr(b,0,5n);wr(b,1,2n**64n+1n);wr(b,-1,7n);wr(b,3,7n);wr(b,1.5,7n)}
  row.push(String(rd(b,0)),String(rd(b,1)),String(rd(b,2)),String(rd(b,-1)),String(rd(b,3)),String(rd(b,1.5)),String(rd(b,NaN)));
  var e1='',e2='';
  try{b[0]=1}catch(e){e1=e.name}
  try{b[10]=1}catch(e){e2=e.name}
  try{b[-1]='x'}catch(e){e2+=e.name}
  b[0]=-1n;row.push(String(b[0]),e1,e2);
  out.push(row.join(' '));
}
var calls=0,o={valueOf:function(){calls++;return 3}};
var u=new Uint8Array(2);
for(var r=0;r<10;r++){u[5]=o;u[-1]=o;u[0.5]=o;u[0]=o}
out.push('coerce '+calls+' '+u.join());
var buf=new ArrayBuffer(4,{maxByteLength:16}),t=new Uint8Array(buf),f=new Uint8Array(buf,0,4),lt=new Uint8Array(buf,2);
var row=['rs'];
function snap(){return [t.length,f.length,lt.length,fmt(t[5]),fmt(t[3]),fmt(f[3]),fmt(lt[1]),fmt(lt[2])].join(',')}
row.push(snap());
for(var r=0;r<40;r++){t[3]=7;t[5]=8}
buf.resize(8);for(var r=0;r<40;r++){t[5]=8}
row.push(snap(),t[5],t[7]);
buf.resize(2);row.push(snap());
for(var r=0;r<40;r++){t[3]=1;f[3]=1;lt[1]=1}
buf.resize(6);row.push(snap());
var shrink={valueOf:function(){buf.resize(1);return 9}};buf.resize(6);t[4]=shrink;row.push(snap(),buf.byteLength);
out.push(row.join(' '));
var d=new ArrayBuffer(8),da=new Uint8Array(d),row2=['det'];
da[2]=5;for(var r=0;r<40;r++)rd(da,2);
var moved=d.transfer?d.transfer():null;
if(moved){for(var r=0;r<40;r++){da[2]=9;rd(da,2)}row2.push(fmt(da[2]),fmt(da[-1]),da.length,fmt(rd(da,0)));da[0]=1;row2.push(fmt(da[0]),moved.byteLength)}
out.push(row2.join(' '));
var sh=typeof SharedArrayBuffer==='function'?new Int32Array(new SharedArrayBuffer(8)):null;
if(sh){for(var r=0;r<40;r++){wr(sh,1,5);wr(sh,2,5);wr(sh,-1,5)}out.push('sab '+rd(sh,1)+' '+fmt(rd(sh,2))+' '+fmt(rd(sh,-1)))}
var arr=[1,2,3];arr[-1]='n';arr[1.5]='f';
out.push('arr '+arr[-1]+arr[1.5]+arr[5]+arr[NaN]);
var x1=new Uint8Array([1,2,300,-1,4.9]),x2=Uint8Array.from([5,6,7]),x3=new Float32Array([1.1,'2',true,null,undefined]),x4=Uint8Array.from([1,2,3],function(v,k){return v*10+k}),x5=new Uint8ClampedArray([300,-5,1.5,2.5,NaN]),x6=new BigInt64Array([1n,2n]);
var sparse=[1,,3];var x7=new Uint8Array(sparse);
Array.prototype[Symbol.iterator]=function*(){yield 42};
var x8=new Uint8Array([1,2,3]);
delete Array.prototype[Symbol.iterator];
out.push([x1,x2,x3,x4,x5,x6,x7,x8].map(function(x){return Array.prototype.join.call(x)}).join('/'));
function isum(a,n){var s=0;for(var i=0;i<n;i++){s=(s+a[i])|0}return s}
function idiff(a,n){var s=0;for(var i=0;i<n;i++){s=(s-a[i])|0}return s}
function xmix(a,t,n){var c=-1;for(var i=0;i<n;i++){c=(c>>>8)^t[(c^a[i])&255]}return c|0}
function wsum(a,b,n){var s=0;for(var i=0;i<n;i++){s=(a[i]+b[i])|0}return s}
function mix(a,n){var s=0;for(var i=0;i<n;i++){s=(s+(a[i]|0))|0}return s}
var ir=['ints'];
var big8=new Uint8Array(300),bi16=new Int16Array(300),bu16=new Uint16Array(300),bi32=new Int32Array(300),bu32=new Uint32Array(300),bc=new Uint8ClampedArray(300),bf=new Float32Array(300),bd=new Float64Array(300),bi8=new Int8Array(300);
for(var i=0;i<300;i++){big8[i]=i*7;bi16[i]=i*9001-100000;bu16[i]=i*9001;bi32[i]=2147483000+i*3000;bu32[i]=2147483000+i*5000000;bc[i]=i*3-20;bf[i]=i*1.37-9;bd[i]=i*1e9+0.5;bi8[i]=i*5}
for(var r=0;r<30;r++){isum(big8,300);isum(bi16,300);isum(bu16,300);isum(bi32,300);isum(bu32,300);isum(bc,300);isum(bf,300);isum(bd,300);isum(bi8,300)}
[big8,bi16,bu16,bi32,bu32,bc,bf,bd,bi8].forEach(function(a){ir.push(isum(a,300),idiff(a,300),isum(a,305),idiff(a,305),mix(a,300))});
out.push(ir.join(' '));
var tbl=[];for(var i=0;i<256;i++){var c=i;for(var k=0;k<8;k++)c=c&1?(c>>>1)^0xEDB88320:c>>>1;tbl.push(c|0)}
var crcIn=new Uint8Array(500);for(var i=0;i<500;i++)crcIn[i]=i*31+7;
var cr=[];for(var r=0;r<40;r++)cr.push(xmix(crcIn,tbl,500));
out.push('crc '+cr[0]+' '+cr[39]+' '+xmix(crcIn,new Int32Array(tbl),500)+' '+xmix(crcIn,new Float64Array(tbl),500));
var ma=[1,2,3,'4',5.5,-6,2147483647,-2147483648,null,undefined,true];
var poly=[new Uint8Array([200,201,202]),new Int32Array([2147483647,2147483647,2147483647]),[250,251,252],new Float64Array([0.5,1.5,-2.5]),new Uint32Array([4294967295,4294967295,4294967295]),['1','2','3']];
var pr=['poly'];
for(var r=0;r<60;r++){poly.forEach(function(a,k){var v=wsum(a,a,3);if(r>57)pr.push(k+':'+v)})}
out.push(pr.join(' '));
var wr2=['wrap'];
var wa=new Int32Array([2147483647,1,-2147483648,-1]),wb=new Uint32Array([4294967295,1,2147483648,3]);
for(var r=0;r<40;r++){wsum(wa,wb,4)}
wr2.push(wsum(wa,wb,4),wsum(wa,wa,4),wsum(wb,wb,4),wsum(wa,wb,6),wsum(new Uint8Array([255,255]),new Int8Array([-1,-1]),2));
out.push(wr2.join(' '));
var oob=new Uint16Array(4),nz=['oob'];
function oobr(a,i){return a[i]+1}
function oobw(a,i,v){a[i]=v;return a[i]}
for(var r=0;r<60;r++){oobr(oob,1);oobw(oob,2,r)}
nz.push(oobr(oob,4),oobr(oob,-1),oobr(oob,-0),fmt(oob[-0]),oobr(oob,1.5),oobr(oob,'1'),oobr(oob,NaN),oobr(oob,4294967296),oobr(oob,2**31),oobw(oob,4,9),oobw(oob,-0,7),oobw(oob,'2',70000),oobw(oob,1.5,3),oob.join());
out.push(nz.join(' '));
var fb=new Float32Array(3),f64=new Float64Array(3),clamp=new Uint8ClampedArray(6),conv=['conv'];
function st(a,i,v){a[i]=v;return a[i]}
for(var r=0;r<60;r++){st(fb,0,r);st(f64,0,r);st(clamp,0,r)}
[16777217,1e40,-1e40,NaN,-0,0.1,'3.7',null,undefined,true,{valueOf:function(){return 5}}].forEach(function(v){conv.push(fmt(st(fb,1,v)),fmt(st(f64,1,v)),fmt(st(clamp,1,v)),fmt(st(clamp,2,v)))});
[0.5,1.5,2.5,254.5,255.5,-0.5,1e9,Infinity,-Infinity].forEach(function(v){conv.push(st(clamp,3,v))});
out.push(conv.join(' '));
var bg=new BigInt64Array(4),bu=new BigUint64Array(4),bgr=['big'];
function bst(a,i,v){a[i]=v;return a[i]}
for(var r=0;r<60;r++){bst(bg,0,BigInt(r));bst(bu,0,BigInt(r))}
bgr.push(String(bst(bg,1,-1n)),String(bst(bu,1,-1n)),String(bst(bg,2,2n**63n)),String(bst(bu,2,2n**64n+5n)),String(bg[9]),String(bg[-1]));
try{bst(bg,0,1)}catch(e){bgr.push(e.constructor.name)}
try{bst(new Uint8Array(2),0,1n)}catch(e){bgr.push(e.constructor.name)}
out.push(bgr.join(' '));
var rb=new ArrayBuffer(8,{maxByteLength:32}),rv=new Int16Array(rb),rf=new Int16Array(rb,0,2),rr=['rz'];
function rsum(a){var s=0;for(var i=0;i<a.length;i++){s=(s+a[i])|0}return s}
function rg(a,i){return a[i]}
for(var i=0;i<4;i++)rv[i]=i+1;
for(var r=0;r<60;r++){rsum(rv);rg(rv,3);rg(rf,1)}
rr.push(rsum(rv),rg(rv,3),rg(rf,1));
rb.resize(16);rr.push(rsum(rv),rg(rv,6),rg(rv,7),rg(rv,8),rg(rf,1));
rb.resize(2);rr.push(rsum(rv),rg(rv,1),rg(rv,3),rg(rf,1),rf.length);
rb.resize(0);rr.push(rsum(rv),rg(rv,0),rf.length);
out.push(rr.join(' '));
var dt=new ArrayBuffer(16),dv=new Int32Array(dt),dr=['dt2'];
for(var i=0;i<4;i++)dv[i]=i*100000;
for(var r=0;r<60;r++){isum(dv,4);rg(dv,2)}
if(dt.transfer){dt.transfer();dr.push(isum(dv,4),rg(dv,0),rg(dv,2),dv.length,st(dv,0,5),dv.byteLength)}
out.push(dr.join(' '));
return out.join('\n');
})()
